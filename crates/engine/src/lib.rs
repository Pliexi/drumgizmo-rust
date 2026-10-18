// SPDX-License-Identifier: GPL-3.0-or-later
//! Drum sampler engine: MIDI → instruments, chokes, voice management,
//! multichannel mixing, sequencer integration and the kit lifecycle.
//!
//! Threading model (same idea as upstream):
//! * the engine runs on the realtime audio thread and owns all state,
//! * control changes arrive through a lock-free `rtrb` ring buffer
//!   (`RtCommand`), drained at block start (never blocking),
//! * kit loading happens on a background thread (`KitLoader`); the engine
//!   polls the result each block and swaps the active kit, the previous
//!   kit stays alive until no reference is held,
//! * no mutexes and no allocation in the render path.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use dg_common::rng::Rand;
use dg_common::{MessageType, MidiEvent, RtCommand, Sample, NO_CHANNEL};
use dg_kit::dom::MainState;
use dg_kit::parser::MidimapEntry;
use dg_kit::{KitLoader, LoadedKit};
use dg_sampler::{
    ramp_frames, SampleBank, SelectionStrategy, VoiceEvent, VoiceManager, CHOKE_EVENT_RAMPDOWN_MS,
    GROUP_RAMPDOWN_MS, VOICE_LIMIT_MAX_DEFAULT, VOICE_LIMIT_RAMPDOWN_DEFAULT_MS,
};
use dg_sequencer::{ChainEntry, Pattern, Sequencer, StepEventKind};
use rtrb::Consumer;

/// Deterministic FNV-1a hash of a group name ("": no group → 0).
fn group_hash(name: &str) -> u32 {
    if name.is_empty() {
        return 0;
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash | 1 // never zero: zero means "no group"
}

/// Snapshot of the pattern bank transported over the command ring.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SeqSnapshot {
    pub patterns: Vec<Pattern>,
    pub chain: Vec<ChainEntry>,
    pub loop_chain: bool,
}

/// Read-only realtime mirror for the GUI: the transport position and
/// kit-availability state. The engine updates the atomics inside its
/// render blocks; hosts poll them, no locks on the audio path.
#[derive(Debug, Default)]
pub struct EngineWatch {
    /// Step inside the currently scheduled pattern (wraps at length).
    pub step_index: AtomicU64,
    /// True while the sequencer transport runs.
    pub playing: AtomicBool,
    /// Kit generation of the last successful install (0 = none yet).
    pub kit_generation: AtomicU64,
    /// Set when a kit load failed; details stay on stderr.
    pub kit_error: AtomicBool,
}

/// The realtime engine: consumes commands + MIDI + sequencer output and
/// fills the output buses.
pub struct Engine {
    samplerate: u64,
    /// Absolute output frame counter (never resets while running).
    frames_played: u64,
    pub kit: Option<Arc<LoadedKit>>,
    selection: Vec<SelectionStrategy>,
    /// MIDI note → kit instruments of the current kit.
    midi_map: Vec<Vec<usize>>,
    /// Instrument id → choke group hash (0 = none).
    group_ids: Vec<u32>,
    /// Directed chokes of the current kit: (target instrument, choketime ms).
    directed_chokes: Vec<(usize, f64)>,
    voices: VoiceManager,
    random: Rand,
    /// Last selected sample per instrument (GUI feedback).
    last_samples: Vec<usize>,

    // --- settings (live-changeable like upstream) ---
    master_gain: f32,
    voice_gains: Vec<f32>,
    normalized_samples: bool,
    bleed_control: bool,
    master_bleed: f32,
    enable_voice_limit: bool,
    voice_limit_max: usize,
    voice_limit_rampdown_ms: f64,

    // --- integration ---
    sequencer: Sequencer,
    sequencer_active: bool,
    loader: Option<KitLoader>,
    commands: Consumer<RtCommand>,
    /// Block relative MIDI events, sorted before processing.
    pending: Vec<MidiEvent>,
    /// Per kit channel rendering scratch (stereo mixdown source).
    scratch: Vec<Vec<Sample>>,
    /// Main channel stereo mapping.
    stereo_mix: StereoMix,
    /// Bumped on every kit install; guards the stereo scratch ownership
    /// against mid-block kit loads.
    kit_generation: u64,
    /// Read-only realtime mirror for hosts (step, transport, kit state).
    watch: Arc<EngineWatch>,
}

