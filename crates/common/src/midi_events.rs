//! Common MIDI event representation shared between the engine, the sequencer
//! and the plugin wrappers.

use serde::{Deserialize, Serialize};

/// The message types relevant for drum triggering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    /// Note on with velocity as an 8.8 fixed point value downstream.
    OnSet,
    /// Note off / zero velocity considered as choke for the same instrument.
    NoteOff,
    /// After-touch on a note: choke/mute trigger (DrumGizmo convention).
    Choke,
}

/// A single MIDI event destined for the realtime engine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MidiEvent {
    /// Message type.
    pub kind: MessageType,
    /// MIDI note itself, 0..=127.
    pub note: u8,
    /// Velocity in the 1..=127 MIDI byte scale as delivered by MIDI
    /// sources (the realtime engine maps OnSet to (v - 0.5)/127 upstream
    /// style and requires Choke velocity > 0 to trigger). Other crate
    /// paths (sequencer steps) carry 0..=1 normalized values instead.
    pub velocity: f32,
    /// The offset of the event within the block that delivered it, in samples.
    pub offset: usize,
}

impl MidiEvent {
    /// Creates a Note-On event.
    pub fn note_on(note: u8, velocity: f32, offset: usize) -> Self {
        Self {
            kind: MessageType::OnSet,
            note,
            velocity,
            offset,
        }
    }

    /// Creates a Note-Off event.
    pub fn note_off(note: u8, offset: usize) -> Self {
        Self {
            kind: MessageType::NoteOff,
            note,
            velocity: 0.0,
            offset,
        }
    }

    /// Creates a choke (after-touch) event on a note.
    pub fn choke(note: u8, offset: usize) -> Self {
        Self {
            kind: MessageType::Choke,
            note,
            // The realtime engine ignores Choke events with velocity 0;
            // a constructed choke must always fire.
            velocity: 1.0,
            offset,
        }
    }
}

/// Decodes one raw MIDI status message (`data` of 1..=4 bytes) into the
/// engine events it carries: note on/off and polyphonic pressure as
/// choke. Runs on every plugin wrapper's realtime path; keeps the byte
/// velocity scale untouched. Realtime/system bytes and running-status
/// data without a leading status are skipped, the MIDI channel is
/// ignored (`midimap`, not the channel, routes drums like upstream).
///
/// Truncated messages keep their defaulted payload bytes (0), matching
/// [`crate::note`]-driven paths.
pub fn decode_midi_status_bytes(data: &[u8]) -> Vec<MidiEvent> {
    let Some(&status) = data.first() else {
        return Vec::new();
    };
    // Running status / realtime bytes alone carry no drum event here.
    if status < 0x80 {
        return Vec::new();
    }
    let note = data.get(1).copied().unwrap_or(0);
    let data1 = data.get(2).copied().unwrap_or(0);
    match status & 0xF0 {
        0x90 => vec![MidiEvent {
            kind: MessageType::OnSet,
            note,
            velocity: data1 as f32,
            offset: 0,
        }],
        0x80 => vec![MidiEvent {
            kind: MessageType::NoteOff,
            note,
            velocity: 0.0,
            offset: 0,
        }],
        0xA0 if data1 > 0 => vec![MidiEvent {
            kind: MessageType::Choke,
            note,
            velocity: data1 as f32,
            offset: 0,
        }],
        _ => Vec::new(),
    }
}
