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
}

/// A single MIDI event destined for the realtime engine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MidiEvent {
    /// Message type.
    pub kind: MessageType,
    /// MIDI note itself, 0..=127.
    pub note: u8,
    /// Velocity, 0.0..=1.0.
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
}
