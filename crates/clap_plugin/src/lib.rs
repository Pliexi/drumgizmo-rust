// SPDX-License-Identifier: GPL-3.0-or-later
//! CLAP wrapper for the drumgizmo-rust engine.
//!
//! The whole implementation lives in [`dg_plugin::nih`] (nice-plug):
//! realtime engine plumbing, note events, persisted state, and a full
//! egui editor embedded into the host window. This crate only exports
//! the entry points.

use dg_plugin::nih::DrumPlugin;

nice_plug::nice_export_clap!(DrumPlugin);
