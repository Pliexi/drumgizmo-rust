// SPDX-License-Identifier: GPL-3.0-or-later
//! Standalone-audio-graph integration test: the real command path the
//! app uses (LoadKit via the ring, pattern via SeqPattern JSON, transport
//! via SeqStart) followed only by `render_stereo` pulls — exactly what
//! the cpal callback does. Asserts the output carries energy, so "kit
//! loads but stays silent" cannot happen without the engine noticing
//! at least one broken link.

use std::time::Duration;

use dg_common::RtCommand;
use dg_engine::{engine_pair, EngineWatch};
use dg_kit::kitgen::{generate_test_kit, TestKitFlavour};
use dg_sequencer::{ChainEntry, Lane, Pattern};

/// Drives `render_stereo` in 512-frame blocks until the watch reports the
/// kit installed (or the attempt budget runs out); returns (l, r) mix.
fn render_until_kit(
    engine: &mut dg_engine::Engine,
    watch: &EngineWatch,
    blocks: usize,
) -> (Vec<f32>, Vec<f32>) {
    let block = 512usize;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for _ in 0..blocks {
        let mut l = vec![0.0f32; block];
        let mut r = vec![0.0f32; block];
        engine.render_stereo(&mut l, &mut r, &[]);
        left.extend_from_slice(&l);
        right.extend_from_slice(&r);
        if watch
            .kit_generation
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    (left, right)
}

/// Peak magnitude of any sample.
fn peak(buffer: &[f32]) -> f32 {
    buffer
        .iter()
        .fold(0.0f32, |best, sample| best.max(sample.abs()))
}

#[test]
fn load_pattern_play_produces_energy_and_watch_tracks() {
    let target = std::env::temp_dir().join("dg-standalone-flow-test");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();

    // 1. Kit on disk, generated like the user's kits/test-kit.
    let xml = generate_test_kit(&target, 44100, TestKitFlavour::Small).unwrap();
    let kit_dir = xml.parent().unwrap().to_path_buf();

    // 2. Engine pair exactly like the standalone main.
    let rate = 44100u64;
    let (mut engine, mut producer) = engine_pair(rate, 256);
    let watch = engine.watch();
    assert_eq!(
        watch
            .kit_generation
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "fresh engine has no kit"
    );

    // 3. GUI "Load Kit" click: directory path through the ring.
    producer
        .push(RtCommand::LoadKit {
            path: kit_dir.display().to_string(),
        })
        .unwrap();

    let (left, right) = render_until_kit(&mut engine, &watch, 400);
    assert!(
        watch
            .kit_generation
            .load(std::sync::atomic::Ordering::Acquire)
            > 0,
        "LoadKit must install the kit within the render pull loop"
    );
    assert_eq!(peak(&left).max(peak(&right)), 0.0, "silence before any hit");

    // 4. GUI lanes + demo beat, serialized exactly like the app does.
    let mut pattern = Pattern {
        length: 16,
        ..Default::default()
    };
    for (index, (note, steps)) in [
        (36u8, vec![0usize, 10]), // Kick
        (38, vec![4, 12]),        // Snare
        (42, vec![2, 6, 10, 14]), // HiHat Closed
        (45, Vec::new()),         // Tom
    ]
    .into_iter()
    .enumerate()
    {
        let mut lane = Lane::new(index, note, pattern.length);
        for step in steps {
            lane.ensure_length(step + 1);
            lane.steps[step].enabled = true;
            lane.steps[step].velocity = 0.85;
        }
        pattern.lanes.push(lane);
    }
    let snapshot = dg_engine::SeqSnapshot {
        patterns: vec![pattern],
        chain: vec![ChainEntry {
            pattern: 0,
            repeats: 1,
        }],
        loop_chain: true,
    };
    producer
        .push(RtCommand::SeqPattern {
            json: serde_json::to_string(&snapshot).unwrap(),
        })
        .unwrap();

    // 5. Transport start, then ~8s of audio pull to cover playback.
    producer.push(RtCommand::SeqStart).unwrap();
    let mut playing_left = Vec::new();
    let mut playing_right = Vec::new();
    let blocks = (rate as usize * 8) / 512;
    for _ in 0..blocks {
        let mut l = vec![0.0f32; 512];
        let mut r = vec![0.0f32; 512];
        engine.render_stereo(&mut l, &mut r, &[]);
        playing_left.extend_from_slice(&l);
        playing_right.extend_from_slice(&r);
    }

    assert!(
        watch.playing.load(std::sync::atomic::Ordering::Acquire),
        "watch must reflect the running transport"
    );
    let rms: f32 = (playing_left.iter().sum::<f32>() / playing_left.len() as f32)
        .abs()
        .sqrt();
    assert!(
        peak(&playing_left) > 0.2 && peak(&playing_right) > 0.2,
        "sequenced playback must be audible (peak left {:.4})",
        peak(&playing_left)
    );
    assert!(rms > 0.001);

    // 6. Stop: the watch flips back and the step resets.
    producer.push(RtCommand::SeqStop).unwrap();
    let mut silent_tail = 0.0f32;
    for _ in 0..64 {
        let mut l = vec![0.0f32; 512];
        let mut r = vec![0.0f32; 512];
        engine.render_stereo(&mut l, &mut r, &[]);
        silent_tail = silent_tail.max(peak(&l).max(peak(&r)));
    }
    assert!(
        !watch.playing.load(std::sync::atomic::Ordering::Acquire),
        "stop must clear the transport watch"
    );
    assert!(
        silent_tail < 0.2,
        "after stop + decay the new blocks must be near-silent ({silent_tail})"
    );

    std::fs::remove_dir_all(&target).ok();
}

#[test]
fn preview_hit_sounds_without_sequencer() {
    let target = std::env::temp_dir().join("dg-preview-flow-test");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();

    let xml = generate_test_kit(&target, 44100, TestKitFlavour::Small).unwrap();
    let kit_dir = xml.parent().unwrap().to_path_buf();

    let (mut engine, mut producer) = engine_pair(44100, 256);
    let watch = engine.watch();
    producer
        .push(RtCommand::LoadKit {
            path: kit_dir.display().to_string(),
        })
        .unwrap();
    render_until_kit(&mut engine, &watch, 400);

    // Instrument preview click (Kit tab "Hit" button path).
    producer
        .push(RtCommand::Midi(dg_common::MidiEvent::note_on(36, 114.0, 0)))
        .unwrap();
    let mut audible = 0.0f32;
    for _ in 0..400 {
        let mut l = vec![0.0f32; 512];
        let mut r = vec![0.0f32; 512];
        engine.render_stereo(&mut l, &mut r, &[]);
        audible = audible.max(peak(&l).max(peak(&r)));
        if audible > 0.2 {
            break;
        }
    }
    assert!(audible > 0.2, "preview hit must be audible ({audible})");
    std::fs::remove_dir_all(&target).ok();
}
