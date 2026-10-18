// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless kit renderer: loads any DrumGizmo kit (directory or
//! drumkit.xml), plays every midimapped note through the full engine
//! chain (XML+sample decode → selection → voice mixdown), exercises
//! velocity layers and chokes, and writes the stereo mixdown as a
//! 16-bit WAV.
//!
//! Usage:
//! ```sh
//! cargo run -p dg-engine --example kit_wav -- <kit-dir-or-xml> <out.wav>
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use dg_common::MidiEvent;
use dg_engine::engine_pair;
use dg_kit::parser::parse_midimap_file;
use dg_kit::{KitLoader, LoadedKit};

/// Loads and polls the kit like the tests do; panics with a useful
/// message on failure.
fn load_kit(arg: &str) -> (Arc<LoadedKit>, Vec<dg_kit::MidimapEntry>) {
    let mut entry = PathBuf::from(arg);
    if entry.is_file() {
        entry = entry.parent().map(PathBuf::from).unwrap_or(entry);
    }
    let xml = if entry.join("drumkit.xml").exists() {
        entry.join("drumkit.xml")
    } else {
        entry.clone()
    };
    let samplerate = 44100u32;
    let mut loader = KitLoader::start(xml.to_str().expect("utf-8 path"), samplerate);
    let kit = loop {
        match loader.poll() {
            Some(Ok(kit)) => break kit,
            Some(Err(error)) => panic!("kit load failed: {error:?}"),
            None => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    };
    let midimap_path = kit.basepath.join("midimap.xml");
    let midimap = parse_midimap_file(&midimap_path).unwrap_or_default();
    println!(
        "loaded kit {:?} v{} — {} instruments, {} channels, {} Hz, {} midimap notes",
        kit.name,
        kit.version,
        kit.instruments.len(),
        kit.channels.len(),
        kit.samplerate,
        midimap.len()
    );
    for (index, instrument) in kit.instruments.iter().enumerate() {
        println!(
            "  [{index}] {:<14} samples: {:>3}  chokes: {}",
            instrument.name,
            instrument.samples.len(),
            instrument.chokes.len()
        );
    }
    (kit, midimap)
}

/// Geometry of `render_stereo` calls: (block position, event) pairs in
/// chronological order, plus the per-hit onset report entries.
type Schedule = Vec<(usize, MidiEvent)>;
type ScheduleReport = Vec<(usize, u8, String, f32)>;

/// Schedule: every mapped note gets a soft/medium/loud hit; then a choke
/// row (note on, choke 0.25 s later) rings the singles out again.
/// Returns the events plus the report entries (note-on onsets) laid out
/// on exactly the same timeline, so the reporting pass cannot drift.
fn build_events(midimap: &[dg_kit::MidimapEntry], rate: usize) -> (Schedule, ScheduleReport) {
    let mut events = Vec::new();
    let mut onsets: Vec<(usize, u8, String, f32)> = Vec::new();
    let mut frame = rate / 10; // 0.1 s lead-in
    for entry in midimap {
        let note = entry.note;
        for velocity in [38.0f32, 76.0, 114.0] {
            events.push((frame, MidiEvent::note_on(note, velocity, 0)));
            onsets.push((frame, note, entry.instrument.clone(), velocity));
            frame += rate / 2; // 0.5 s between hits
        }
    }
    for entry in midimap {
        frame += rate; // gap before each choke row hit
        events.push((frame, MidiEvent::note_on(entry.note, 100.0, 0)));
        onsets.push((frame, entry.note, entry.instrument.clone(), 100.0));
        events.push((frame + rate / 4, MidiEvent::choke(entry.note, 0)));
    }
    events.sort_by_key(|(frame, _)| *frame);
    (events, onsets)
}

fn to_s16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * 32_767.0) as i16
}

