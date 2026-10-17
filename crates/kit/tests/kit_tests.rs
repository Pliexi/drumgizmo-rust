// SPDX-License-Identifier: GPL-3.0-or-later
//! Kit crate tests: parsing, loading, decoding, round-trips.
//! These run headless (no sound card required).

use std::path::Path;

use dg_common::rng::Rand;

use dg_kit::decoder::decode_file;
use dg_kit::kitgen::{generate_test_kit, TestKitFlavour};
use dg_kit::loader::{load_kit_blocking, resolve_kit_xml, ProgressShared};
use dg_kit::parser::{parse_drumkit_file, parse_instrument_xml, parse_midimap_xml, MidimapEntry};

fn tempdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dgkittest-{}-{name}", std::process::id(),));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn parser_loads_generated_kit() {
    let dir = tempdir("parser");
    let xml_path = generate_test_kit(&dir, 44100, TestKitFlavour::Small).unwrap();

    let dom = parse_drumkit_file(&xml_path).unwrap();
    assert_eq!(dom.metadata.title, "Rust Test Kit");
    assert_eq!(dom.version, "2.0");
    assert_eq!(dom.samplerate, 44100.0);
    assert_eq!(dom.channels.len(), 8);
    assert_eq!(dom.instruments.len(), 5);

    // hihat choke linkage
    let closed = dom
        .instruments
        .iter()
        .find(|i| i.name == "HiHat Closed")
        .unwrap();
    assert_eq!(closed.chokes.len(), 1);
    assert_eq!(closed.chokes[0].instrument, "HiHat Open");
    assert!((closed.chokes[0].choketime - 68.0).abs() < f64::EPSILON);
    assert_eq!(closed.group, "HiHat");
    // channel mapping
    assert_eq!(closed.channel_map[0].r#in, "Main L");
    assert_eq!(closed.channel_map[0].out, "Hat In");
}

#[test]
fn parser_validates_required_attributes() {
    // Missing required 'name' attribute on channels/channel must error.
    let xml = r#"<drumkit version="2.0"><channels><channel/></channels></drumkit>"#;
    let result = dg_kit::parser::parse_drumkit_xml(xml);
    assert!(result.is_err());

    // Missing instrument file attribute must error.
    let xml =
        r#"<drumkit version="2.0"><instruments><instrument name="x"/></instruments></drumkit>"#;
    assert!(dg_kit::parser::parse_drumkit_xml(xml).is_err());
}

#[test]
fn parser_defaults_match_upstream() {
    // All-optional attrs default like dgxmlparser.cc: version 1.0, rate 44100.
    let xml = r#"<drumkit><channels></channels><instruments></instruments></drumkit>"#;
    let dom = dg_kit::parser::parse_drumkit_xml(xml).unwrap();
    assert_eq!(dom.version, "1.0");
    assert_eq!(dom.samplerate, 44100.0);

    // choketime defaults to 68 ms upstream.
    let xml = r#"<drumkit version="2.0"><instruments><instrument name="a" file="a.xml">
        <chokes><choke instrument="b"/></chokes></instrument></instruments></drumkit>"#;
    let dom = dg_kit::parser::parse_drumkit_xml(xml).unwrap();
    assert!((dom.instruments[0].chokes[0].choketime - 68.0).abs() < f64::EPSILON);

    // filechannel defaults to 1 (mono).
    let xml = r#"<instrument name="a"><samples><sample name="s1">
        <audiofile channel="Main L" file="foo.wav"/></sample></samples></instrument>"#;
    let dom = parse_instrument_xml(xml).unwrap();
    assert_eq!(dom.samples[0].audiofiles[0].filechannel, 1);
}

