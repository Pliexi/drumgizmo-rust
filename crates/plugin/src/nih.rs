// SPDX-License-Identifier: GPL-3.0-or-later
//! nice-plug plugin implementation shared by the CLAP and VST3 export
//! crates: the full egui editor (kit file chooser, sequencer, inspector)
//! embedded through the same lock-free command ring the standalone
//! uses, with all state persisted through host save/restore.
//!
//! Threading: `process()` and `activate()` touch the `PluginCore` behind a
//! mutex, but only ever on the audio thread (nice-plug guarantees both run
//! there), so the lock stays uncontended. The editor thread never touches
//! the core; it shares the command producer ring instead.
//!
//! State model: the sampler needs no host-automatable parameters, so the
//! host sees four persistent plain fields (kit path, pattern JSON, master
//! gain, sequencer tempo). Master gain/tempo are edited in the GUI just
//! like the standalone; tempo follows the host transport while it plays.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use nice_plug::context::gui::GuiContext;
use nice_plug::editor::dpi::LogicalSize;
use nice_plug::prelude::*;
use nice_plug_egui::{
    baseview, create_egui_editor, EguiEditor, EguiEditorState, EguiNiceSettings, Frame,
    NiceEguiApp, RepaintNotifier,
};

use dg_common::{MessageType, MidiEvent};
use dg_sequencer::Pattern;

use crate::core::PluginCore;

// ---------------------------------------------------------------------
// persisted state (parameters)
// ---------------------------------------------------------------------

/// Persistent fields the host stores in its session. There are no
/// automatable parameters: the engine's gain and tempo live behind the
/// GUI/ring, and a drum sampler's automation is normally done with the
/// host track fader anyway (see run_guide.md).
#[derive(Params)]
pub struct DrumParams {
    /// The kit directory; replayed into the engine on activation.
    #[persist = "kit_path"]
    pub kit_path: Arc<Mutex<String>>,
    /// Serialized [`Pattern`]; replayed on activation.
    #[persist = "pattern_json"]
    pub pattern_json: Arc<Mutex<String>>,
    /// Master output gain 0.0..=8.0 (multiplier).
    #[persist = "master_gain"]
    pub master_gain: Arc<Mutex<f32>>,
    /// Sequencer tempo in BPM.
    #[persist = "seq_tempo"]
    pub tempo: Arc<Mutex<f64>>,
}

impl Default for DrumParams {
    fn default() -> Self {
        Self {
            kit_path: Arc::new(Mutex::new(String::new())),
            pattern_json: Arc::new(Mutex::new(String::new())),
            master_gain: Arc::new(Mutex::new(1.0)),
            tempo: Arc::new(Mutex::new(120.0)),
        }
    }
}

impl DrumParams {
    /// Applies the persisted fields into the core before its state
    /// replay (activation).
    fn apply_persisted(&self, core: &mut PluginCore) {
        let kit_path = self.kit_path.lock().unwrap().clone();
        if !kit_path.is_empty() {
            core.set_kit(&kit_path);
        }
        let pattern_json = self.pattern_json.lock().unwrap().clone();
        if !pattern_json.is_empty() {
            if let Ok(pattern) = serde_json::from_str::<Pattern>(&pattern_json) {
                core.set_pattern(pattern);
            }
        }
        let master_gain = *self.master_gain.lock().unwrap();
        core.master_gain(master_gain);
        let tempo = *self.tempo.lock().unwrap();
        core.set_tempo(tempo);
    }
}

impl ClapPlugin for DrumPlugin {
    const CLAP_ID: &'static str = "com.pliexi.drumgizmo-rust";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("DrumGizmo re-implementation in Rust (kit sampler + sequencer)");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> =
        Some("https://github.com/Pliexi/drumgizmo-rust/issues");
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Sampler];
}

impl Vst3Plugin for DrumPlugin {
    // Opaque but stable identifier for the combined component/controller
    // class nice-plug exports (changed from the v0.1.x hand-written
    // wrapper, which had separate CIDs — see docs/decisions.md).
    const VST3_CLASS_ID: [u8; 16] = *b"drumgizmo-rustV3";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Instrument,
        Vst3SubCategory::Drum,
        Vst3SubCategory::Sampler,
    ];
}

// ---------------------------------------------------------------------
// editor shell
// ---------------------------------------------------------------------

/// The nice-plug-egui app wrapping a full [`dg_gui::DrumApp`] pass.
pub struct DrumEditorApp {
    app: dg_gui::DrumApp,
    /// The pattern JSON last mirrored into the persisted field.
    last_pattern_json: String,
    /// The master gain last mirrored into the persisted field.
    last_master_gain: f32,
    /// The tempo last mirrored into the persisted field.
    last_tempo: f64,
    /// Persist handle for the mirror step.
    params: Arc<DrumParams>,
}

