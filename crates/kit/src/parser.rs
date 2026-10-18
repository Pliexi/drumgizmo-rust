// SPDX-License-Identifier: GPL-3.0-or-later
//! A port of `src/dgxmlparser.cc` and `src/midimapparser.cc`.
//!
//! Attribute handling follows upstream:
//!
//! * required attributes and elements hard-fail the whole parse
//! * optional attributes fall back to exactly the same defaults
//!   (`drumkit version="1.0"`, `samplerate="44100.0"`,
//!   `choketime="68"` milliseconds, `filechannel="1"` mono, ...)
//! * `<metadata>` children overwrite the fallback `name`/`description`
//!   attributes of the `<drumkit>` root node, matching `dgxmlparser.cc`

use std::path::Path;

use roxmltree::Node;

use crate::dom::{
    has_power, uses_velocities, AudioFileDom, ChannelDom, ChannelMapDom, ChokeDom, ClickMapDom,
    DrumkitDom, InstrumentChannelDom, InstrumentDom, InstrumentRefDom, MainState, MetadataDom,
    ParseError, SampleDom, SampleRefDom, VelocityDom,
};

/// Load and parse a `drumkit.xml`.
pub fn parse_drumkit_file(filename: &Path) -> Result<DrumkitDom, ParseError> {
    let xml = read(filename)?;
    parse_drumkit_xml(&xml)
}

/// Load and parse an instrument XML file.
pub fn parse_instrument_file(filename: &Path) -> Result<InstrumentDom, ParseError> {
    let xml = read(filename)?;
    parse_instrument_xml(&xml)
}

/// Load a `midimap.xml` file.
pub fn parse_midimap_file(filename: &Path) -> Result<Vec<MidimapEntry>, ParseError> {
    let xml = read(filename)?;
    parse_midimap_xml(&xml)
}

fn read(filename: &Path) -> Result<String, ParseError> {
    std::fs::read_to_string(filename)
        .map_err(|e| ParseError(format!("Cannot open '{}': {}", filename.display(), e)))
}

fn xml_doc<'a>(xml: &'a str, label: &'a str) -> Result<roxmltree::Document<'a>, ParseError> {
    roxmltree::Document::parse(xml)
        .map_err(|e| ParseError(format!("{label}: XML parse error: {e}")))
}

/// Parse a `drumkit.xml` payload.
pub fn parse_drumkit_xml(xml: &str) -> Result<DrumkitDom, ParseError> {
    let doc = xml_doc(xml, "drumkit")?;
    let drumkit = only(doc.root(), "drumkit")
        .ok_or_else(|| ParseError("No <drumkit> root element".into()))?;

    // Defaults from upstream: version "1.0", samplerate 44100.
    let mut dom = DrumkitDom {
        version: attr(drumkit, "version").unwrap_or("1.0").to_string(),
        samplerate: opt_f64(drumkit, "samplerate", 44100.0),
        metadata: MetadataDom::default(),
        instruments: Vec::new(),
        channels: Vec::new(),
    };

    // Old-style name/description attributes as metadata fallback.
    dom.metadata.title = attr(drumkit, "name").unwrap_or("").to_string();
    dom.metadata.description = attr(drumkit, "description").unwrap_or("").to_string();

    let meta = &mut dom.metadata;
    if let Some(metadata) = only(drumkit, "metadata") {
        meta.version = element_text(metadata, "version");
        meta.title = element_text(metadata, "title");
        meta.description = element_text(metadata, "description");
        meta.license = element_text(metadata, "license");
        meta.notes = element_text(metadata, "notes");
        meta.author = element_text(metadata, "author");
        meta.email = element_text(metadata, "email");
        meta.website = element_text(metadata, "website");

        if let Some(logo) = only(metadata, "logo") {
            meta.logo = attr(logo, "src").unwrap_or("").to_string();
        }
        if let Some(image) = only(metadata, "image") {
            meta.image = attr(image, "src").unwrap_or("").to_string();
            meta.image_map = attr(image, "map").unwrap_or("").to_string();
            for clickmap in children_of(image, "clickmap") {
                meta.clickmaps.push(ClickMapDom {
                    instrument: req(clickmap, "instrument").unwrap_or_default().to_string(),
                    colour: attr(clickmap, "colour").unwrap_or("").to_string(),
                });
            }
        }
        if let Some(midimap) = only(metadata, "defaultmidimap") {
            meta.default_midimap_file = attr(midimap, "src").unwrap_or("").to_string();
        }
    }

    // Channels; `name` is required.
    if let Some(channels) = only(drumkit, "channels") {
        for channel in children_of(channels, "channel") {
            dom.channels.push(ChannelDom {
                name: req(channel, "name")?.to_string(),
            });
        }
    }

    // Instruments: required name+file, optional group/channelmap/chokes.
    if let Some(instruments) = only(drumkit, "instruments") {
        for instrument in children_of(instruments, "instrument") {
            let mut reference = InstrumentRefDom {
                name: req(instrument, "name")?.to_string(),
                file: req(instrument, "file")?.to_string(),
                group: attr(instrument, "group").unwrap_or("").to_string(),
                channel_map: Vec::new(),
                chokes: Vec::new(),
            };

            for cmap in children_of(instrument, "channelmap") {
                reference.channel_map.push(ChannelMapDom {
                    r#in: req(cmap, "in").unwrap_or_default().to_string(),
                    out: req(cmap, "out").unwrap_or_default().to_string(),
                    main: parse_main(cmap),
                });
            }

            let chokes_count = children_of(instrument, "chokes").count();
            if chokes_count > 1 {
                return Err(ParseError(
                    "At most one 'chokes' node allowed pr. instrument.".into(),
                ));
            } else if chokes_count == 1 {
                // unwrap is safe: chokes_count == 1 above.
                let node = children_of(instrument, "chokes").next().unwrap();
                for choke in children_of(node, "choke") {
                    // Upstream default: 68 ms.
                    reference.chokes.push(ChokeDom {
                        instrument: req(choke, "instrument")?.to_string(),
                        choketime: opt_f64(choke, "choketime", 68.0),
                    });
                }
            }

            dom.instruments.push(reference);
        }
    }

    Ok(dom)
}