/// Which kit channels to sum into the stereo output mix (main channels), or
/// the fallback channel pair [0, 1] when nothing is marked main.
#[derive(Debug, Clone, PartialEq)]
enum StereoMix {
    /// Explicit main kit channels (summed into L+R).
    Main(Vec<usize>),
    /// Fallback when no main channel exists: (left, right).
    Pair(usize, usize),
}

impl Default for StereoMix {
    fn default() -> Self {
        Self::Pair(0, 1)
    }
}

/// Creates the engine control pair: the engine (rt side) and the command
/// producer for the UI/host thread.
pub fn engine_pair(
    samplerate: u64,
    command_capacity: usize,
) -> (Engine, rtrb::Producer<RtCommand>) {
    let (producer, consumer) = rtrb::RingBuffer::new(command_capacity.max(16));
    let engine = Engine::new(samplerate, consumer);
    (engine, producer)
}

impl Engine {
    pub fn new(samplerate: u64, commands: Consumer<RtCommand>) -> Self {
        let sequencer = Sequencer::new(samplerate, 0xD3AD_B0BA_EE57);
        Self {
            samplerate,
            frames_played: 0,
            kit: None,
            selection: Vec::new(),
            midi_map: vec![Vec::new(); 128],
            group_ids: Vec::new(),
            directed_chokes: Vec::new(),
            voices: VoiceManager::new(),
            random: Rand::new(0x1FAD_DCAD_9897),
            last_samples: Vec::new(),
            master_gain: 1.0,
            voice_gains: Vec::new(),
            normalized_samples: true,
            bleed_control: false,
            master_bleed: 0.5,
            enable_voice_limit: false,
            voice_limit_max: VOICE_LIMIT_MAX_DEFAULT,
            voice_limit_rampdown_ms: VOICE_LIMIT_RAMPDOWN_DEFAULT_MS,
            sequencer,
            sequencer_active: false,
            loader: None,
            commands,
            pending: Vec::new(),
            scratch: Vec::new(),
            stereo_mix: StereoMix::default(),
            kit_generation: 0,
            watch: Arc::new(EngineWatch::default()),
        }
    }

    /// Handle to the realtime mirror (transport position, kit presence).
    /// Gracefully shared with hosts after the engine moved elsewhere.
    pub fn watch(&self) -> Arc<EngineWatch> {
        Arc::clone(&self.watch)
    }

    /// Feed one MIDI event that belongs to the **next** rendered block
    /// (`offset` is block relative in frames). Drained by
    /// [`Engine::render_block`].
    pub fn push_midi(&mut self, event: MidiEvent) {
        self.pending.push(event);
    }

    /// Engine sample rate.
    pub fn samplerate(&self) -> u64 {
        self.samplerate
    }

    /// Sequencer access (patterns/chain/tempo); polled while active.
    pub fn sequencer_mut(&mut self) -> &mut Sequencer {
        &mut self.sequencer
    }

    /// Sequencer activation (transport).
    pub fn set_sequencer_active(&mut self, active: bool) {
        self.sequencer_active = active;
    }

    /// Last selected sample per instrument (GUI feedback).
    pub fn last_selected_samples(&self) -> &[usize] {
        &self.last_samples
    }

    /// Voice pool load 0..1 (GUI feedback).
    pub fn voice_load(&self) -> f32 {
        self.voices.load()
    }

