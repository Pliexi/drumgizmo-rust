// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic synthetic test-kit generator.
//!
//! This is the port of `test/drumkit_creator.cc`: it writes a complete
//! drumkit directory (drumkit.xml, one instrument XML per instrument,
//! midimap.xml and 16-bit PCM WAV files), every byte derived from a fixed
//! seed so tests and CI never depend on randomness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dg_common::rng::Rand;

/// Kit layout variants that the generator can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestKitFlavour {
    /// Kick/Snare/HiHat pair/Tom with per-sample power (>= 2.0 semantics).
    #[default]
    Small,
    /// Kick/Snare with v1.0 velocity groups.
    Vintage,
}

/// Writes a test kit into `dir` and returns the drumkit.xml path.
pub fn generate_test_kit(
    dir: &Path,
    samplerate: u32,
    flavour: TestKitFlavour,
) -> Result<PathBuf, std::io::Error> {
    std::fs::create_dir_all(dir)?;

    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Kick,
        Snare,
        HiHatClosed,
        HiHatOpen,
        Tom,
    }

    struct SampleSpec {
        name: &'static str,
        power: f64,
        kind: Kind,
    }
    struct InstrumentSpec {
        name: &'static str,
        xml_file: &'static str,
        kit_channel_left: &'static str,
        kit_channel_right: &'static str,
        group: &'static str,
        chokes: &'static [(&'static str, f64)],
        samples: Vec<SampleSpec>,
    }

    /// Synthesize one stereo sample; deterministic by design.
    fn synth(kind: Kind, seed: u64, frames: usize, rate: u32) -> [Vec<f32>; 2] {
        let mut rng = Rand::new(seed);
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let detune = f64::from(rng.float_in_range(-0.004, 0.004));
        for frame in 0..frames {
            let t = frame as f64 / rate as f64;
            let (l, r) = match kind {
                Kind::Kick => {
                    let freq = 50.0 * (-2.2 * t).exp() + 45.0;
                    let body =
                        (2.0 * std::f64::consts::PI * freq * frame as f64 / rate as f64).sin();
                    let thump = (-14.0 * t).exp();
                    let click = if t < 0.01 {
                        (-t * 300.0).exp() * 0.4
                    } else {
                        0.0
                    };
                    let value = body * thump + click;
                    (value * 0.9, value * 0.82)
                }
                Kind::Snare => {
                    let noise = rng.float_in_range(-1.0, 1.0) as f64;
                    let ring0 =
                        (2.0 * std::f64::consts::PI * 196.0 * frame as f64 / rate as f64).sin();
                    let ring1 =
                        (2.0 * std::f64::consts::PI * 185.0 * frame as f64 / rate as f64).sin();
                    let value = (ring0 * 0.55 + ring1 * 0.45) * (-22.0 * t).exp()
                        + noise * (-26.0 * t).exp() * 0.8;
                    (value, value * 0.94)
                }
                Kind::HiHatClosed => {
                    let noise = rng.float_in_range(-1.0, 1.0) as f64;
                    let value = noise * (-45.0 * t).exp() * 0.85;
                    (value, value)
                }
                Kind::HiHatOpen => {
                    let noise = rng.float_in_range(-1.0, 1.0) as f64;
                    let value = noise * (-9.0 * t).exp() * 0.5;
                    (value, value * 0.92)
                }
                Kind::Tom => {
                    let freq = 128.0 + 42.0 * (-5.0 * t).exp();
                    let value = (2.0 * std::f64::consts::PI * freq * frame as f64 / rate as f64)
                        .sin()
                        * (-9.0 * t).exp()
                        * 0.95;
                    (value * 0.95, value * 0.8)
                }
            };
            left.push(l as f32);
            right.push((r * (1.0 + detune)) as f32);
        }
        [left, right]
    }

    // ------------------------------------------------------------------
    // Instrument tables
    // ------------------------------------------------------------------

    /// Fixed sample tables: 3 velocity layers x 2 round-robin variants.
    const LAYERS6: [&str; 6] = [
        "stroke00", "stroke01", "stroke10", "stroke11", "stroke20", "stroke21",
    ];
    const KICK_POWERS: [f64; 6] = [0.15, 0.15, 0.5, 0.5, 0.9, 0.9];
    const SNARE_POWERS: [f64; 6] = [0.2, 0.2, 0.55, 0.55, 0.85, 0.85];

    let small: Vec<InstrumentSpec> = vec![
        InstrumentSpec {
            name: "Kick",
            xml_file: "kick.xml",
            kit_channel_left: "Kick In",
            kit_channel_right: "Kick Out",
            group: "",
            chokes: &[],
            samples: LAYERS6
                .iter()
                .enumerate()
                .map(|(index, name)| SampleSpec {
                    name,
                    power: KICK_POWERS[index],
                    kind: Kind::Kick,
                })
                .collect(),
        },
        InstrumentSpec {
            name: "Snare",
            xml_file: "snare.xml",
            kit_channel_left: "Snare In",
            kit_channel_right: "Snare Out",
            group: "",
            chokes: &[],
            samples: LAYERS6
                .iter()
                .enumerate()
                .map(|(index, name)| SampleSpec {
                    name,
                    power: SNARE_POWERS[index],
                    kind: Kind::Snare,
                })
                .collect(),
        },
        InstrumentSpec {
            name: "HiHat Closed",
            xml_file: "hihat_closed.xml",
            kit_channel_left: "Hat In",
            kit_channel_right: "Hat Out",
            group: "HiHat",
            chokes: &[("HiHat Open", 68.0)],
            samples: vec![
                SampleSpec {
                    name: "stroke00",
                    power: 0.1,
                    kind: Kind::HiHatClosed,
                },
                SampleSpec {
                    name: "stroke10",
                    power: 0.4,
                    kind: Kind::HiHatClosed,
                },
            ],
        },
        InstrumentSpec {
            name: "HiHat Open",
            xml_file: "hihat_open.xml",
            kit_channel_left: "Hat In",
            kit_channel_right: "Hat Out",
            group: "HiHat",
            chokes: &[("HiHat Closed", 68.0)],
            samples: vec![
                SampleSpec {
                    name: "stroke00",
                    power: 0.7,
                    kind: Kind::HiHatOpen,
                },
                SampleSpec {
                    name: "stroke10",
                    power: 1.0,
                    kind: Kind::HiHatOpen,
                },
            ],
        },
        InstrumentSpec {
            name: "Tom",
            xml_file: "tom.xml",
            kit_channel_left: "Tom In",
            kit_channel_right: "Tom Out",
            group: "",
            chokes: &[],
            samples: vec![
                SampleSpec {
                    name: "stroke00",
                    power: 0.3,
                    kind: Kind::Tom,
                },
                SampleSpec {
                    name: "stroke10",
                    power: 0.75,
                    kind: Kind::Tom,
                },
            ],
        },
    ];

    let vintage: Vec<InstrumentSpec> = vec![
        InstrumentSpec {
            name: "Kick",
            xml_file: "kick.xml",
            kit_channel_left: "Kick In",
            kit_channel_right: "Kick Out",
            group: "",
            chokes: &[],
            samples: LAYERS6
                .iter()
                .enumerate()
                .map(|(index, name)| SampleSpec {
                    name,
                    power: KICK_POWERS[index],
                    kind: Kind::Kick,
                })
                .collect(),
        },
        InstrumentSpec {
            name: "Snare",
            xml_file: "snare.xml",
            kit_channel_left: "Snare In",
            kit_channel_right: "Snare Out",
            group: "",
            chokes: &[],
            samples: LAYERS6
                .iter()
                .enumerate()
                .map(|(index, name)| SampleSpec {
                    name,
                    power: SNARE_POWERS[index],
                    kind: Kind::Snare,
                })
                .collect(),
        },
    ];

    let instruments = match flavour {
        TestKitFlavour::Small => &small,
        TestKitFlavour::Vintage => &vintage,
    };

    let frames = (samplerate as f64 * 0.4) as usize; // 400 ms samples

    // ------------------------------------------------------------------
    // WAV files
    // ------------------------------------------------------------------
    let mut wave_files: BTreeMap<String, (Kind, u64)> = BTreeMap::new();
    for instrument in instruments {
        for sample in &instrument.samples {
            let file = wav_file_name(instrument.name, sample.name);
            wave_files
                .entry(file)
                .or_insert((sample.kind, seed_from(instrument.name, sample.name)));
        }
    }

    for (file, (kind, seed)) in &wave_files {
        let planes = synth(*kind, *seed, frames, samplerate);
        write_wav(&dir.join(file), samplerate, &[&planes[0], &planes[1]])?;
    }

    // ------------------------------------------------------------------
    // drumkit.xml
    // ------------------------------------------------------------------
    let kit_version = match flavour {
        TestKitFlavour::Small => "2.0",
        TestKitFlavour::Vintage => "1.0",
    };

    let mut kit_channels: Vec<&str> = Vec::new();
    for instrument in instruments {
        for channel in [instrument.kit_channel_left, instrument.kit_channel_right] {
            if !kit_channels.contains(&channel) {
                kit_channels.push(channel);
            }
        }
    }

    let mut kit_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <drumkit name=\"Rust Test Kit\" version=\"{kit_version}\" \
         description=\"Generated synthetic drumgizmo test kit\" \
         samplerate=\"{samplerate}\">\n\
         <metadata>\n\
           <version>{kit_version}</version>\n\
           <title>Rust Test Kit</title>\n\
           <description>Generated synthetic drumgizmo test kit</description>\n\
           <license>GPL-3.0-or-later</license>\n\
           <author>drumgizmo-rust</author>\n\
           <email>noreply@localhost</email>\n\
           <website>https://localhost</website>\n\
           <defaultmidimap src=\"midimap.xml\"/>\n\
         </metadata>\n\
         <channels>\n"
    );
    for channel in &kit_channels {
        kit_xml.push_str(&format!("  <channel name=\"{channel}\"/>\n"));
    }
    kit_xml.push_str("</channels>\n<instruments>\n");

    for instrument in instruments {
        let group_attr = format!(" group=\"{}\"", instrument.group);
        let group_attr = if instrument.group.is_empty() {
            ""
        } else {
            group_attr.as_str()
        };
        kit_xml.push_str(&format!(
            "  <instrument name=\"{}\" file=\"{}\"{}>\n",
            instrument.name, instrument.xml_file, group_attr
        ));
        kit_xml.push_str(&format!(
            "    <channelmap in=\"Main L\" out=\"{}\"/>\n",
            instrument.kit_channel_left
        ));
        kit_xml.push_str(&format!(
            "    <channelmap in=\"Main R\" out=\"{}\"/>\n",
            instrument.kit_channel_right
        ));
        if !instrument.chokes.is_empty() {
            kit_xml.push_str("    <chokes>\n");
            for (target, choketime) in instrument.chokes {
                kit_xml.push_str(&format!(
                    "      <choke instrument=\"{target}\" choketime=\"{choketime}\"/>\n"
                ));
            }
            kit_xml.push_str("    </chokes>\n");
        }
        kit_xml.push_str("  </instrument>\n");
    }
    kit_xml.push_str("</instruments>\n</drumkit>\n");
    std::fs::write(dir.join("drumkit.xml"), kit_xml)?;

    // ------------------------------------------------------------------
    // Instrument XMLs
    // ------------------------------------------------------------------
    for instrument in instruments {
        let instrument_version = match flavour {
            TestKitFlavour::Small => "2.0",
            TestKitFlavour::Vintage => "1.0",
        };
        let mut xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <instrument name=\"{}\" version=\"{instrument_version}\">\n\
             <channels>\n  <channel name=\"Main L\"/>\n  <channel name=\"Main R\"/>\n</channels>\n\
             <samples>\n",
            instrument.name
        );
        for sample in &instrument.samples {
            if instrument_version == "2.0" {
                xml.push_str(&format!(
                    "  <sample name=\"{}\" power=\"{}\" normalized=\"true\">\n",
                    sample.name, sample.power
                ));
            } else {
                xml.push_str(&format!("  <sample name=\"{}\">\n", sample.name));
            }
            let file = wav_file_name(instrument.name, sample.name);
            xml.push_str(&format!(
                "    <audiofile channel=\"Main L\" file=\"{file}\" filechannel=\"1\"/>\n"
            ));
            xml.push_str(&format!(
                "    <audiofile channel=\"Main R\" file=\"{file}\" filechannel=\"2\"/>\n"
            ));
            xml.push_str("  </sample>\n");
        }
        xml.push_str("</samples>\n");
        if instrument_version == "1.0" {
            // Group samples into three velocity ranges.
            xml.push_str("<velocities>\n");
            let groups: [(f64, f64, [usize; 2]); 3] =
                [(0.0, 0.4, [0, 1]), (0.4, 0.7, [2, 3]), (0.7, 1.0, [4, 5])];
            for (lower, upper, members) in groups {
                xml.push_str(&format!("  <velocity lower=\"{lower}\" upper=\"{upper}\">"));
                for member in members {
                    if let Some(sample) = instrument.samples.get(member) {
                        xml.push_str(&format!(
                            "\n    <sampleref probability=\"1.0\" name=\"{}\"/>",
                            sample.name
                        ));
                    }
                }
                xml.push_str("\n  </velocity>\n");
            }
            xml.push_str("</velocities>\n");
        }
        xml.push_str("</instrument>\n");
        std::fs::write(dir.join(instrument.xml_file), xml)?;
    }

    // ------------------------------------------------------------------
    // midimap.xml
    // ------------------------------------------------------------------
    let mut midimap = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<midimap>\n");
    for (index, instrument) in instruments.iter().enumerate() {
        let note = [36u8, 38, 42, 46, 45][index];
        midimap.push_str(&format!(
            "  <map note=\"{note}\" instr=\"{}\"/>\n",
            instrument.name
        ));
    }
    midimap.push_str("</midimap>\n");
    std::fs::write(dir.join("midimap.xml"), midimap)?;

    Ok(dir.join("drumkit.xml"))
}

