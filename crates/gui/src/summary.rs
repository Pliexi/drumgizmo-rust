// SPDX-License-Identifier: GPL-3.0-or-later
//! `KitSummary`: UI side quick view of a drumkit directory (pure reader,
//! used to show instrument names, midi bindings, voice gain sliders and
//! default sequencer lanes without touching the audio thread).

use std::path::Path;

use dg_kit::dom::DrumkitDom;
use dg_kit::parser::{parse_drumkit_file, parse_midimap_file};

use dg_kit::MidimapEntry;

/// Instrument binding useable by the sequencer lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInstrument {
    /// Instrument id = drumkit.xml index (the engine's instrument order).
    pub instrument_id: usize,
    pub name: String,
    pub group: String,
    /// MIDI note from midimap.xml (None = untriggerable, lane hidden).
    pub midi_note: Option<u8>,
}

/// Everything the Kit tab shows about a loaded kit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KitSummary {
    /// Display name of the drumkit.
    pub name: String,
    /// Absolute kit dir for display.
    pub path: String,
    pub samplerate: Option<u32>,
    pub instrument_count: usize,
    pub channel_count: usize,
    /// Instruments with their midi bindings (midimap resolved).
    pub instruments: Vec<BoundInstrument>,
}

impl KitSummary {
    /// Reads a kit dir (or drumkit.xml file path) on the UI thread.
    /// Failures return an error message for the status bar (the audio
    /// side load command then reports nothing; both sides share the same
    /// parser, so UI failure ≈ audio failure).
    pub fn load_from(path: &Path) -> Result<KitSummary, String> {
        let (dir, xml_file) = resolve_kit_xml(path)?;
        let dom: DrumkitDom = parse_drumkit_file(&xml_file).map_err(|e| e.to_string())?;

        let midi_path = dir.join("midimap.xml");
        let midi_entries: Vec<MidimapEntry> = parse_midimap_file(&midi_path).unwrap_or_default();
        let mut note_by_name: Vec<(&String, u8)> = Vec::new();
        for entry in &midi_entries {
            note_by_name.push((&entry.instrument, entry.note));
        }

        let instruments = dom
            .instruments
            .iter()
            .enumerate()
            .map(|(id, inst)| {
                let midi_note = note_by_name
                    .iter()
                    .find(|(name, _)| **name == inst.name)
                    .map(|(_, note)| *note);
                BoundInstrument {
                    instrument_id: id,
                    name: inst.name.clone(),
                    group: inst.group.clone(),
                    midi_note,
                }
            })
            .collect();

        Ok(KitSummary {
            name: if dom.metadata.title.is_empty() {
                file_path_stem(&xml_file)
            } else {
                dom.metadata.title.clone()
            },
            path: dir.display().to_string(),
            samplerate: Some(dom.samplerate as u32),
            instrument_count: dom.instruments.len(),
            channel_count: dom.channels.len(),
            instruments,
        })
    }

    /// The lane set the sequencer derives at kit load: one lane per
    /// midimap-bound instrument (a visible slot for every playable
    /// instrument, initially empty).
    pub fn default_lanes(&self, length: usize) -> Vec<dg_sequencer::Lane> {
        self.instruments
            .iter()
            .filter_map(|inst| {
                inst.midi_note
                    .map(|note| dg_sequencer::Lane::new(inst.instrument_id, note, length))
            })
            .collect()
    }

    /// Display name of the lane's instrument by id (used by the demo beat
    /// heuristics and the grid).
    pub fn lane_name(&self, id: usize) -> String {
        self.instrument_name(id).to_string()
    }

    /// Instrument display name by engine id.
    pub fn instrument_name(&self, id: usize) -> &str {
        self.instruments
            .iter()
            .find(|inst| inst.instrument_id == id)
            .map(|inst| inst.name.as_str())
            .unwrap_or("???")
    }
}

/// A kit path argument may be a directory (with drumkit.xml) or the file.
fn resolve_kit_xml(path: &Path) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    if path.is_dir() {
        let file = path.join("drumkit.xml");
        return Ok((path.to_path_buf(), file));
    }
    if path.is_file() {
        let dir = path
            .parent()
            .ok_or_else(|| "drumkit file has no parent".to_string())?;
        return Ok((dir.to_path_buf(), path.to_path_buf()));
    }
    Err(format!("kit path does not exist: {}", path.display()))
}

/// Fallback display name: the parent directory name of drumkit.xml.
fn file_path_stem(xml_file: &Path) -> String {
    xml_file
        .parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "drumkit".into())
}

/// Resolves a user-entered kit path (directory or drumkit.xml file) into
/// the directory form the engine LoadKit command expects.
pub fn kit_directory(path: &Path) -> Option<String> {
    if path.is_dir() {
        Some(path.display().to_string())
    } else if path.is_file() {
        path.parent().map(|parent| parent.display().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_kit_summary_has_midimap_bindings() {
        let dir = std::env::temp_dir().join("dg-gui-tests").join("summary");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kit_path =
            dg_kit::kitgen::generate_test_kit(&dir, 44100, dg_kit::kitgen::TestKitFlavour::Small)
                .unwrap();

        let summary = KitSummary::load_from(&kit_path).unwrap();
        assert_eq!(summary.name, "Rust Test Kit");
        assert_eq!(summary.instrument_count, 5);
        assert_eq!(summary.channel_count, 8);

        let kick = &summary.instruments[0];
        assert_eq!(kick.name, "Kick");
        assert_eq!(kick.midi_note, Some(36));

        // Lanterns: every mapped instrument has a lane; hihat pair shares
        // a kit group but each has its own note.
        let lanes = summary.default_lanes(16);
        assert_eq!(lanes.len(), 5);
        assert!(lanes
            .iter()
            .zip(summary.instruments.iter())
            .all(|(lane, inst)| lane.instrument_id == inst.instrument_id));
        assert_eq!(lanes[0].midi_note, 36);
    }

    #[test]
    fn missing_path_is_an_error_message() {
        let dir = std::env::temp_dir().join("dg-gui-tests").join("nowhere");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(KitSummary::load_from(&dir).is_err());
    }

    #[test]
    fn drumkit_xml_file_argument_resolves_to_dir() {
        let dir = std::env::temp_dir().join("dg-gui-tests").join("xml-arg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml =
            dg_kit::kitgen::generate_test_kit(&dir, 44100, dg_kit::kitgen::TestKitFlavour::Small)
                .unwrap();
        assert!(kit_directory(&xml).is_some());
    }
}