impl NiceEguiApp for DrumEditorApp {
    fn build(
        &mut self,
        egui_ctx: egui::Context,
        _nice_gui_ctx: GuiContext,
        _frame: &mut Frame,
    ) -> Result<(), baseview::HandlerError> {
        dg_gui::DrumApp::configure_theme_on(&egui_ctx);
        Ok(())
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        let ctx = ui.ctx().clone();
        self.app.handle_frame(&ctx);
        self.app.draw_ui(ui);

        // Mirror the editor state into the persisted fields. The kit
        // path follows the entry text (the user's latest intent), gain
        // and tempo follow the app's sliders, and the pattern persists
        // whenever it changed — hosts may save between any of these.
        let snapshot = self.app.editor_snapshot();
        *self.params.kit_path.lock().unwrap() = snapshot.kit_path_entry;

        let master_gain = snapshot.master_gain;
        if (master_gain - self.last_master_gain).abs() > f32::EPSILON {
            self.last_master_gain = master_gain;
            *self.params.master_gain.lock().unwrap() = master_gain;
        }

        let tempo = snapshot.tempo;
        if (tempo - self.last_tempo).abs() > f64::EPSILON {
            self.last_tempo = tempo;
            *self.params.tempo.lock().unwrap() = tempo;
        }

        if let Ok(json) = serde_json::to_string(&snapshot.pattern) {
            if json != self.last_pattern_json {
                self.last_pattern_json = json.clone();
                *self.params.pattern_json.lock().unwrap() = json;
            }
        }
    }
}

// ---------------------------------------------------------------------
// the plugin
// ---------------------------------------------------------------------

/// The drumgizmo-rust instrument plugin (nice-plug; exported for both
/// CLAP and VST3 from the wrapper crates).
pub struct DrumPlugin {
    params: Arc<DrumParams>,
    /// Realtime engine plumbing; audio-thread only.
    core: Mutex<PluginCore>,
    /// Editor window state (size/zoom survive open/close).
    egui_state: Arc<EguiEditorState>,
    repaint_notifier: RepaintNotifier,
    /// Event scratch reused across blocks (allocated once).
    midi_scratch: Vec<MidiEvent>,
    /// Mono-layout render scratch; sized on activate.
    scratch_left: Vec<f32>,
    scratch_right: Vec<f32>,
    /// The pattern JSON last pushed into the engine (dedupe per block).
    last_pattern_json: Mutex<Option<String>>,
}

impl Default for DrumPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(DrumParams::default()),
            core: Mutex::new(PluginCore::default()),
            egui_state: EguiEditorState::from_size(LogicalSize::new(900.0, 640.0), 1.0),
            repaint_notifier: RepaintNotifier::default(),
            midi_scratch: Vec::new(),
            scratch_left: Vec::new(),
            scratch_right: Vec::new(),
            last_pattern_json: Mutex::new(None),
        }
    }
}

impl Plugin for DrumPlugin {
    const NAME: &'static str = "DrumGizmo Rust";
    const VENDOR: &'static str = "drumgizmo-rust project";
    const URL: &'static str = "https://github.com/Pliexi/drumgizmo-rust";
    const EMAIL: &'static str = "milyukovegor2009@gmail.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;
    const HARD_REALTIME_ONLY: bool = false;

