// SPDX-License-Identifier: GPL-3.0-or-later
// Voice manager, sample selection and realtime rendering of sample events.
//!
//! Ports of upstream `sample_selection.cc` (power list scoring, >= 2.0 kits),
//! `rangemap.h`/`instrument.cc` (v1.0 velocity groups + random choice), and
//! the per-channel voice handling of `events_ds.h`/`audiocacheeventhandler.cc`
//! (voice stealing, rampdown, normalization).

pub mod selection;
pub mod voice;

pub use selection::{PowerList, SampleRef, SampleSelection, SelectionStrategy, VelocityRangeTable};
pub use voice::{
    ramp_frames, SampleBank, Voice, VoiceEvent, VoiceManager, CHOKE_EVENT_RAMPDOWN_MS,
    GROUP_RAMPDOWN_MS, VOICE_LIMIT_MAX_DEFAULT, VOICE_LIMIT_RAMPDOWN_DEFAULT_MS, VOICE_POOL,
};