#[test]
fn parser_instrument_v1_velocity_groups() {
    let xml = r#"<instrument name="snare" version="1.0">
        <channels><channel name="Main L"/></channels>
        <samples>
          <sample name="lo"><audiofile channel="Main L" file="lo.wav"/></sample>
          <sample name="hi"><audiofile channel="Main L" file="hi.wav"/></sample>
        </samples>
        <velocities>
          <velocity lower="0" upper="0.5"><sampleref probability="1.0" name="lo"/></velocity>
          <velocity lower="0.5" upper="1"><sampleref probability="1.0" name="hi"/></velocity>
        </velocities>
    </instrument>"#;
    let dom = parse_instrument_xml(xml).unwrap();
    assert_eq!(dom.velocities.len(), 2);
    assert_eq!(dom.velocities[0].samplerefs[0].name, "lo");
}

#[test]
fn midimap_parsing_skips_broken_entries() {
    let xml = r#"<midimap>
        <map note="36" instr="Kick"/>
        <map instr="Broken"/>
        <map note="999" instr="OOR"/>
        <map note="47" instr=" Snip "/>
        <map note="48" instr=""/>
    </midimap>"#;
    let entries = parse_midimap_xml(xml).unwrap();
    assert_eq!(
        entries,
        vec![
            MidimapEntry {
                note: 36,
                instrument: "Kick".into()
            },
            // 999 is out of range: upstream skips with as_int() failure path
            MidimapEntry {
                note: 47,
                instrument: " Snip ".into()
            },
            // empty instr skipped
        ]
    );
}

#[test]
fn generated_kit_loads_completely() {
    let dir = tempdir("load");
    generate_test_kit(&dir, 44100, TestKitFlavour::Small).unwrap();

    let progress = ProgressShared::default();
    let kit = load_kit_blocking(&dir, 48000, &progress).expect("kit should load");

    // kit channel resolution
    assert_eq!(kit.channels.len(), 8);
    assert_eq!(kit.channels[0], "Kick In");
    assert_eq!(kit.instruments.len(), 5);

    // Kick must have 6 samples, stereo files resampled to engine rate 48000
    let kick = &kit.instruments[0];
    assert_eq!(kick.name, "Kick");
    assert_eq!(kick.samples.len(), 6);
    let file = &kit.files()[0];
    assert_eq!(file.samplerate, 48000);
    assert_eq!(file.data.len(), 2);
    assert!(file.data[0].len() > 1000);

    // instrument channels map onto kit channels
    let idx = kick
        .channels
        .iter()
        .position(|c| c.name == "Main L")
        .unwrap();
    assert_eq!(kick.channels[idx].kit_channel, 0);

    // choke resolution
    let closed = kit
        .instruments
        .iter()
        .find(|i| i.name == "HiHat Closed")
        .unwrap();
    assert_eq!(closed.group, "HiHat");
    assert_eq!(closed.chokes.len(), 1);
    assert_eq!(closed.chokes[0].choketime_ms, 68.0);
    assert_eq!(
        closed.chokes[0].instrument_id,
        kit.instruments
            .iter()
            .position(|i| i.name == "HiHat Open")
            .unwrap()
    );

    // midi map builder
    let midimap = dg_kit::parser::parse_midimap_file(&dir.join("midimap.xml")).unwrap();
    let map = kit.build_midi_map(&midimap);
    assert_eq!(map[36], vec![0]); // Kick
    assert_eq!(map[38], vec![1]); // Snare
    assert_eq!(map[42], vec![2]); // HiHat Closed
    assert!(map[57].is_empty());
}

#[test]
fn generated_vintage_kit_loads_velocity_groups() {
    let dir = tempdir("load_v1");
    generate_test_kit(&dir, 44100, TestKitFlavour::Vintage).unwrap();

    let progress = ProgressShared::default();
    let kit = load_kit_blocking(&dir, 44100, &progress).unwrap();

    assert_eq!(kit.instruments.len(), 2);
    let kick = &kit.instruments[0];
    assert!(dg_kit::dom::uses_velocities(&kick.version));
    assert_eq!(kick.velocity_groups.len(), 3);
    assert!((kick.velocity_groups[1].upper - 0.7).abs() < 1e-6);
    assert_eq!(kick.velocity_groups[2].samples, vec![4, 5]);
}