/// Parse an instrument XML payload (`<instrument>` root).
pub fn parse_instrument_xml(xml: &str) -> Result<InstrumentDom, ParseError> {
    let doc = xml_doc(xml, "instrument")?;
    let instrument = only(doc.root(), "instrument")
        .ok_or_else(|| ParseError("No <instrument> root element".into()))?;

    let mut dom = InstrumentDom {
        name: req(instrument, "name")?.to_string(),
        version: attr(instrument, "version").unwrap_or("1.0").to_string(),
        description: attr(instrument, "description").unwrap_or("").to_string(),
        samples: Vec::new(),
        instrument_channels: Vec::new(),
        velocities: Vec::new(),
    };

    let uses_velocities = uses_velocities(&dom.version);
    let has_power = has_power(&dom.version);

    if let Some(channels) = only(instrument, "channels") {
        for channel in children_of(channels, "channel") {
            dom.instrument_channels.push(InstrumentChannelDom {
                name: req(channel, "name")?.to_string(),
                main: parse_main(channel),
            });
        }
    }

    if let Some(samples) = only(instrument, "samples") {
        for sample in children_of(samples, "sample") {
            let mut sample_dom = SampleDom {
                name: req(sample, "name")?.to_string(),
                power: 0.0,
                normalized: false,
                audiofiles: Vec::new(),
            };
            if has_power {
                // Power is required from v2.0 instruments on.
                sample_dom.power = opt_f64(sample, "power", 0.0);
                sample_dom.normalized = attr(sample, "normalized") == Some("true");
            }
            for audiofile in children_of(sample, "audiofile") {
                sample_dom.audiofiles.push(AudioFileDom {
                    instrument_channel: req(audiofile, "channel")?.to_string(),
                    file: req(audiofile, "file")?.to_string(),
                    // Defaults to channel 1 in mono (1-based) as upstream.
                    filechannel: opt_usize(audiofile, "filechannel", 1),
                });
            }
            dom.samples.push(sample_dom);
        }
    }

    if uses_velocities {
        if let Some(velocities) = only(instrument, "velocities") {
            for velocity in children_of(velocities, "velocity") {
                let mut velocity_dom = VelocityDom {
                    lower: opt_f64(velocity, "lower", 0.0),
                    upper: opt_f64(velocity, "upper", 0.0),
                    samplerefs: Vec::new(),
                };
                for sampleref in children_of(velocity, "sampleref") {
                    velocity_dom.samplerefs.push(SampleRefDom {
                        probability: opt_f64(sampleref, "probability", 0.0),
                        name: req(sampleref, "name")?.to_string(),
                    });
                }
                dom.velocities.push(velocity_dom);
            }
        }
    }

    Ok(dom)
}

/// A single `<map note="..." instr="..."/>` mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidimapEntry {
    /// MIDI note (raw 0..=127 value).
    pub note: u8,
    /// Instrument name; may contain whitespace exactly as found in the map.
    pub instrument: String,
}

/// Parse `midimap.xml` content. Upstream skips entries with a missing/unusable
/// `note` or an empty `instr`; we do the same instead of failing.
pub fn parse_midimap_xml(xml: &str) -> Result<Vec<MidimapEntry>, ParseError> {
    let doc = xml_doc(xml, "midimap")?;
    let mut entries = Vec::new();
    for midimap in children_of(doc.root(), "midimap") {
        for map in children_of(midimap, "map") {
            let note_text = map.attribute("note").unwrap_or("");
            // Upstream uses as_int() with a bad_value fallback and skips those.
            let Ok(note) = note_text.trim().parse::<i64>() else {
                continue;
            };
            if !(0..=127).contains(&note) {
                continue;
            }
            let instrument = map.attribute("instr").unwrap_or("").to_string();
            if instrument.trim().is_empty() {
                continue;
            }
            entries.push(MidimapEntry {
                note: note as u8,
                instrument,
            });
        }
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// internal helpers
// ---------------------------------------------------------------------------

fn children_of<'a, 'input>(
    node: Node<'a, 'input>,
    tag: &'a str,
) -> impl Iterator<Item = Node<'a, 'input>> + use<'a, 'input> {
    node.children().filter(move |c| c.tag_name().name() == tag)
}

fn only<'a, 'input>(node: Node<'a, 'input>, tag: &'a str) -> Option<Node<'a, 'input>> {
    node.children().find(|c| c.tag_name().name() == tag)
}

fn attr<'a>(n: Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.attribute(name)
}

fn req<'a>(n: Node<'a, 'a>, name: &str) -> Result<&'a str, ParseError> {
    n.attribute(name).ok_or_else(|| {
        ParseError(format!(
            "Attribute {name} not found in <{}>",
            n.tag_name().name()
        ))
    })
}

fn opt_f64(n: Node, name: &str, default: f64) -> f64 {
    n.attribute(name)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn opt_usize(n: Node, name: &str, default: usize) -> usize {
    n.attribute(name)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn element_text(n: Node, name: &str) -> String {
    match only(n, name) {
        Some(c) => c.text().unwrap_or("").to_string(),
        None => String::new(),
    }
}

fn parse_main(n: Node) -> MainState {
    match attr(n, "main") {
        Some("true") => MainState::IsMain,
        Some("false") => MainState::IsNotMain,
        _ => MainState::Unset,
    }
}
