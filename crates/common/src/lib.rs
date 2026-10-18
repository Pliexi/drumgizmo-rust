//! Shared types and realtime-safe helpers for the drumgizmo-rust workspace.
//!
//! This crate must stay dependency-light and must never depend on the other
//! workspace crates: everything else depends on it.

pub mod midi_events;
pub mod rng;
pub mod rt_message;

pub use midi_events::{decode_midi_status_bytes, MessageType, MidiEvent};
pub use rt_message::RtCommand;

/// The sample format used everywhere in the workspace.
pub type Sample = f32;

/// Gateway value for "no channel" in kit channel mapping.
pub const NO_CHANNEL: usize = usize::MAX;

/// Per-note velocity range 0.0..=1.0 as defined by MIDI 2.0 bridging.
pub const MAX_VELOCITY: f32 = 1.0;

/// Computes a linear-to-dB factor for `ratio` (1.0 == 0 dB).
#[inline]
pub fn dbfs_from_ratio(ratio: Sample) -> Sample {
    debug_assert!(ratio >= 0.0);
    20.0 * ratio.log10()
}

#[inline]
pub fn ratio_from_dbfs(dbfs: Sample) -> Sample {
    10f32.powf(dbfs / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_roundtrip() {
        assert!((dbfs_from_ratio(ratio_from_dbfs(-6.02)) + 6.02).abs() < 1e-3);
        assert_eq!(dbfs_from_ratio(1.0), 0.0);
        assert!(ratio_from_dbfs(0.0) == 1.0);
    }
}
