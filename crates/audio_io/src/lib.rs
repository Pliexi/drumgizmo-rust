// SPDX-License-Identifier: GPL-3.0-or-later
//! System audio and MIDI plumbing for the standalone app:
//!
//! - one cpal output stream that renders [`Engine`] blocks straight from
//!   the audio callback (the engine lives fully inside the callback
//!   closure, so there is no lock anywhere on the realtime path; control
//!   commands reach it through the engine's own lock-free command ring),
//! - one midir MIDI input connection whose callbacks decode raw MIDI
//!   bytes into [`MidiEvent`]s and push them into the same command ring
//!   with a zero offset (events land at the first frame of the next
//!   block, matching the engine's `push_midi` contract),
//! - pure decode/gather/scatter helpers that are unit-tested without any
//!   sound card.

use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use midir::{MidiInput, MidiInputConnection, MidiInputPort};

use dg_common::{MidiEvent, MessageType, RtCommand, SharedProducer};
use dg_engine::Engine;

/// Decode raw short MIDI bytes as delivered by a midir input callback
/// into engine events with a zero block offset.
///
/// Semantics follow upstream `audioinputenginemidi.cc`:
/// - `0x90` → Note-On (`MessageType::OnSet`), velocity kept in the
///   1..=127 byte scale (the engine applies the upstream
///   `(v - 0.5)/127` centering itself),
/// - `0x80` → Note-Off (upstream ignores those for triggering),
/// - `0xA0` (poly aftertouch) with pressure > 0 → Choke with velocity =
///   pressure (the engine only rams voices down on non-zero chokes),
/// - realtime/system bytes and running-status bytes without a leading
///   status are skipped.
///
/// The MIDI channel is deliberately ignored: the midimap, not the
/// channel, decides which instrument a note hits (like upstream, which
/// binds drums on any channel).
pub fn decode_midi_bytes(data: &[u8]) -> Vec<MidiEvent> {
    let Some(&status) = data.first() else {
        return Vec::new();
    };
    // Running status / realtime bytes alone carry no drum event here.
    if status < 0x80 {
        return Vec::new();
    }
    let payload = data.get(1..).unwrap_or(&[]);
    let note = payload.first().copied().unwrap_or(0);
    let data1 = payload.get(1).copied().unwrap_or(0);
    let event = match status & 0xF0 {
        0x90 => Some(MidiEvent {
            kind: MessageType::OnSet,
            note,
            velocity: data1 as f32,
            offset: 0,
        }),
        0x80 => Some(MidiEvent {
            kind: MessageType::NoteOff,
            note,
            velocity: 0.0,
            offset: 0,
        }),
        0xA0 => {
            if data1 > 0 {
                Some(MidiEvent {
                    kind: MessageType::Choke,
                    note,
                    velocity: data1 as f32,
                    offset: 0,
                })
            } else {
                None
            }
        }
        _ => None,
    };
    event.map_or_else(Vec::new, |event| vec![event])
}

/// Copies an interleaved stereo output buffer into two planar scratch
/// buffers so the engine can render without caring about interleaving.
pub fn gather_stereo_interleaved(data: &[f32], left: &mut [f32], right: &mut [f32]) {
    let frames = data.len() / 2;
    for frame in 0..frames.min(left.len()).min(right.len()) {
        left[frame] = data[2 * frame];
        right[frame] = data[2 * frame + 1];
    }
}

/// Writes planar left/right into an arbitrary-channel interleaved output
/// buffer: channel 0 = left, channel 1 = right (mono = their mean),
/// remaining channels are zeroed silence.
pub fn scatter_to_interleaved(data: &mut [f32], left: &[f32], right: &[f32]) {
    if data.is_empty() || left.is_empty() {
        return;
    }
    let frames = left.len().min(right.len());
    let out_channels = data.len() / frames;
    match out_channels {
        2 => {
            for frame in 0..frames {
                data[2 * frame] = left[frame];
                data[2 * frame + 1] = right[frame];
            }
        }
        1 => {
            for frame in 0..frames {
                data[frame] = (left[frame] + right[frame]) * 0.5;
            }
        }
        n if n > 2 => {
            for frame in 0..frames {
                let base = frame * n;
                data[base] = left[frame];
                data[base + 1] = right[frame];
                data[base + 2..base + n].fill(0.0);
            }
        }
        _ => {}
    }
}

/// Lock-free audio thread → UI statistics snapshot.
#[derive(Debug, Default)]
pub struct StreamStats {
    /// Total rendered frames since stream start.
    frames: AtomicU64,
    /// Peak absolute sample since the last take (stored as f32 bits).
    peak: AtomicU32,
    /// Non-zero once the error callback fired.
    failed: AtomicU32,
}