    /// Master output volume.
    pub fn set_master_gain(&mut self, gain: f32) {
        self.master_gain = gain.clamp(0.0, 8.0);
    }

    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }

    /// Per instrument volume multiplier.
    pub fn set_voice_gain(&mut self, instrument: usize, gain: f32) {
        if instrument == usize::MAX {
            self.voice_gains.fill(gain.clamp(0.0, 8.0));
        } else if let Some(g) = self.voice_gains.get_mut(instrument) {
            *g = gain.clamp(0.0, 8.0);
        }
    }

    /// Upstream `normalized_samples` setting (velocity scales the voice).
    pub fn set_normalized_samples(&mut self, normalized: bool) {
        self.normalized_samples = normalized;
    }

    /// Upstream bleed control: non-main channels at `master_bleed` gain.
    pub fn set_bleed_control(&mut self, enabled: bool, master_bleed: f32) {
        self.bleed_control = enabled;
        self.master_bleed = master_bleed.clamp(0.0, 1.0);
    }

    /// Upstream voice limiting of per-instrument voices.
    pub fn set_voice_limit(&mut self, enabled: bool, max: usize, rampdown_ms: f64) {
        self.enable_voice_limit = enabled;
        self.voice_limit_max = max.max(1);
        self.voice_limit_rampdown_ms = rampdown_ms.max(0.01);
    }

    // -- kit lifecycle ------------------------------------------------------

    /// Replaces the kit; `midimap` resolves MIDI notes (None = no triggers,
    /// upstream shows the kit but cannot sound without a midimap).
    pub fn set_kit(&mut self, kit: Arc<LoadedKit>, midimap: Option<&[MidimapEntry]>) {
        self.selection = kit
            .instruments
            .iter()
            .map(SelectionStrategy::for_instrument)
            .collect();
        self.midi_map = match midimap {
            Some(entries) => kit.build_midi_map(entries),
            None => vec![Vec::new(); 128],
        };
        self.group_ids = kit
            .instruments
            .iter()
            .map(|i| group_hash(&i.group))
            .collect();
        self.directed_chokes = kit
            .instruments
            .iter()
            .flat_map(|i| i.chokes.iter().map(|c| (c.instrument_id, c.choketime_ms)))
            .collect();
        self.voice_gains = vec![1.0; kit.instruments.len()];
        self.last_samples = vec![usize::MAX; kit.instruments.len()];
        self.scratch = vec![Vec::new(); kit.channels.len()];

        // Stereo mixdown: sum every kit channel that a main instrument
        // channel maps to; without any main mapping use the first pair.
        let mut mains: Vec<usize> = kit
            .instruments
            .iter()
            .flat_map(|i| i.channels.iter())
            .filter(|c| matches!(c.main, MainState::IsMain))
            .map(|c| c.kit_channel)
            .filter(|c| *c < NO_CHANNEL)
            .collect();
        mains.sort_unstable();
        mains.dedup();
        self.stereo_mix = if mains.is_empty() {
            StereoMix::Pair(0, 1)
        } else {
            StereoMix::Main(mains)
        };

        self.kit = Some(kit);
        self.kit_generation += 1;
        self.watch
            .kit_generation
            .store(self.kit_generation, Ordering::Release);
        self.watch.kit_error.store(false, Ordering::Release);
    }

    /// Starts an asynchronous kit load; on completion the engine parses
    /// `midimap.xml` inside the kit directory when present.
    pub fn load_kit(&mut self, path: &str) {
        self.loader = Some(KitLoader::start(path, self.samplerate as u32));
    }

    /// Polls a finished load once per block; non-blocking.
    fn take_loader_result(&mut self) {
        let Some(loader) = self.loader.as_mut() else {
            return;
        };
        match loader.poll() {
            None => {} // still loading
            Some(Ok(kit)) => {
                self.loader = None;
                let entries = parse_midimap_of(&kit).unwrap_or_default();
                self.set_kit(kit, Some(&entries));
            }
            Some(Err(e)) => {
                self.loader = None;
                self.watch.kit_error.store(true, Ordering::Release);
                eprintln!("kit load failed: {e:?}");
            }
        }
    }

    // -- commands ------------------------------------------------------------

    /// Drains the command ring buffer at block start.
    fn drive_commands(&mut self) {
        while let Ok(command) = self.commands.pop() {
            self.apply_command(command);
        }
    }

    fn apply_command(&mut self, command: RtCommand) {
        match command {
            RtCommand::Midi(event) => self.push_midi(event),
            RtCommand::LoadKit { path } => self.load_kit(&path),
            RtCommand::MasterGain { gain } => self.set_master_gain(gain),
            RtCommand::VoiceGain { instrument, gain } => self.set_voice_gain(instrument, gain),
            RtCommand::Panic => self.panic(),
            RtCommand::SeqStart => {
                self.sequencer_active = true;
                if !self.sequencer.is_playing() {
                    self.sequencer.start(0.0);
                }
                self.watch.playing.store(true, Ordering::Release);
            }
            RtCommand::SeqStop => {
                self.sequencer_active = false;
                self.sequencer.stop();
                self.watch.playing.store(false, Ordering::Release);
                self.watch.step_index.store(0, Ordering::Release);
            }
            RtCommand::SeqTempo { bpm } => {
                // Explicit tempo commands (GUI drag, plugin tempo
                // parameter) target the internal clock: hosts that
                // drive sample positions per block never send this.
                // Previously `host_tempo` was stored while
                // `host_sync == false` left `tempo()` reading the
                // untouched internal value (erratum now fixed).
                self.sequencer.host_sync = false;
                self.sequencer.host_tempo = Some(bpm);
                if bpm.is_finite() && bpm > 0.0 {
                    self.sequencer.internal_tempo = bpm;
                }
            }
            RtCommand::SeqPattern { json } => match serde_json::from_str::<SeqSnapshot>(&json) {
                Ok(snapshot) => {
                    self.sequencer.patterns = snapshot.patterns;
                    self.sequencer.chain = snapshot.chain;
                    self.sequencer.loop_chain = snapshot.loop_chain;
                    self.sequencer.stop();
                }
                Err(e) => eprintln!("sequencer pattern rejected: {e}"),
            },
        }
    }

    /// All voices fade out quickly; a panic distinct from∞ choke semantics.
    pub fn panic(&mut self) {
        let ramp = ramp_frames(10.0, self.samplerate);
        for voice in &mut self.voices.voices {
            if voice.active && !voice.rampdown_in_progress() {
                arm_soft_ramp(voice, ramp);
            }
        }
    }

    // -- input pipeline -------------------------------------------------------

    /// Upstream `InputProcessor::processOnset` port for one instrument.
    fn process_hit(&mut self, instrument: usize, velocity: f32, offset: usize) {
        if self.kit.is_none() || self.selection.get(instrument).is_none() {
            return;
        }
        let delay = offset as f64;
        let group_id = self.group_ids.get(instrument).copied().unwrap_or(0);

        // 1. choke group of the instrument (excluding itself), 68 ms
        self.voices.apply_choke_group(
            group_id,
            instrument,
            ramp_frames(GROUP_RAMPDOWN_MS, self.samplerate),
            delay,
        );

        // 2. directed chokes of the instrument (per choketime)
        for (target, choketime_ms) in self.directed_chokes.iter() {
            if *target != instrument {
                self.voices.apply_choke_instrument(
                    *target,
                    ramp_frames(*choketime_ms, self.samplerate),
                    delay,
                );
            }
        }

        // 3. sample selection (level = power_min + velocity * power_span)
        let pos = self.frames_played + offset as u64;
        let Some(sample_index) = self.selection[instrument].sample_index(
            velocity,
            pos,
            self.samplerate,
            &mut self.random,
        ) else {
            return;
        };
        if let Some(cell) = self.last_samples.get_mut(instrument) {
            *cell = sample_index;
        }

        // 4. voice limit (off by default, like upstream)
        if self.enable_voice_limit {
            self.voices.limit_voices(
                instrument,
                self.voice_limit_max,
                self.voice_limit_rampdown_ms,
                self.samplerate,
            );
        }

        // 5. one voice per (file, filechannel → kit channel) mapping
        let Some(kit) = &self.kit else { return };
        let Some(instrument_kit) = kit.instruments.get(instrument) else {
            return;
        };
        let Some(sample) = instrument_kit.samples.get(sample_index) else {
            return;
        };
        let voice_gain = self.voice_gains.get(instrument).copied().unwrap_or(1.0);

        for mapping in &sample.audiofiles {
            if mapping.instrument_channel == NO_CHANNEL {
                continue; // unmapped kit channel
            }
            let normalized_scale = if self.normalized_samples && sample.normalized {
                velocity
            } else {
                1.0
            };
            let bleed_scale = if self.bleed_control {
                let main = instrument_kit
                    .channels
                    .iter()
                    .find(|c| c.kit_channel == mapping.instrument_channel)
                    .map(|c| matches!(c.main, MainState::IsMain))
                    .unwrap_or(true);
                if main {
                    1.0
                } else {
                    self.master_bleed
                }
            } else {
                1.0
            };
            let event = VoiceEvent {
                instrument_id: instrument,
                sample_index,
                file_index: mapping.file_index,
                filechannel: mapping.filechannel,
                kit_channel: mapping.instrument_channel,
                offset: offset as f64,
                scale: voice_gain * normalized_scale * bleed_scale,
                group_id,
            };
            let frames = kit.channel(mapping.file_index, mapping.filechannel).len() as u64;
            // Files decode at the engine rate already; step == 1.0.
            self.voices.spawn(&event, frames, 1.0);
        }
    }

    /// Upstream `processChoke` port: ramps the instrument's voices down.
    fn process_choke(&mut self, instrument: usize, offset: usize) {
        self.voices.apply_choke_instrument(
            instrument,
            ramp_frames(CHOKE_EVENT_RAMPDOWN_MS, self.samplerate),
            offset as f64,
        );
    }

    /// True while neither voices nor the sequencer make sound.
    pub fn is_silent(&self) -> bool {
        self.voices.active_count() == 0 && !self.sequencer_active
    }

    /// Sequencer events inside the upcoming block become engine requests.
    fn poll_sequencer(&mut self, block_size: usize) -> bool {
        if !self.sequencer_active || !self.sequencer.is_playing() {
            self.watch.playing.store(false, Ordering::Release);
            self.watch.step_index.store(0, Ordering::Release);
            return false;
        }
        self.watch
            .step_index
            .store(self.sequencer.position_step() as u64, Ordering::Release);
        let until = self.frames_played + block_size as u64;
        let events = self.sequencer.poll(until);
        let any = !events.is_empty();
        for event in events {
            let relative = event.frame.saturating_sub(self.frames_played);
            let offset = relative.clamp(0, block_size.max(1) as u64 - 1) as usize;
            match event.kind {
                StepEventKind::Hit => self.process_hit(event.instrument_id, event.velocity, offset),
                StepEventKind::Choke => self.process_choke(event.instrument_id, offset),
            }
        }
        any
    }

    // -- render -----------------------------------------------------------------

    /// Renders one block. `out` holds one buffer per rendered kit channel
    /// (usual: stereo pair). Returns the count of consumed event groups
    /// (diagnostics). The buffers are cleared first.
    pub fn render_block(&mut self, out: &mut [&mut [Sample]], midi: &[MidiEvent]) -> usize {
        let block_size = out.first().map(|c| c.len()).unwrap_or(0);
        for channel in out.iter_mut() {
            channel.fill(0.0);
        }
        let mut events = 0;

        self.drive_commands();
        self.take_loader_result();

        // 1. MIDI events of this block (voice_limit ramp independent). The
        //      count clamped to the block keeps a voice from skipping the
        //      render frame entirely.
        self.pending.extend_from_slice(midi);
        self.pending.sort_by_key(|e| e.offset);
        let midi_events: Vec<MidiEvent> = std::mem::take(&mut self.pending);
        for event in midi_events {
            events += 1;
            let note = event.note as usize;
            let offset = event.offset.min(block_size.max(1) - 1);
            match event.kind {
                MessageType::OnSet => {
                    if event.velocity != 0.0 {
                        // upstream maps velocities to [.5/127, 126.5/127]
                        let centered = (event.velocity - 0.5).max(0.0) / 127.0;
                        let instruments = self.midi_map[note.min(127)].clone();
                        for instrument in instruments {
                            self.process_hit(instrument, centered, offset);
                        }
                    }
                }
                MessageType::NoteOff => {
                    // ignored, like upstream
                }
                MessageType::Choke => {
                    if event.velocity > 0.0 {
                        let instruments = self.midi_map[note.min(127)].clone();
                        for instrument in instruments {
                            self.process_choke(instrument, offset);
                        }
                    }
                }
            }
        }

        // 2. sequencer
        if self.poll_sequencer(block_size) {
            events += 1;
        }

        // 3. voice rendering into the kit channels
        if !out.is_empty() {
            let bank: Option<&dyn SampleBank> = self.kit.as_deref().map(|k| k as &dyn SampleBank);
            if let Some(bank) = bank {
                self.voices.render(bank, out);
            }
        }

        // 4. master gain
        let master = self.master_gain;
        if master != 1.0 {
            for channel in out.iter_mut() {
                for sample in channel.iter_mut() {
                    *sample *= master;
                }
            }
        }

        self.frames_played += block_size as u64;
        events
    }

    /// Stereo convenience path for hosts with fixed stereo outputs:
    /// renders into the internal per-kit-channel scratch buffers and mixes
    /// down following docs/decisions.md (every `main` kit channel sums
    /// into L and R; without any main mapping channel 0 → left,
    /// channel 1 → right). Returns the count of consumed event groups
    /// like [`Engine::render_block`].
    pub fn render_stereo(
        &mut self,
        left: &mut [Sample],
        right: &mut [Sample],
        midi: &[MidiEvent],
    ) -> usize {
        let block_size = left.len().min(right.len()).max(1);
        // Temporarily own the scratch buffers and the stereo mapping so
        // that `render_block` can borrow `self` while the per-channel
        // views borrow the local vecs. `take` is a plain move, no
        // allocation on the audio path.
        let generation = self.kit_generation;
        let mut scratch = std::mem::take(&mut self.scratch);
        let mix = std::mem::take(&mut self.stereo_mix);
        for buffer in scratch.iter_mut() {
            let len = buffer.len();
            if len < block_size {
                // Rare: kit swap between our calls before this or the
                // block size changed (host driven).
                buffer.resize(block_size, 0.0);
            }
            buffer[..block_size.min(len)].fill(0.0);
        }
        let mut views: Vec<&mut [Sample]> = scratch.iter_mut().map(|b| b.as_mut_slice()).collect();
        let events = self.render_block(&mut views, midi);
        drop(views);

        left[..block_size].fill(0.0);
        right[..block_size].fill(0.0);
        match &mix {
            StereoMix::Main(mains) => {
                for channel in mains {
                    if let Some(buffer) = scratch.get(*channel) {
                        for frame in 0..block_size {
                            let value = buffer[frame];
                            left[frame] += value;
                            right[frame] += value;
                        }
                    }
                }
            }
            StereoMix::Pair(l_channel, r_channel) => {
                if let Some(buffer) = scratch.get(*l_channel) {
                    for frame in 0..block_size {
                        left[frame] += buffer[frame];
                    }
                }
                if let Some(buffer) = scratch.get(*r_channel) {
                    for frame in 0..block_size {
                        right[frame] += buffer[frame];
                    }
                }
            }
        }
        if generation == self.kit_generation {
            // A mid-block kit load (take_loader_result) installs fresh
            // scratch buffers and a fresh mapping for the new kit; keep
            // those instead of the stale ones in the locals.
            self.scratch = scratch;
            self.stereo_mix = mix;
        }
        events
    }
}

/// Arms a quick fade used by the panic command.
fn arm_soft_ramp(voice: &mut dg_sampler::Voice, ramp: u64) {
    let ramp = ramp.max(1);
    voice.ramp_length = ramp;
    voice.rampdown_start = voice.position;
    voice.rampdown_count = ramp as i64;
}

fn parse_midimap_of(kit: &LoadedKit) -> Option<Vec<MidimapEntry>> {
    let path = kit.basepath.join("midimap.xml");
    dg_kit::parser::parse_midimap_file(&path).ok()
}