    type Editor = EguiEditor<DrumEditorApp>;
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
        let (kit_path, master_gain, tempo, pattern, producer) = {
            let core = self.core.lock().unwrap();
            let state = core.state();
            (
                state.kit_path.clone(),
                state.master_gain,
                state.tempo,
                state.pattern.clone(),
                core.shared_producer(),
            )
        };
        let app = dg_gui::DrumApp::new_for_host(
            producer,
            kit_path,
            master_gain,
            tempo,
            pattern.unwrap_or_default(),
        );
        create_egui_editor(
            Arc::clone(&self.egui_state),
            self.repaint_notifier.clone(),
            EguiNiceSettings {
                title: "DrumGizmo Rust".to_string(),
                ..EguiNiceSettings::default()
            },
            DrumEditorApp {
                app,
                last_pattern_json: String::new(),
                last_master_gain: master_gain,
                last_tempo: tempo,
                params: Arc::clone(&self.params),
            },
        )
    }

    fn activate(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl ActivateContext<Self>,
    ) -> bool {
        let mut core = self.core.lock().unwrap();
        // Restore the persisted state into the core first, so the
        // samplerate swap below replays it into the fresh engine.
        self.params.apply_persisted(&mut core);
        core.set_samplerate(buffer_config.sample_rate.round() as u32);

        // Seed the pattern dedupe with the restored state to avoid
        // re-pushing the same pattern on the next block.
        if let Ok(json) = serde_json::to_string(&core.state().pattern) {
            *self.last_pattern_json.lock().unwrap() = Some(json);
        }

        let max_block = buffer_config.max_buffer_size.max(1) as usize;
        self.scratch_left.clear();
        self.scratch_right.clear();
        self.scratch_left.resize(max_block, 0.0);
        self.scratch_right.resize(max_block, 0.0);
        drop(core);
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.sync_engine_state(context);

        // Drain host note events into the engine's MIDI dialect,
        // ordered by their offset within the block.
        self.midi_scratch.clear();
        while let Some(event) = context.next_event() {
            if let Some(midi) = to_midi_event(&event) {
                self.midi_scratch.push(midi);
            }
        }
        self.midi_scratch
            .sort_by_key(|event: &MidiEvent| event.offset);

        let output = buffer.as_slice();
        {
            let mut core = self.core.lock().unwrap();
            match output.len() {
                0 => {}
                1 => {
                    // Uncommon mono layout: render into scratch, then
                    // fold the stereo pair down to one channel.
                    let frames = output[0].len();
                    if self.scratch_left.len() < frames {
                        self.scratch_left.resize(frames, 0.0);
                        self.scratch_right.resize(frames, 0.0);
                    }
                    core.process_stereo(
                        &mut self.scratch_left[..frames],
                        &mut self.scratch_right[..frames],
                        &self.midi_scratch,
                    );
                    let right = &self.scratch_right[..frames];
                    let (mono, left) = (&mut output[0], &self.scratch_left[..frames]);
                    for frame in 0..frames {
                        mono[frame] = (left[frame] + right[frame]) * 0.5;
                    }
                }
                2 => {
                    let (left, right) = output.split_at_mut(1);
                    core.process_stereo(&mut *left[0], &mut *right[0], &self.midi_scratch);
                }
                _ => {
                    // First two channels carry the stereo pair, the
                    // rest (host surround layouts) are silent.
                    let (stereo, rest) = output.split_at_mut(2);
                    let (left, right) = stereo.split_at_mut(1);
                    core.process_stereo(&mut *left[0], &mut *right[0], &self.midi_scratch);
                    for channel in rest {
                        channel.fill(0.0);
                    }
                }
            }
        }
        // The sequencer produces audio without host input, so silence
        // must not let the host suspend the plugin.
        ProcessStatus::KeepAlive
    }
}

impl DrumPlugin {
    /// Reconciles persisted values (edited in the GUI) and the host
    /// transport into the core's command ring; runs at the head of
    /// every block.
    fn sync_engine_state(&mut self, context: &impl ProcessContext<Self>) {
        let master_gain = *self.params.master_gain.lock().unwrap();
        let tempo = *self.params.tempo.lock().unwrap();
        let kit_path = self.params.kit_path.lock().unwrap().clone();
        let pattern_json = self.params.pattern_json.lock().unwrap().clone();
        // Host tempo is authoritative only while the transport runs;
        // otherwise the GUI's tempo slider rules.
        let host_tempo = if context.transport().playing {
            context.transport().tempo
        } else {
            None
        };

        {
            let mut core = self.core.lock().unwrap();
            if (core.state().master_gain - master_gain).abs() > f32::EPSILON {
                core.master_gain(master_gain);
            }
            if let Some(host_bpm) = host_tempo {
                if (core.state().tempo - host_bpm).abs() > f64::EPSILON {
                    core.set_tempo(host_bpm);
                }
            } else if (core.state().tempo - tempo).abs() > f64::EPSILON {
                core.set_tempo(tempo);
            }
            if core.state().kit_path != kit_path {
                core.set_kit(&kit_path);
            }
            let pushed = self.last_pattern_json.lock().unwrap();
            let pattern_changed = pushed.as_ref() != Some(&pattern_json);
            drop(pushed);
            if pattern_changed {
                if let Ok(pattern) = serde_json::from_str::<Pattern>(&pattern_json) {
                    core.set_pattern(pattern);
                }
                *self.last_pattern_json.lock().unwrap() = Some(pattern_json);
            }
        }
    }
}