impl StreamStats {
    fn push_block(&self, peak: f32, frames: u64) {
        self.frames.fetch_add(frames, Ordering::Relaxed);
        let peak_bits = peak.to_bits();
        // Track the running maximum with a bit-encoded CAS loop; when the
        // CAS loses because another frame raised the bar, retry only if
        // this block still wins.
        let mut observed = self.peak.load(Ordering::Relaxed);
        loop {
            if f32::from_bits(observed) >= peak {
                return;
            }
            match self
                .peak
                .compare_exchange(observed, peak_bits, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => observed = actual, // someone raised the bar; re-check
            }
        }
    }

    fn note_failure(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Frames rendered so far.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Highest absolute sample since the last [`StreamStats::take_peak`]
    /// and resets it to zero.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0.0f32.to_bits(), Ordering::Relaxed))
    }

    /// True once the audio stream reported an error.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed) > 0
    }
}

/// Audio output errors (device selection and stream creation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// cpal could not find any output device.
    NoDevice(String),
    /// No usable configuration (no f32 output stream anywhere).
    NoConfig(String),
    /// cpal refused to build or start the stream.
    Stream(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoDevice(context) => write!(f, "no output device: {context}"),
            AudioError::NoConfig(context) => write!(f, "no output config: {context}"),
            AudioError::Stream(context) => write!(f, "stream error: {context}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// MIDI binding errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiError {
    /// midir has no input whose name matches the request.
    NoInput(String),
    /// midir failed to start the connection.
    Connect(String),
}

impl fmt::Display for MidiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MidiError::NoInput(context) => write!(f, "no midi input: {context}"),
            MidiError::Connect(context) => write!(f, "midi connect error: {context}"),
        }
    }
}

impl std::error::Error for MidiError {}

/// Output device plus the stream configuration the engine must adopt
/// (create the engine pair *after* this resolution so the rates match).
pub struct OutputSetup {
    device: cpal::Device,
    pub config: StreamConfig,
}

impl OutputSetup {
    /// Default host, default output device.
    pub fn detect() -> Result<Self, AudioError> {
        Self::detect_on_host(cpal::default_host())
    }

    /// Same with an explicit host (tests may force `null`).
    pub fn detect_on_host(host: cpal::Host) -> Result<Self, AudioError> {
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::NoDevice(host.id().to_string()))?;
        Self::choose(&device)
    }

    /// Picks an f32 config for the device in this order:
    /// the device default when it is already f32, else the first f32
    /// range at the default rate, else the max-rate f32 config of the
    /// first range. Only f32 is accepted because the engine renders f32
    /// directly (no format conversion anywhere in the output path).
    pub fn choose(device: &cpal::Device) -> Result<Self, AudioError> {
        let default_config = device
            .default_output_config()
            .map_err(|error| AudioError::NoConfig(format!("{error}")))?;
        let ranges: Vec<_> = device
            .supported_output_configs()
            .map(|iter| iter.filter(|range| range.sample_format() == SampleFormat::F32).collect())
            .unwrap_or_default();
        if ranges.is_empty() {
            return Err(AudioError::NoConfig(
                "device offers no f32 output stream".into(),
            ));
        }

        let config = if default_config.sample_format() == SampleFormat::F32 {
            default_config
        } else {
            let default_rate = default_config.sample_rate();
            ranges
                .iter()
                .find_map(|range| range.try_with_sample_rate(default_rate))
                .or_else(|| {
                    ranges
                        .first()
                        .map(|range| (*range).with_max_sample_rate())
                })
                .unwrap_or(default_config)
        };

        Ok(Self {
            device: device.clone(),
            config: config.config(),
        })
    }

    /// Builds the running cpal stream around `engine`. The engine moves
    /// into the audio callback and stays there for the stream lifetime;
    /// control happens through the command ring only.
    pub fn build(self, mut engine: Engine) -> Result<RunningStream, AudioError> {
        let channels = self.config.channels as usize;
        let sample_rate = self.config.sample_rate;
        // Per-block planar scratch, grows once if the host asks for a
        // bigger block than the initial 2048 frames, zero allocation in
        // the steady state.
        let mut left = vec![0.0f32; 2048];
        let mut right = vec![0.0f32; left.len()];

        let stats = Arc::new(StreamStats::default());
        let stats_callback = Arc::clone(&stats);
        let fail_callback = Arc::clone(&stats);

        let data_callback = move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels.max(1);
            if frames == 0 {
                return;
            }
            if left.len() < frames {
                // One-time growth when the host delivers a bigger block
                // than the initial scratch (2048 frames ≈ 46 ms @ 44.1k).
                left.resize(frames, 0.0);
                right.resize(frames, 0.0);
            }
            if channels == 2 {
                gather_stereo_interleaved(data, &mut left[..frames], &mut right[..frames]);
            }
            engine.render_stereo(&mut left[..frames], &mut right[..frames], &[]);
            scatter_to_interleaved(data, &left[..frames], &right[..frames]);
            let peak = data
                .iter()
                .fold(0.0f32, |best: f32, sample| sample.abs().max(best));
            stats_callback.push_block(peak, frames as u64);
        };

        let fail_callback_fn = move |error| {
            eprintln!("audio stream error: {error}");
            fail_callback.note_failure();
        };

        let stream = self
            .device
            .build_output_stream::<f32, _, _>(
                self.config,
                data_callback,
                fail_callback_fn,
                None,
            )
            .map_err(|error| AudioError::Stream(format!("{error}")))?;
        stream
            .play()
            .map_err(|error| AudioError::Stream(format!("{error}")))?;
        Ok(RunningStream {
            stream,
            stats,
            sample_rate,
            channels: self.config.channels,
        })
    }
}

