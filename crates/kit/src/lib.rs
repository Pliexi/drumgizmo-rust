// SPDX-License-Identifier: GPL-3.0-or-later
//! DrumGizmo kit loader for the drumgizmo-rust workspace.
//!
//! This crate is a faithful port of the original DrumGizmo (0.9.20) kit
//! loading pipeline:
//!
//! * [`dom`] mirrors `src/DGDOM.h`
//! * [`parser`] mirrors `src/dgxmlparser.cc` (drumkit.xml and instrument XML)
//!   plus `src/midimapparser.cc` (midimap.xml)
//! * [`decoder`] wraps `src/audiofile.cc` semantics: WAV/FLAC/AIFF decoding,
//!   per-filechannel extraction, and exact-rate resampling to the engine rate
//!   (like the original `zrwrapper.cc` + `audiocachefile.cc` pair)
//! * [`loader`] mirrors `src/drumkitloader.cc`: background thread, progress
//!   notifications, immutable [`LoadedKit`] published via `ArcSwap`-like API
//! * [`kitgen`] (the `kitgen` binary) equals `test/drumkit_creator.cc` and
//!   produces deterministic synthetic test kits.

pub mod decoder;
pub mod dom;
pub mod kitgen;
pub mod loader;
pub mod parser;

pub use dom::{DrumkitDom, InstrumentDom, SampleDom};
pub use loader::{
    lex_normalise, resolve_kit_xml, AudioFileData, AudioFileRef, ChokeRef, InstrumentChannelRef,
    KitLoader, LoadError, LoadProgress, LoadStage, LoadedInstrument, LoadedKit, LoadedSample,
    VelocityGroup,
};

/// Synchronous kit load for tools and tests (no live progress).
pub fn load_kit_sync(
    path: &std::path::Path,
    engine_samplerate: u32,
) -> Result<std::sync::Arc<LoadedKit>, LoadError> {
    loader::load_kit_blocking(path, engine_samplerate, &loader::ProgressShared::default())
}
pub use parser::{parse_drumkit_file, parse_instrument_file, parse_midimap_file, MidimapEntry};