/// Maps a host note event to the engine's MIDI dialect (byte velocity
/// scale; choke events from explicit chokes and velocity-zero note-ons
/// exactly like the legacy wrappers did).
fn to_midi_event<SysEx>(event: &NoteEvent<SysEx>) -> Option<MidiEvent> {
    match event {
        NoteEvent::NoteOn {
            timing,
            key,
            velocity,
            ..
        } => {
            let note = key.number()?;
            let offset = *timing as usize;
            // A velocity-zero note-on is a note-off in MIDI semantics
            // (the byte clamp would otherwise always register a hit).
            if *velocity <= 0.0 {
                Some(MidiEvent {
                    kind: MessageType::NoteOff,
                    note,
                    velocity: 0.0,
                    offset,
                })
            } else {
                Some(PluginCore::velocity_byte_event(
                    MessageType::OnSet,
                    note,
                    velocity.clamp(0.0, 1.0),
                    offset,
                ))
            }
        }
        NoteEvent::NoteOff {
            timing,
            key,
            velocity,
            ..
        } => {
            let note = key.number()?;
            Some(MidiEvent {
                kind: MessageType::NoteOff,
                note,
                // Note-offs "sound" only through their choking effect.
                velocity: velocity.clamp(0.0, 1.0),
                offset: *timing as usize,
            })
        }
        NoteEvent::Choke { timing, key, .. } => {
            let note = key.number()?;
            Some(MidiEvent {
                kind: MessageType::Choke,
                note,
                velocity: 1.0,
                offset: *timing as usize,
            })
        }
        // Channel pressure is deliberately ignored (matches the legacy
        // wrappers); poly pressure spans voices the engine never had.
        _ => None,
    }
}

// ---------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nice_plug::midi::{Channel, Key, VoiceID};

    #[test]
    fn persisted_fields_roundtrip_through_params() {
        let params = DrumParams::default();
        *params.kit_path.lock().unwrap() = "/some/kit".to_string();
        *params.pattern_json.lock().unwrap() = "{\"name\":\"x\"}".to_string();
        *params.master_gain.lock().unwrap() = 2.5;
        *params.tempo.lock().unwrap() = 143.5;

        // Values are persisted as JSON strings.
        let serialized = params.serialize_fields();
        assert_eq!(serialized["kit_path"], "\"/some/kit\"");
        assert_eq!(serialized["pattern_json"], "\"{\\\"name\\\":\\\"x\\\"}\"");
        assert_eq!(serialized["master_gain"], "2.5");
        assert_eq!(serialized["seq_tempo"], "143.5");

        let restored = DrumParams::default();
        restored.deserialize_fields(&serialized);
        assert_eq!(restored.kit_path.lock().unwrap().as_str(), "/some/kit");
        assert_eq!(
            restored.pattern_json.lock().unwrap().as_str(),
            "{\"name\":\"x\"}"
        );
        assert!((*restored.master_gain.lock().unwrap() - 2.5).abs() < f32::EPSILON);
        assert!((*restored.tempo.lock().unwrap() - 143.5).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_persisted_seeds_core_state() {
        let params = DrumParams::default();
        *params.kit_path.lock().unwrap() = "/fold/kit".to_string();
        *params.master_gain.lock().unwrap() = 0.75;
        *params.tempo.lock().unwrap() = 90.0;

        let mut core = PluginCore::default();
        params.apply_persisted(&mut core);

        let state = core.state();
        assert_eq!(state.kit_path, "/fold/kit");
        assert!((state.master_gain - 0.75).abs() < f32::EPSILON);
        assert!((state.tempo - 90.0).abs() < f64::EPSILON);
    }

    /// A velocity-zero note-on maps to note-off, a positive one to a
    /// byte-scaled hit, and out-of-range keys are dropped.
    #[test]
    fn note_events_map_to_midi_dialect() {
        let on: NoteEvent<()> = NoteEvent::NoteOn {
            timing: 7,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Number(36),
            velocity: 0.8,
        };
        let hit = to_midi_event(&on).expect("hit event");
        assert_eq!(hit.kind, MessageType::OnSet);
        assert_eq!(hit.note, 36);
        assert_eq!(hit.offset, 7);
        assert!((hit.velocity - 102.0).abs() < f32::EPSILON);

        let zero: NoteEvent<()> = NoteEvent::NoteOn {
            timing: 9,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Number(36),
            velocity: 0.0,
        };
        let off = to_midi_event(&zero).expect("velocity-zero off");
        assert_eq!(off.kind, MessageType::NoteOff);
        assert_eq!(off.velocity, 0.0);

        let choke: NoteEvent<()> = NoteEvent::Choke {
            timing: 12,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Number(40),
        };
        let ch = to_midi_event(&choke).expect("choke");
        assert_eq!(ch.kind, MessageType::Choke);
        assert_eq!(ch.note, 40);

        let wild: NoteEvent<()> = NoteEvent::NoteOn {
            timing: 3,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Wildcard,
            velocity: 1.0,
        };
        assert!(to_midi_event(&wild).is_none());
    }
}
