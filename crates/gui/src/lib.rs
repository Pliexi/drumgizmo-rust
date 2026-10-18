// SPDX-License-Identifier: GPL-3.0-or-later
//! Dark-themed egui interface for drumgizmo-rust: kit browser, sequencer
//! grid and step inspector in three tabs, with undo/redo and keyboard
//! shortcuts.
//!
//! The crate owns the *editing* state (pattern, gains, selection) and
//! hands realtime states to the engine through the shared lock-free
//! command ring (`RtCommand`). The engine itself never lives in the GUI
//! process except in the standalone binary, where cpal owns it and this
//! app only talks to the command producer.
//!
//! Tests: GUI behaviour is only exercised by the build (eframe needs a
//! platform); logic helpers are pure where possible.

mod app;
mod inspector;
mod kit_tab;
mod seq_tab;
mod summary;

pub use app::{DrumApp, Mode};

use rtrb::Producer;

use dg_common::RtCommand;
use dg_sequencer::Pattern;

/// Options for launching the window (standalone binary builds these).
#[derive(Debug, Clone)]
pub struct AppOptions {
    /// Initial kit directory loaded on start (no load when empty).
    pub kit_path: String,
    /// Initial sequencer pattern lanes are derived from the kit midimap.
    pub pattern: Pattern,
    /// Initial master gain 0.0..=8.0.
    pub master_gain: f32,
    /// Window scale multiplier (DPI aware; 0.0 = let the platform decide).
    pub scale_override: f32,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            kit_path: String::new(),
            pattern: Pattern::default(),
            master_gain: 1.0,
            scale_override: 0.0,
        }
    }
}

/// Runs the eframe main loop. Returns when the window is closed. The
/// producer must belong to the same engine the standalone app streamed.
pub fn run(options: AppOptions, producer: Producer<RtCommand>) -> eframe::Result<()> {
    let scale = if options.scale_override > 0.0 {
        Some(options.scale_override)
    } else {
        None
    };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("drumgizmo-rust")
            .with_min_inner_size([960.0, 560.0]),
        ..Default::default()
    };
    // egui 0.37 does the DPI discovery itself; a manual override is only
    // needed on odd compositors, where the user env variable remains the
    // authoritative source (scale_override kept for the standalone CLI).
    let _ = scale;
    eframe::run_native(
        "drumgizmo-rust",
        native,
        Box::new(move |cc| {
            let app = DrumApp::new(cc, options, producer);
            Ok(Box::new(app))
        }),
    )
}
