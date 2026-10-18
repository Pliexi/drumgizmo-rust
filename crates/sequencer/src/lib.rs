// SPDX-License-Identifier: GPL-3.0-or-later
//! Drum step sequencer: pattern model and timing engine.
//!
//! * [`model`]: serializable patterns (lanes, steps, swing, humanize,
//!   flam, ratchet, articulations, chain/song entries).
//! * [`timing`]: sample-accurate scheduling, host tempo sync with internal
//!   fallback, deterministic probability/humanize stream.

pub mod model;
pub mod timing;

pub use model::{Articulation, ChainEntry, Division, Lane, Pattern, Repeat, Step};
pub use timing::{Sequencer, StepEvent, StepEventKind};