/// WAV file name for a sample: lowercase instrument name + sample name.
fn wav_file_name(instrument: &str, sample: &str) -> String {
    // e.g. "hihat_closed_stroke10" -> never depends on unicode casing.
    let mut name = String::with_capacity(instrument.len() + sample.len() + 1);
    for c in instrument.chars() {
        if c == ' ' {
            name.push('_');
        } else if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        }
    }
    name.push('_');
    name.push_str(sample);
    name.push_str(".wav");
    name
}

fn write_wav(path: &Path, rate: u32, planes: &[&[f32]]) -> std::io::Result<()> {
    let channels = planes.len() as u16;
    let frames = planes[0].len() as u32;
    let bits_per_sample: u16 = 16;
    let block_align: u16 = channels * bits_per_sample / 8;
    let byte_rate = rate * u32::from(block_align);
    let data_len = frames * u32::from(block_align);

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());

    for frame in 0..frames {
        for plane in planes {
            let value = (plane[frame as usize].clamp(-1.0, 1.0) * 32767.0) as i16;
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    std::fs::write(path, out)
}

/// Deterministic 64-bit seed from instrument/sample names.
fn seed_from(instrument: &str, sample: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in instrument.as_bytes().iter().chain(sample.as_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_file_names_are_deterministic() {
        use super::wav_file_name;
        assert_eq!(
            wav_file_name("HiHat Closed", "stroke10"),
            "hihat_closed_stroke10.wav"
        );
        assert_eq!(seed_from("Kick", "stroke10"), seed_from("Kick", "stroke10"));
    }
}
