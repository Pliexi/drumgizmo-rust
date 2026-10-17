// SPDX-License-Identifier: GPL-3.0-or-later
//! `kitgen` binary: writes a synthetic DrumGizmo test kit into a directory.
//!
//! Usage: `kitgen [OPTION]... OUTPUT_DIR`
//!
//! Ported from `test/drumkit_creator.cc`.

use dg_kit::kitgen::{generate_test_kit, TestKitFlavour};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut samplerate = 44100u32;
    let mut flavour = TestKitFlavour::Small;
    let mut output = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--samplerate" => {
                samplerate = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(samplerate);
            }
            "--flavour" => {
                flavour = match args.next().as_deref() {
                    Some("vintage") => TestKitFlavour::Vintage,
                    _ => TestKitFlavour::Small,
                };
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            other => output = Some(other.to_string()),
        }
    }

    let Some(output) = output else {
        eprintln!("kitgen: no output directory given.");
        print_help();
        std::process::exit(2);
    };

    let dir = std::path::PathBuf::from(output);
    match generate_test_kit(&dir, samplerate, flavour) {
        Ok(xml) => println!("Test kit written to {}", xml.display()),
        Err(e) => {
            eprintln!("kitgen: {e}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        "kitgen — generate a deterministic DrumGizmo test kit\n\
         usage: kitgen [--samplerate RATE] [--flavour small|vintage] OUTPUT_DIR"
    );
}