#[test]
fn missing_kit_path_errors() {
    let progress = ProgressShared::default();
    let ghost = std::env::temp_dir().join(format!("no-such-kit-{}", std::process::id()));
    assert!(load_kit_blocking(&ghost, 48000, &progress).is_err());
}

#[test]
fn resolve_kit_xml_supports_directory_and_file() {
    let dir = tempdir("resolve");
    let xml = generate_test_kit(&dir, 44100, TestKitFlavour::Small).unwrap();
    let (base1, file1) = resolve_kit_xml(&dir).unwrap();
    assert_eq!(file1, xml);
    assert_eq!(base1, lexed(&dir));
    let (base2, file2) = resolve_kit_xml(&xml).unwrap();
    assert_eq!(file2, xml);
    assert_eq!(base2, base1);
}

fn lexed(dir: &Path) -> std::path::PathBuf {
    dg_kit::loader::lex_normalise(dir)
}

#[test]
fn decoded_wavs_are_deterministic_and_stereo() {
    let dir = tempdir("decode");
    let xml = generate_test_kit(&dir, 44100, TestKitFlavour::Small).unwrap();
    let dom = parse_drumkit_file(&xml).unwrap();
    let kick_xml = dir.join(&dom.instruments[0].file);
    let inst = dg_kit::parser::parse_instrument_file(&kick_xml).unwrap();
    let sample = &inst.samples[0];
    let file = dir.join(&sample.audiofiles[0].file);

    let decoded = decode_file(&file, Some(48000.0)).unwrap();
    assert_eq!(decoded.samplerate, 48000);
    assert_eq!(decoded.file_channels, 2);
    assert!(decoded.data[1].len() > 1000);
    // both channels decode to something non-identical (detuned)
    assert_ne!(decoded.data[0][100], decoded.data[1][100]);

    let again = decode_file(&file, Some(48000.0)).unwrap();
    assert_eq!(decoded.data[0][100], again.data[0][100]);
}

#[test]
fn decode_resamples_44100_to_48000() {
    let dir = tempdir("resample");
    let xml = generate_test_kit(&dir, 44100, TestKitFlavour::Small).unwrap();
    let dom = parse_drumkit_file(&xml).unwrap();
    let kick_xml = dir.join(&dom.instruments[0].file);
    let inst = dg_kit::parser::parse_instrument_file(&kick_xml).unwrap();
    let file = dir.join(&inst.samples[0].audiofiles[0].file);

    let original = decode_file(&file, None).unwrap();
    let converted = decode_file(&file, Some(48000.0)).unwrap();
    assert_eq!(original.samplerate, 44100);
    assert_eq!(converted.samplerate, 48000);
    // 0.4s at 44100 -> 17640 frames; at 48000 -> 19200 frames
    assert_eq!(original.data[0].len(), 17_640);
    assert_eq!(converted.data[0].len(), 19_200);
}

#[test]
fn random_engine_is_deterministic() {
    let mut a = Rand::new(12345);
    let mut b = Rand::new(12345);
    for _ in 0..100 {
        assert_eq!(a.float_in_range(0.0, 1.0), b.float_in_range(0.0, 1.0));
    }
}

#[test]
fn generated_kit_two_runs_identical_bytes() {
    let dir1 = tempdir("det1");
    let dir2 = tempdir("det2");
    let xml1 = generate_test_kit(&dir1, 22050, TestKitFlavour::Small).unwrap();
    let xml2 = generate_test_kit(&dir2, 22050, TestKitFlavour::Small).unwrap();
    assert_eq!(std::fs::read(&xml1).unwrap(), std::fs::read(&xml2).unwrap());
}

#[cfg(test)]
mod fixture {
    /// Writes the CI/"committed" test kit; called by `kitgen` binary.
    pub fn write_committed_kit(dir: &std::path::Path) -> std::path::PathBuf {
        dg_kit::kitgen::generate_test_kit(dir, 44100, dg_kit::kitgen::TestKitFlavour::Small)
            .expect("test kit generation")
    }
}
