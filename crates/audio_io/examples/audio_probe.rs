// SPDX-License-Identifier: GPL-3.0-or-later
//! Hardware probe: builds the real cpal output stream around an engine
//! pair, loads the kit given as the first argument and triggers one hit;
//! prints the measured stream stats. Developer debugging helper only.

use std::sync::{Arc, Mutex};

use dg_audio_io::OutputSetup;
use dg_common::{RtCommand, SharedProducer};

fn main() {
    let Ok(setup) = OutputSetup::detect() else {
        eprintln!("probe: no output device/config on this host");
        return;
    };
    println!(
        "probe: device config {} Hz / {} ch ({:?})",
        setup.config.sample_rate, setup.config.channels, setup.sample_format
    );

    let (engine, producer) = dg_engine::engine_pair(u64::from(setup.config.sample_rate), 256);
    let watch = engine.watch();
    let producer: SharedProducer = Arc::new(Mutex::new(producer));
    let Some(kit) = std::env::args().nth(1) else {
        eprintln!("probe: usage: audio_probe <kit-dir>");
        return;
    };
    let _ = producer
        .lock()
        .unwrap()
        .push(RtCommand::LoadKit { path: kit });
    let stream = match setup.build(engine) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("probe: stream failed: {error}");
            return;
        }
    };
    // Wait for the kit install, THEN hit — commands consumed before the
    // kit lands are dropped by the engine (no voices to trigger).
    for _ in 0..300 {
        if watch
            .kit_generation
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = producer
        .lock()
        .unwrap()
        .push(RtCommand::Midi(dg_common::MidiEvent::note_on(36, 116.0, 0)));
    std::thread::sleep(std::time::Duration::from_millis(800));
    let _ = producer.lock().unwrap().push(RtCommand::Panic);
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        println!(
            "probe: frames={} peak={:.4} failed={} kit_gen={} kit_err={}",
            stream.stats.frames(),
            stream.stats.take_peak(),
            stream.stats.failed(),
            watch
                .kit_generation
                .load(std::sync::atomic::Ordering::Acquire),
            watch.kit_error.load(std::sync::atomic::Ordering::Acquire),
        );
    }
}
