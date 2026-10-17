//! Commands sent from the UI / host thread down to the realtime audio thread.
//!
//! Everything here must be `Send` and cheap to move across a lock-free ring
//! buffer (`rtrb`). Strings are transported as lightweight handles.

use serde::{Deserialize, Serialize};

/// Realtime command set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RtCommand {
    /// Note-On / Note-Off event to be sent into the instrument pipeline.
    Midi(crate::midi_events::MidiEvent),
    /// Replaces the active kit with the one at `path` (asynchronous load).
    /// The old kit stays valid until the load finishes.
    LoadKit {
        path: String,
    },
    /// Master output volume, 0.0..=8.0 linear gain.
    MasterGain {
        gain: f32,
    },
    /// Per-instrument volume, 0.0..=8.0 linear gain, -1 instrument id = all.
    /// Ids start at 0 following the kit instrument order.
    VoiceGain {
        instrument: usize,
        gain: f32,
    },
    /// Mute all currently playing voices (panic button).
    Panic,
    /// Enable/disable velocity humanize of the sequencer (audio side).
    SeqStart,
    SeqStop,
    /// tempo in quarter notes per minute.
    SeqTempo {
        bpm: f64,
    },
    /// Pattern activation as a serialized JSON string; the sequencer object on
    /// the audio side will re-hydrate it.
    SeqPattern {
        json: String,
    },
}
