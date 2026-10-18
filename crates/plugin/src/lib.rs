// SPDX-License-Identifier: GPL-3.0-or-later
//! Format-independent plugin logic shared by the CLAP/LV2/VST3 wrappers
//! and the standalone binary: the serializable plugin state, the
//! heartbeat between host parameters and the realtime engine, and the
//! stereo processing entry point.
//!
//! The engine is created against the *host* samplerate (plugin hosts
//! drive the rate), and control flows through the same lock-free
//! command ring the standalone app uses.

pub mod state;
pub mod core;

pub use core::PluginCore;
pub use state::PluginState;

use dg_common::MidiEvent;

/// Host velocities are 0..=1; the engine expects the 1..=127 byte scale
/// upstream-style (byte `v - 0.5) / 127` centre transform). This maps a
/// host velocity to the equivalent byte value and keeps the low and high
/// ends reachable (0.0 → 1 so zero velocity does not disqualify the hit;
/// hosts rarely want true zero-velocity hits anyway).
pub fn velocity_to_byte(host_velocity: f32) -> f32 {
    (host_velocity * 127.0)
        .round()
        .clamp(1.0, 127.0)
}

/// Block-relative offset adapter used by wrappers that receive absolute
/// event positions inside the current block.
pub fn with_offset(mut event: MidiEvent, offset: usize) -> MidiEvent {
    event.offset = offset;
    event
}