/// A playing output stream; dropping (or [`RunningStream::stop`]) it
/// stops audio.
pub struct RunningStream {
    stream: cpal::Stream,
    pub stats: Arc<StreamStats>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl RunningStream {
    /// Stops and drops the underlying stream.
    pub fn stop(self) {
        // Dropping `stream` lets cpal join the audio thread.
        let _ = self.stream;
    }
}

/// Lists MIDI input port names as resolved by midir on the default client.
pub fn list_midi_inputs() -> Vec<String> {
    let Ok(input) = MidiInput::new("dg-midi-list") else {
        return Vec::new();
    };
    input
        .ports()
        .iter()
        .filter_map(|port| input.port_name(port).ok())
        .collect()
}

/// Keeps a MIDI input alive: callbacks push decoded events into the
/// engine's command ring (shared, host-side producer).
pub struct MidiInputHandle {
    /// The connection is typed over the shared producer; the handle
    /// keeps it alive for the port lifetime.
    _connection: MidiInputConnection<SharedProducer>,
    pub port: String,
}

/// Binds the input port whose name contains `name_fragment` (an exact
/// name works as a substring); an empty fragment or `"default"` picks
/// the first port. Decoded events go into `producer` as
/// [`RtCommand::Midi`].
pub fn bind_midi_input(
    name_fragment: &str,
    producer: SharedProducer,
) -> Result<MidiInputHandle, MidiError> {
    let input =
        MidiInput::new("drumgizmo-rust-midi").map_err(|e| MidiError::Connect(format!("{e}")))?;
    let ports = input.ports();
    if ports.is_empty() {
        return Err(MidiError::NoInput("no midi ports exist".into()));
    }
    let picked: Option<&MidiInputPort> = if name_fragment.is_empty()
        || name_fragment.eq_ignore_ascii_case("default")
    {
        Some(&ports[0])
    } else {
        ports
            .iter()
            .find(|port| {
                input
                    .port_name(port)
                    .map(|full| full.contains(name_fragment))
                    .unwrap_or(false)
            })
            .or(Some(&ports[0]))
    };
    let Some(port) = picked else {
        return Err(MidiError::NoInput(name_fragment.to_string()));
    };
    let port_name = input
        .port_name(port)
        .unwrap_or_else(|_| "unknown".into());
    let connection = input
        .connect(
            port,
            "drumgizmo-rust-in",
            move |_stamp, bytes, shared| {
                for event in decode_midi_bytes(bytes) {
                    // Messages cannot wait: dropping one beats stalling
                    // the midi thread, and the command ring is the only
                    // transport toward the audio thread. The lock is
                    // taken only against the GUI/CLI producers, never
                    // the audio thread.
                    let _ = shared.lock().unwrap().push(RtCommand::Midi(event));
                }
            },
            producer.clone(),
        )
        .map_err(|error| MidiError::Connect(format!("{error}")))?;
    Ok(MidiInputHandle {
        _connection: connection,
        port: port_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<MidiEvent> {
        decode_midi_bytes(bytes)
    }

    #[test]
    fn note_on_decodes_with_raw_byte_velocity() {
        let event = events(&[0x90, 0x24, 110, 0])[0];
        assert_eq!(event.kind, MessageType::OnSet);
        assert_eq!(event.note, 36);
        assert_eq!(event.velocity, 110.0);
        assert_eq!(event.offset, 0);
    }

    #[test]
    fn note_off_and_choke_decode_upstream_style() {
        let off = events(&[0x80, 60, 0])[0];
        assert_eq!(off.kind, MessageType::NoteOff);

        let choke = events(&[0xA0, 42, 90])[0];
        assert_eq!(choke.kind, MessageType::Choke);
        assert_eq!(choke.note, 42);
        assert_eq!(choke.velocity, 90.0);

        // zero pressure chokes are engine no-ops upstream, drop them
        assert!(events(&[0xA0, 42, 0]).is_empty());
    }

    #[test]
    fn realtime_and_running_status_bytes_are_skipped() {
        assert!(events(&[0xF8]).is_empty(), "clock");
        assert!(events(&[0xFE]).is_empty(), "active sensing");
        assert!(events(&[0x3C, 110]).is_empty(), "running status data");
        assert!(events(&[]).is_empty(), "empty message");
    }

    #[test]
    fn channels_are_ignored_only_midimap_binds() {
        let a = events(&[0x99, 36, 100])[0];
        let b = events(&[0x90, 36, 100])[0];
        assert_eq!(a.note, b.note);
        assert_eq!(a.kind, b.kind);
    }

    #[test]
    fn scatter_stereo_mono_and_surround() {
        let left = [0.5, 0.25, 0.0];
        let right = [0.1, 0.15, 0.05];

        let mut stereo = vec![0.0; 6];
        scatter_to_interleaved(&mut stereo, &left, &right);
        assert_eq!(stereo, vec![0.5, 0.1, 0.25, 0.15, 0.0, 0.05]);

        let mut mono = vec![0.0; 3];
        scatter_to_interleaved(&mut mono, &left, &right);
        // (l + r) / 2
        assert_eq!(mono, vec![0.3, 0.2, 0.025]);

        let mut surround = vec![9.0; 8];
        scatter_to_interleaved(&mut surround, &left[..2], &right[..2]);
        assert_eq!(surround, vec![0.5, 0.1, 0.0, 0.0, 0.25, 0.15, 0.0, 0.0]);
    }

    #[test]
    fn gather_scatter_roundtrip_interleaved() {
        let original: Vec<f32> = (0..128).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut left = vec![0.0f32; 64];
        let mut right = vec![0.0f32; 64];
        gather_stereo_interleaved(&original, &mut left, &mut right);
        for frame in 0..64 {
            assert_eq!(left[frame], original[2 * frame]);
            assert_eq!(right[frame], original[2 * frame + 1]);
        }
        let mut copy = vec![0.0f32; original.len()];
        scatter_to_interleaved(&mut copy, &left, &right);
        assert_eq!(copy, original);
    }

    #[test]
    fn stats_accumulate_frames_and_peak_maxima() {
        let stats = StreamStats::default();
        stats.push_block(0.25, 64);
        stats.push_block(0.75, 128);
        stats.push_block(0.5, 64);
        assert_eq!(stats.frames(), 64 + 128 + 64);
        assert_eq!(stats.take_peak(), 0.75, "running maximum");
        assert_eq!(stats.take_peak(), 0.0, "reset on take");
    }

    #[test]
    fn stats_flags_failures() {
        let stats = StreamStats::default();
        assert!(!stats.failed());
        stats.note_failure();
        assert!(stats.failed());
    }

    #[test]
    fn peak_accumulates_running_maximum() {
        // The CAS-refined monotone max: later lower peaks must not
        // clobber an earlier higher one.
        let stats = StreamStats::default();
        stats.push_block(0.9, 1);
        stats.push_block(0.1, 1);
        stats.push_block(0.2, 1);
        assert_eq!(stats.take_peak(), 0.9);
    }

    #[test]
    fn decoded_events_sound_through_engine_ring() {
        // The full glue minus the OS callbacks: decode a raw kick drum
        // message, route it the same way the midir callback does, and
        // render one block with the engine pair.
        let dir = std::env::temp_dir().join("dg-audio-io").join("ring-glue");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kit_path = dg_kit::kitgen::generate_test_kit(
            &dir,
            44100,
            dg_kit::kitgen::TestKitFlavour::Small,
        )
        .unwrap();
        let mut loader = dg_kit::KitLoader::start(kit_path.to_str().unwrap(), 44100);
        let kit = loop {
            match loader.poll() {
                Some(Ok(kit)) => break kit,
                Some(Err(error)) => panic!("kit failed: {error:?}"),
                None => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        };
        let midimap_path = kit.basepath.join("midimap.xml");
        let midimap = dg_kit::parser::parse_midimap_file(&midimap_path).unwrap_or_default();

        let (engine, mut producer) = dg_engine::engine_pair(44100, 64);
        let mut engine = engine;
        engine.set_kit(kit, Some(&midimap));

        // Raw note-on for the kick (midimap binds note 36) ...
        for event in decode_midi_bytes(&[0x90, 36, 110]) {
            producer.push(dg_common::RtCommand::Midi(event)).unwrap();
        }
        producer
            .push(dg_common::RtCommand::Midi(decode_midi_bytes(&[0xA0, 36, 70]).remove(0)))
            .unwrap();

        let mut left = vec![0.0f32; 512];
        let mut right = vec![0.0f32; 512];
        engine.render_stereo(&mut left, &mut right, &[]);
        let peak = left.iter().fold(0.0f32, |best, s| s.abs().max(best));
        assert!(peak > 1e-3, "kick sounded through the ring: {peak}");
    }
}

