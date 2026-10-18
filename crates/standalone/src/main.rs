// SPDX-License-Identifier: GPL-3.0-or-later
//! Standalone application: cpal output stream driving the realtime
//! engine, an optional MIDI input feeding its command ring, and the
//! egui window on top. No sound card? The audio path fails loudly on
//! stderr and the GUI keeps running silent (see docs/decisions.md).

use std::collections::VecDeque;
use std::process::ExitCode;
use std::sync::Mutex;

use rtrb::Producer;

use dg_audio_io::{bind_midi_input, list_midi_inputs, OutputSetup};
use dg_common::{RtCommand, SharedProducer};

/// Command ring capacity shared by MIDI, GUI and the CLI pre-commands.
const COMMAND_CAPACITY: usize = 256;

const USAGE: &str = "\
drumgizmo-rust — DrumGizmo re-implementation in Rust (kit sampler + sequencer)

USAGE:
    drumgizmo-rust [OPTIONS] [KIT_PATH]

OPTIONS:
    --kit <PATH>       initial kit directory (same as the positional argument)
    --midi-in <NAME>   MIDI input port name fragment; \"default\" uses the first port
    --gain <VALUE>     master gain 0.0..=8.0 (default 1.0)
    --scale <FACTOR>   UI scale override (default: platform DPI)
    --list-midi        list MIDI input ports and exit
    --help             this text
    --version          print the version and exit
";

/// What the command line asks for.
#[derive(Debug, Clone)]
pub enum Action {
    Run(Config),
    ListMidi,
    Help,
    Version,
}

/// Parsed command line configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Initial kit directory (empty: load through the GUI later).
    pub kit_path: String,
    /// MIDI input port name fragment (`default` picks the first port).
    pub midi_in: String,
    /// Master gain 0.0..=8.0.
    pub gain: f32,
    /// UI scale multiplier (0.0 = platform default).
    pub scale: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            kit_path: String::new(),
            midi_in: "default".into(),
            gain: 1.0,
            scale: 0.0,
        }
    }
}

impl Config {
    pub fn default_gain() -> f32 {
        1.0
    }
}

/// Consumes the value for a flag that takes one: either the inline
/// `--flag=value` form or the next argv entry.
fn fetch_value(
    args: &[String],
    index: &mut usize,
    inline: Option<String>,
    flag: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value);
    }
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("--{} needs a value", flag.trim_start_matches('-')))
}

