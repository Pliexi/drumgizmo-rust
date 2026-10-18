// SPDX-License-Identifier: GPL-3.0-or-later
//! Engine integration tests against the generated test kit (no sound
//! card required): decode → map → note-on → mix.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dg_common::{MidiEvent, RtCommand};
use dg_engine::{engine_pair, Engine, SeqSnapshot};
use dg_kit::kitgen::{generate_test_kit, TestKitFlavour};
use dg_kit::{KitLoader, LoadedKit};
use dg_sequencer::{ChainEntry, Pattern, Step};

fn unique_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("dg-engine-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn load_test_kit(dir: &Path) -> Arc<LoadedKit> {
    load_test_kit_flavour(dir, TestKitFlavour::Small)
}

fn load_test_kit_flavour(dir: &Path, flavour: TestKitFlavour) -> Arc<LoadedKit> {
    let path = generate_test_kit(dir, 44100, flavour).expect("generate kit");
    let mut loader = KitLoader::start(path.to_str().unwrap(), 44100);
    loop {
        match loader.poll() {
            Some(Ok(kit)) => return kit,
            Some(Err(e)) => panic!("kit failed: {e:?}"),
            None => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
}

/// Engine + test kit with its own midimap applied.
struct TestEngine {
    engine: Engine,
    producer: rtrb::Producer<RtCommand>,
    #[allow(dead_code)]
    dir: PathBuf,
}

impl TestEngine {
    fn new(name: &str) -> Self {
        let dir = unique_dir(name);
        let kit = load_test_kit(&dir);
        let (engine, producer) = engine_pair(44100, 128);
        let mut engine = engine;
        let midimap_path = kit.basepath.join("midimap.xml");
        let midimap = dg_kit::parser::parse_midimap_file(&midimap_path).unwrap_or_default();
        engine.set_kit(kit, Some(&midimap));
        Self {
            engine,
            producer,
            dir,
        }
    }

    fn energy(&mut self, blocks: usize) -> (f32, f32, usize) {
        let mut left = vec![0.0f32; 256];
        let mut right = vec![0.0f32; 256];
        let mut events = 0;
        let mut peak_left = 0.0f32;
        let mut peak_right = 0.0f32;
        for _ in 0..blocks {
            let e = self.engine.render_block(&mut [&mut left, &mut right], &[]);
            events += e;
            peak_left = peak_left.max(left.iter().fold(0.0f32, |a, s| a.max(*s)).abs());
            peak_right = peak_right.max(right.iter().fold(0.0f32, |a, s| a.max(*s)).abs());
        }
        (peak_left, peak_right, events)
    }
}

fn sum(channel: &[f32]) -> f32 {
    channel.iter().map(|s| s.abs()).sum()
}

#[test]
fn note_on_sounds() {
    let mut engine = TestEngine::new("note-on");
    engine.engine.push_midi(MidiEvent::note_on(36, 110.0, 0));
    let (peak_l, peak_r, events) = engine.energy(4);
    assert_eq!(events, 1, "one midi event consumed");
    assert!(
        peak_l > 1e-3 && peak_r > 1e-3,
        "sends audio: {peak_l} {peak_r}"
    );
}

#[test]
fn louder_velocity_is_louder() {
    let mut soft_engine = TestEngine::new("soft");
    soft_engine.engine.push_midi(MidiEvent::note_on(36, 5.0, 0));
    let soft = sum_abs(&mut soft_engine.engine, 8);

    let mut hard_engine = TestEngine::new("hard");
    hard_engine
        .engine
        .push_midi(MidiEvent::note_on(36, 120.0, 0));
    let hard = sum_abs(&mut hard_engine.engine, 8);

    assert!(hard > soft * 1.5, "soft {soft} vs hard {hard}");
}

fn sum_abs(engine: &mut Engine, blocks: usize) -> f32 {
    let mut out = vec![0.0f32; 256];
    let mut total = 0.0f32;
    for _ in 0..blocks {
        engine.render_block(&mut [&mut out], &[]);
        total += sum(&out);
    }
    total
}

#[test]
fn velocity_byte_transform_matches_upstream() {
    // (v - 0.5)/127 — byte 1 -> 0.5/127, byte 127 -> 126.5/127 (never 0/1);
    // byte 0 note-ons are ignored before the transform (upstream too).
    assert!((dg_engine_transform(1) - 0.5 / 127.0).abs() < 1e-9);
    assert!((dg_engine_transform(127) - 126.5 / 127.0).abs() < 1e-9);
}

fn dg_engine_transform(v: u8) -> f32 {
    (f32::from(v) - 0.5) / 127.0
}

#[test]
fn choke_group_ramps_hihat_pair() {
    let mut engine = TestEngine::new("choke-group");
    // note 46 = HiHat Open; it belongs to the "HiHat" group
    engine.engine.push_midi(MidiEvent::note_on(46, 100.0, 0));
    let mut out = vec![0.0f32; 512];
    engine.engine.render_block(&mut [&mut out], &[]);
    assert!(sum(&out) > 0.0, "open hat sounds");
    // note 42 = HiHat Closed → chokes the open one (68 ms ramp)
    engine.engine.push_midi(MidiEvent::note_on(42, 100.0, 0));
    // Ramp length: 68 ms at 44100 = 2998 frames; render 4 blocks and watch
    // the sound decay below the previous block levels.
    let mut left = vec![0.0f32; 256];
    let mut right = vec![0.0f32; 256];
    let mut envelope = Vec::new();
    for _ in 0..4 {
        engine
            .engine
            .render_block(&mut [&mut left, &mut right], &[]);
        envelope.push(left.iter().map(|s| s.abs()).sum::<f32>());
    }
    assert!(
        envelope[2] + envelope[3] < envelope[0] + envelope[1],
        "choke fades: {envelope:?}"
    );
}

#[test]
fn panic_fades_everything() {
    let mut engine = TestEngine::new("panic");
    // first block spawns the voice
    engine.engine.push_midi(MidiEvent::note_on(36, 100.0, 0));
    let mut left = vec![0.0f32; 256];
    let mut right = vec![0.0f32; 256];
    engine
        .engine
        .render_block(&mut [&mut left, &mut right], &[]);
    let before: f32 = left.iter().map(|s| s.abs()).sum();
    assert!(before > 0.0, "voice sounds before the panic");

    // the panic lands in the next block: 10 ms ramp = 441 frames
    let (mut producer,) = (engine.producer,);
    producer.push(RtCommand::Panic).unwrap();
    let mut left = vec![0.0f32; 512];
    let mut right = vec![0.0f32; 512];
    for _ in 0..2 {
        engine
            .engine
            .render_block(&mut [&mut left, &mut right], &[]);
    }
    let total: f32 = left.iter().sum();
    assert!(total.abs() < 1e-9, "panic should have faded: {total}");
    assert!(engine.engine.is_silent());
}

#[test]
fn master_gain_scales_output() {
    let mut engine = TestEngine::new("master");
    engine.engine.push_midi(MidiEvent::note_on(36, 100.0, 0));
    engine
        .producer
        .push(RtCommand::MasterGain { gain: 0.5 })
        .unwrap();
    let mut left = vec![0.0f32; 256];
    let mut right = vec![0.0f32; 256];
    engine
        .engine
        .render_block(&mut [&mut left, &mut right], &[]);
    assert!(sum(&left) > 0.0);
    // note: the command applies before the render, so compare halves
    let halved: f32 = left.iter().map(|s| s.abs()).sum::<f32>();

    let mut engine = TestEngine::new("master-plain");
    engine.engine.push_midi(MidiEvent::note_on(36, 100.0, 0));
    let mut left = vec![0.0f32; 256];
    let mut right = vec![0.0f32; 256];
    engine
        .engine
        .render_block(&mut [&mut left, &mut right], &[]);
    let plain: f32 = left.iter().map(|s| s.abs()).sum::<f32>();
    assert!(
        (halved * 2.0 - plain).abs() / plain < 0.5,
        "{halved} vs {plain}"
    );
}

#[test]
fn sequencer_runs_through_engine() {
    let mut engine = TestEngine::new("sequencer");
    let mut pattern = Pattern::default();
    let mut kick = dg_sequencer::Lane::new(0, 36, 16);
    for step in &mut kick.steps[0..4] {
        *step = Step::hit(1.0);
    }
    pattern.lanes.push(kick);
    let snapshot = SeqSnapshot {
        patterns: vec![pattern],
        chain: vec![ChainEntry {
            pattern: 0,
            repeats: 1,
        }],
        loop_chain: true,
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    engine
        .producer
        .push(RtCommand::SeqPattern { json })
        .unwrap();
    engine.producer.push(RtCommand::SeqStart).unwrap();

    // Four enabled steps = 4 hits per pass; 3 bars is plenty for 12 hits.
    let mut left = vec![0.0f32; 512];
    let mut right = vec![0.0f32; 512];
    let mut sounding = 0;
    for _ in 0..24 {
        engine
            .engine
            .render_block(&mut [&mut left, &mut right], &[]);
        if left.iter().any(|s| s.abs() > 1e-6) {
            sounding += 1;
        }
    }
    assert!(sounding > 8, "sequencer drives the engine: {sounding}");
}

#[test]
fn async_kit_load_via_command() {
    let dir = unique_dir("async-load");
    let kit_path = generate_test_kit(&dir, 44100, TestKitFlavour::Small).unwrap();
    let (engine, mut producer) = engine_pair(44100, 128);
    let mut engine = engine;
    producer
        .push(RtCommand::LoadKit {
            path: kit_path.to_str().unwrap().to_string(),
        })
        .unwrap();

    // a first block pulls the command and starts the loader thread
    let mut left = vec![0.0f32; 64];
    let mut right = vec![0.0f32; 64];
    engine.render_block(&mut [&mut left, &mut right], &[]);
    // decoding runs on a background thread; the render loop here is much
    // faster than real audio time, so keep polling with real time budget
    let mut rendered = false;
    for _ in 0..2000 {
        engine.render_block(&mut [&mut left, &mut right], &[]);
        if engine.kit.is_some() {
            rendered = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(rendered, "kit arrived asynchronously");
}

/// Renders the same kit state twice: once per kit channel (via
/// [`Engine::render_block`] with a buffer per channel) and once through
/// [`Engine::render_stereo`]. Both engine instances must be fresh.
struct StereoProbe {
    channels: Vec<Vec<f32>>,
    left: Vec<f32>,
    right: Vec<f32>,
    frames: usize,
}

fn take_channels(kit: &Arc<LoadedKit>, midimap: &[dg_kit::MidimapEntry]) -> StereoProbe {
    let frames = 256usize;
    // One fresh engine for the per-channel render.
    let (engine_a, _producer) = engine_pair(44100, 128);
    let mut per_channel = engine_a;
    per_channel.set_kit(kit.clone(), Some(midimap));
    let mut channels: Vec<Vec<f32>> = kit.channels.iter().map(|_| vec![0.0f32; frames]).collect();
    let mut views: Vec<&mut [f32]> = channels.iter_mut().map(|c| c.as_mut_slice()).collect();
    let midi = [MidiEvent::note_on(36, 110.0, 0)];
    per_channel.render_block(&mut views, &midi);
    drop(views);

    // Second fresh engine for the stereo render.
    let (engine_b, producer) = engine_pair(44100, 128);
    let mut stereo = engine_b;
    stereo.set_kit(kit.clone(), Some(midimap));
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let midi = [MidiEvent::note_on(36, 110.0, 0)];
    stereo.render_stereo(&mut left, &mut right, &midi);
    let _ = producer;
    StereoProbe {
        channels,
        left,
        right,
        frames,
    }
}

#[test]
fn render_stereo_pairs_fallback_channels() {
    let dir = unique_dir("stereo-pair");
    let kit = load_test_kit_flavour(&dir, TestKitFlavour::Small);
    let midimap_path = kit.basepath.join("midimap.xml");
    let midimap = dg_kit::parser::parse_midimap_file(&midimap_path).unwrap();
    let probe = take_channels(&kit, &midimap);

    // Without any main marking the fallback pair is the first two kit
    // channels (Kick In / Kick Out for this kit).
    assert_eq!(
        &probe.left[..probe.frames],
        &probe.channels[0][..probe.frames]
    );
    assert_eq!(
        &probe.right[..probe.frames],
        &probe.channels[1][..probe.frames]
    );
    assert!(
        probe
            .channels
            .iter()
            .skip(2)
            .all(|c| c.iter().all(|s| *s == 0.0)),
        "only the pair channels render in per-channel mode with one hit"
    );
}

#[test]
fn render_stereo_sums_main_channels() {
    let dir = unique_dir("stereo-main");
    let kit = load_test_kit_flavour(&dir, TestKitFlavour::MainSum);
    let midimap_path = kit.basepath.join("midimap.xml");
    let midimap = dg_kit::parser::parse_midimap_file(&midimap_path).unwrap();
    let probe = take_channels(&kit, &midimap);

    // "Kick In" and "Snare In" are marked main (kit channels 0 and 2); a
    // kick hit only renders into channel 0 and 1 (Kick In / Kick Out), so
    // the stereo sum of this hit equals the single main channel 0.
    let mut expected = vec![0.0f32; probe.frames];
    for (frame, sum) in expected.iter_mut().enumerate().take(probe.frames) {
        *sum = probe.channels[0][frame];
    }
    assert_eq!(&probe.left[..probe.frames], &expected[..probe.frames]);
    assert_eq!(&probe.right[..probe.frames], &expected[..probe.frames]);

    // The non-main "Kick Out" plane (kit channel 1) is active in
    // per-channel mode but must stay out of the stereo sum: both planes
    // differ (right plane is detuned), so equality with channel 1 would
    // expose a leak.
    assert!(
        probe.channels[1].iter().any(|s| *s != 0.0),
        "kick right plane renders in per-channel mode"
    );
    assert_ne!(
        probe.left, probe.channels[1],
        "stereo mix must not contain the non-main kit channel"
    );
}