/// Minimal 16-bit PCM stereo WAV writer (RIFF manual; no dependencies).
fn write_wav(
    path: &std::path::Path,
    rate: usize,
    left: &[f32],
    right: &[f32],
) -> std::io::Result<()> {
    assert_eq!(left.len(), right.len(), "stereo lengths match");
    let data_bytes = left.len() as u32 * 4;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&(rate as u32).to_le_bytes());
    out.extend_from_slice(&((rate as u32) * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for (l, r) in left.iter().zip(right.iter()) {
        out.extend_from_slice(&to_s16(*l).to_le_bytes());
        out.extend_from_slice(&to_s16(*r).to_le_bytes());
    }
    std::fs::write(path, out)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: kit_wav <kit-dir-or-xml> <out.wav>");
        std::process::exit(2);
    }
    let (kit, midimap) = load_kit(&args[1]);
    if midimap.is_empty() {
        eprintln!("kit has no usable midimap.xml — nothing to play");
        std::process::exit(1);
    }
    let rate = kit.samplerate as usize;
    let (mut engine, _producer) = engine_pair(kit.samplerate as u64, 256);
    engine.set_kit(Arc::clone(&kit), Some(&midimap));

    let (events, onsets) = build_events(&midimap, rate);
    let total_frames = events.iter().map(|(frame, _)| *frame).max().unwrap_or(0) + 2 * rate;

    let block = 512usize;
    let mut left = vec![0.0f32; total_frames];
    let mut right = vec![0.0f32; total_frames];
    let mut played = 0usize;
    let mut cursor = 0usize;
    while cursor < total_frames {
        let end = (cursor + block).min(total_frames);
        let mut block_left = vec![0.0f32; end - cursor];
        let mut block_right = vec![0.0f32; end - cursor];
        let mut block_events: Vec<MidiEvent> = Vec::new();
        while played < events.len() && events[played].0 < end {
            let (frame, event) = &events[played];
            let offset = frame - cursor;
            block_events.push(MidiEvent {
                kind: event.kind,
                note: event.note,
                velocity: event.velocity,
                offset,
            });
            played += 1;
        }
        let _ = engine.render_stereo(&mut block_left, &mut block_right, &block_events);
        left[cursor..end].copy_from_slice(&block_left);
        right[cursor..end].copy_from_slice(&block_right);
        cursor = end;
    }

    // Per-hit report: peak around every note-on onset (same timeline
    // the event builder produced, so windows cannot drift).
    let mut clip_frames = 0usize;
    for (l, r) in left.iter().zip(right.iter()) {
        if l.abs() > 1.0 || r.abs() > 1.0 {
            clip_frames += 1;
        }
    }
    for (onset, note, name, velocity) in &onsets {
        let window_end = (*onset + rate / 4).min(total_frames);
        let peak = left[*onset..window_end]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        println!(
            "note {:>3} {:<14} v{:<3} @ {:.2}s -> peak {:.4}",
            note,
            name,
            velocity,
            *onset as f64 / rate as f64,
            peak
        );
    }
    // Tail check after the last choke row: the window between the
    // final onset and its choke must show the hit ringing (peak > 0).
    let last_onset = onsets.last().map(|(onset, ..)| onset).copied().unwrap_or(0);
    let choke_window_end = (last_onset + rate / 4).min(total_frames);
    let peak_at_last_onset = left[last_onset..choke_window_end]
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()));

    let rms: f32 = (left.iter().zip(&right))
        .map(|(l, r)| l * l + r * r)
        .sum::<f32>()
        / total_frames as f32;
    let peak_all = left
        .iter()
        .zip(&right)
        .map(|(l, r)| l.abs().max(r.abs()))
        .fold(0.0f32, f32::max);
    let seconds = total_frames as f64 / rate as f64;
    println!(
        "mixdown: {:.2}s, rms {:.6}, peak {:.4} — valid sample data: {}",
        seconds,
        rms.sqrt(),
        peak_all,
        rms > 0.0
    );
    println!(
        "frames above 1.0 (mixdown clipping): {}, final-onset window peak: {:.4}",
        clip_frames, peak_at_last_onset
    );
    write_wav(std::path::Path::new(&args[2]), rate, &left, &right).expect("write wav");
    println!("wrote {}", args[2]);
}