/// Parses argv; a `Help`/`Version` action carries the message text.
pub fn parse_args(args: &[String]) -> Result<Action, String> {
    let mut config = Config::default();
    let mut positional: VecDeque<String> = VecDeque::new();
    let mut index = 0usize;
    while index < args.len() {
        let raw = args[index].as_str().to_string();
        // Whole-word actions first (values are never split off them).
        match raw.as_str() {
            "--help" | "-h" => return Ok(Action::Help),
            "--version" => return Ok(Action::Version),
            "--list-midi" => return Ok(Action::ListMidi),
            _ => {}
        }
        // Only flags may carry `=value`; a kit path like `a=b` stays a
        // positional argument untouched.
        let is_flag = raw.starts_with("--") || (raw.len() == 2 && raw.starts_with('-'));
        let (name, inline) = if is_flag {
            match raw.split_once('=') {
                Some((name, value)) => (name.to_string(), Some(value.to_string())),
                None => (raw.clone(), None),
            }
        } else {
            (raw.clone(), None)
        };
        match name.as_str() {
            "--kit" | "-k" => {
                config.kit_path = fetch_value(args, &mut index, inline, "--kit")?;
            }
            "--midi-in" | "-m" => {
                config.midi_in = fetch_value(args, &mut index, inline, "--midi-in")?;
            }
            "--gain" | "-g" => {
                let value = fetch_value(args, &mut index, inline, "--gain")?;
                config.gain = value
                    .parse()
                    .map_err(|_| "gain must be a number 0.0..=8.0".to_string())?;
                if !(0.0..=8.0).contains(&config.gain) {
                    return Err("gain must be 0.0..=8.0".to_string());
                }
            }
            "--scale" | "-s" => {
                let value = fetch_value(args, &mut index, inline, "--scale")?;
                config.scale = value
                    .parse()
                    .map_err(|_| "scale must be a number > 0".to_string())?;
                if config.scale <= 0.0 {
                    return Err("scale must be > 0".to_string());
                }
            }
            name if name.starts_with('-') => {
                return Err(format!("unknown option {name}\n\n{USAGE}"));
            }
            _ => positional.push_back(raw),
        }
        index += 1;
    }
    if positional.len() > 1 {
        return Err(format!("unexpected extra arguments\n\n{USAGE}"));
    }
    if let Some(path) = positional.pop_front() {
        config.kit_path = path;
    }
    Ok(Action::Run(config))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Action::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Action::Version) => {
            println!("drumgizmo-rust {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Action::ListMidi) => {
            for (number, name) in list_midi_inputs().iter().enumerate() {
                println!("{number}: {name}");
            }
            ExitCode::SUCCESS
        }
        Ok(Action::Run(config)) => match run(&config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

/// Wraps the engine ring producer for multi-sender (GUI + MIDI + CLI)
/// host-side use.
fn shared(producer: Producer<RtCommand>) -> SharedProducer {
    std::sync::Arc::new(Mutex::new(producer))
}

/// Builds the engine + stream + MIDI graph, then enters the GUI main
/// loop; teardown happens when the window closes. Return carries only
/// hard failures (GUI cannot open); audio/MIDI problems are warnings
/// because both degrade gracefully.
pub fn run(config: &Config) -> Result<(), String> {
    // 1. Output device: the engine bakes the samplerate in, so the
    //    stream configuration must be known before the engine pair.
    //    Both failure paths keep the app alive with a silent 44.1 kHz
    //    pair so the GUI stays functional without a sound card.
    let (stream_result, engine_slot, producer, watch, audio_status) = match OutputSetup::detect() {
        Ok(setup) => {
            let (engine, producer) =
                dg_engine::engine_pair(u64::from(setup.config.sample_rate), COMMAND_CAPACITY);
            let watch = engine.watch();
            let producer: SharedProducer = shared(producer);
            match setup.build(engine) {
                Ok(stream) => {
                    let status = dg_common::AudioStatusSnapshot {
                        running: true,
                        sample_rate: stream.sample_rate,
                        channels: stream.channels,
                        note: format!(
                            "{} Hz / {} ch streaming",
                            stream.sample_rate, stream.channels
                        ),
                    };
                    (Ok(stream), EngineSlot::Running, producer, watch, status)
                }
                Err(stream_error) => {
                    eprintln!("warning: audio stream failed: {stream_error}; running silent");
                    let (engine, producer) = dg_engine::engine_pair(44100, COMMAND_CAPACITY);
                    let watch = engine.watch();
                    let status = dg_common::AudioStatusSnapshot {
                        running: false,
                        sample_rate: 0,
                        channels: 0,
                        note: format!("stream failed: {stream_error}"),
                    };
                    (
                        Err(stream_error),
                        EngineSlot::Silent(Box::new(engine)),
                        shared(producer),
                        watch,
                        status,
                    )
                }
            }
        }
        Err(device_error) => {
            eprintln!("warning: no output device: {device_error}; running silent");
            let (engine, producer) = dg_engine::engine_pair(44100, COMMAND_CAPACITY);
            let watch = engine.watch();
            let status = dg_common::AudioStatusSnapshot {
                running: false,
                sample_rate: 0,
                channels: 0,
                note: format!("no device: {device_error}"),
            };
            (
                Err(device_error),
                EngineSlot::Silent(Box::new(engine)),
                shared(producer),
                watch,
                status,
            )
        }
    };

    // 2. CLI pre-commands reach the engine through the ring before the
    //    first render block (LoadKit is asynchronous from there on).
    if !config.kit_path.is_empty() {
        let _ = producer.lock().unwrap().push(RtCommand::LoadKit {
            path: config.kit_path.clone(),
        });
    }
    if (config.gain - Config::default_gain()).abs() > f32::EPSILON {
        let _ = producer
            .lock()
            .unwrap()
            .push(RtCommand::MasterGain { gain: config.gain });
    }

    // 3. MIDI input feeds the same ring; losing the port must not stop
    //    the application.
    let midi = bind_midi_input(&config.midi_in, producer.clone());
    match &midi {
        Ok(handle) => eprintln!("midi: connected to {}", handle.port),
        Err(error) => eprintln!("midi: not connected ({error}), continuing"),
    }

    // 4. GUI on the main thread; when the window closes the stream is
    //    stopped and the MIDI connection dropped.
    let options = dg_gui::AppOptions {
        kit_path: config.kit_path.clone(),
        master_gain: config.gain,
        scale_override: config.scale,
        audio: Some(audio_status),
        watch: Some(watch),
        ..Default::default()
    };
    let gui_result = dg_gui::run(options, producer);
    let _dropped_engine = engine_slot; // silent fallback engine dies with the scope
    if let Ok(handle) = midi {
        eprintln!("midi: dropped port {}", handle.port);
    }
    if let Ok(stream) = stream_result {
        let stats = stream.stats.clone();
        stream.stop();
        eprintln!(
            "audio: {} frames rendered, final peak {:+.3}{}",
            stats.frames(),
            stats.take_peak(),
            if stats.failed() {
                ", stream failed"
            } else {
                ""
            }
        );
    }
    match gui_result {
        Ok(()) => Ok(()),
        Err(error) => Err(format!("gui: {error:?}")),
    }
}

/// Engine ownership marker: the running engine already lives in the
/// audio callback (moved by `OutputSetup::build`); only the silent
/// fallback still owns one on the main thread, and it simply dies
/// after the GUI loop.
#[must_use]
enum EngineSlot {
    Running,
    /// Carries the dummy engine purely for lifetime bookkeeping: it
    /// releases the silent pair's resources when the GUI loop ends.
    #[allow(dead_code)]
    Silent(Box<dg_engine::Engine>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_and_positional_kit() {
        match parse_args(&[]).expect("no args") {
            Action::Run(config) => {
                assert!(config.kit_path.is_empty());
                assert!((config.gain - 1.0).abs() < f32::EPSILON);
                assert_eq!(config.midi_in, "default");
            }
            other => panic!("run expected, got {other:?}"),
        }

        match parse_args(&args(&["kits/open_celest_loops"])).expect("positional kit") {
            Action::Run(config) => assert_eq!(config.kit_path, "kits/open_celest_loops"),
            other => panic!("run expected, got {other:?}"),
        }
    }

    #[test]
    fn flags_with_equals_and_separate_values() {
        match parse_args(&args(&["--kit=kits/a", "--midi-in", "MP3", "--gain=2.5"])).expect("flags")
        {
            Action::Run(config) => {
                assert_eq!(config.kit_path, "kits/a");
                assert_eq!(config.midi_in, "MP3");
                assert!((config.gain - 2.5).abs() < f32::EPSILON);
            }
            other => panic!("run expected, got {other:?}"),
        }
    }

    #[test]
    fn bad_values_fail_with_reasons() {
        assert!(parse_args(&args(&["--gain", "no"])).is_err());
        assert!(parse_args(&args(&["--gain", "42"])).is_err());
        assert!(parse_args(&args(&["--scale", "0"])).is_err());
        assert!(parse_args(&args(&["--what"])).is_err());
        assert!(parse_args(&args(&["--midi-in"])).is_err());
        assert!(parse_args(&args(&["kit1", "kit2"])).is_err());
    }

    #[test]
    fn special_actions() {
        assert!(matches!(
            parse_args(&args(&["--version"])),
            Ok(Action::Version)
        ));
        assert!(matches!(parse_args(&args(&["--help"])), Ok(Action::Help)));
        assert!(matches!(
            parse_args(&args(&["--list-midi"])),
            Ok(Action::ListMidi)
        ));
    }
}
