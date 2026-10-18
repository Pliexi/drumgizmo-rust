// SPDX-License-Identifier: GPL-3.0-or-later
//! The drumgizmo-rust eframe application: three tabs (Kit / Sequencer /
//! Inspector), undo/redo stacks, keyboard shortcuts and all realtime
//! commands going through the command ring producer.

use rtrb::Producer;

use egui::{Context, Key, Modifiers};

use dg_common::{MessageType, MidiEvent, RtCommand, SharedProducer};
use dg_sequencer::Pattern;

use crate::summary::KitSummary;

/// Which tab is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Mute-all button (panic) + kit browser (escape closes app).
    Default,
    /// Kit browser tab.
    #[default]
    Kit,
    /// Sequencer grid tab.
    Sequencer,
    /// Step inspector tab.
    Inspector,
}

/// Undo entry: a pattern state snapshot made *before* a mutation.
#[derive(Debug, Clone)]
enum UndoEntry {
    Pattern(Pattern),
}

/// GUI application state (`pub(crate)` fields: the tab modules are the
/// view layer of this state). The command ring producer is shared with
/// the MIDI callback through the mutex — host-side only, the audio
/// thread owns the consumer side and never locks it.
pub struct DrumApp {
    pub(crate) producer: SharedProducer,
pub(crate) mode: Mode,
    /// Status text that the bottom bar shows.
    status: String,

    // --- kit tab ---
    pub(crate) kit_dir_entry: String,
    pub(crate) kit_summary: Option<KitSummary>,
    pub(crate) kit_error: Option<String>,
    pub(crate) master_gain: f32,
    pub(crate) voice_gains: Vec<f32>,

    // --- sequencer tab ---
    pub(crate) pattern: Pattern,
    /// Selected lane; saved across frames.
    pub(crate) selected_lane: usize,
    /// Selected step within the selected lane (inspector target).
    pub(crate) selected_step: usize,
    /// Velocity used when clicking empty steps of the grid.
    pub(crate) default_step_velocity: f32,
    /// While dragging a lane toggles, the drag applies this state.
    pub(crate) drag_paint: Option<bool>,
    pub(crate) tempo: f64,
    pub(crate) midi_import_path: String,
    pub(crate) midi_export_path: String,
    /// UI side transport memory for the space shortcut.
    pub(crate) playing: bool,

    /// Snapshot scheduled on the first paint cell of a drag gesture.
    pub(crate) drag_snapshot_pending: bool,

    // --- undo / redo ---
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,

    /// Commands get the engine pattern only when the audio side needs a
    /// fresh copy; flushed after every edit that changed the pattern.
    pattern_dirty: bool,
}

impl DrumApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        options: crate::AppOptions,
        producer: SharedProducer,
    ) -> Self {
        Self::configure_theme(&cc.egui_ctx);
        let mut app = Self {
            producer,
            mode: Mode::Kit,
            status: "No kit loaded.".into(),
            kit_dir_entry: options.kit_path.clone(),
            kit_summary: None,
            kit_error: None,
            master_gain: options.master_gain,
            voice_gains: Vec::new(),
            pattern: options.pattern,
            selected_lane: 0,
            selected_step: 0,
            default_step_velocity: 0.8,
            drag_paint: None,
            drag_snapshot_pending: false,
            tempo: 120.0,
            midi_import_path: String::new(),
            midi_export_path: String::new(),
            playing: false,
            undo: Vec::new(),
            redo: Vec::new(),
            pattern_dirty: false,
        };
        if !options.kit_path.is_empty() {
            app.request_kit_load(options.kit_path);
        }
        app
    }

    /// Dark palette, 1 px separations, larger glow text areas.
    fn configure_theme(ctx: &Context) {
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(egui::Color32::from_rgb(214, 214, 220));
        visuals.panel_fill = egui::Color32::from_rgb(22, 23, 27);
        visuals.window_fill = egui::Color32::from_rgb(30, 31, 36);
        visuals.extreme_bg_color = egui::Color32::from_rgb(14, 15, 18);
        visuals.faint_bg_color = egui::Color32::from_rgb(27, 29, 34);
        visuals.selection.bg_fill = egui::Color32::from_rgb(70, 130, 180);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(22, 23, 27);
        ctx.set_visuals(visuals);
    }

    // ------------------------------------------------------------------
    // realtime routing
    // ------------------------------------------------------------------

    fn send(&mut self, command: RtCommand) {
        // Commands may not wait: the ring only holds a few messages.
        if self.producer.lock().unwrap().push(command).is_err() {
            self.status_push("command buffer full, message dropped");
        }
    }

    fn send_midi(&mut self, event: MidiEvent) {
        self.send(RtCommand::Midi(event));
    }

    /// Loads a kit: UI side parses (instant summary), audio side gets the
    /// asynchronous LoadKit command.
    pub fn request_kit_load(&mut self, path: String) {
        self.kit_dir_entry.clone_from(&path);
        self.kit_error = None;
        let kit_path = std::path::PathBuf::from(&path);
        match crate::summary::KitSummary::load_from(&kit_path) {
            Ok(summary) => {
                let directory = crate::summary::kit_directory(&kit_path)
                    .unwrap_or_else(|| summary.path.clone());
                self.send(RtCommand::LoadKit { path: directory.clone() });
                let lanes = summary.default_lanes(self.pattern.length);
                self.snapshot_pattern();
                self.pattern.lanes = lanes;
                self.mark_pattern_dirty();
                self.voice_gains = vec![1.0; summary.instrument_count];
                self.selected_step = self.selected_step.min(self.pattern.length.saturating_sub(1));
                self.kit_summary = Some(summary);
                self.status = format!("Kit loaded: {path}");
            }
            Err(error) => {
                self.kit_error = Some(error);
                self.status = format!("Kit load failed: {path}");
            }
        }
    }

    /// Swaps the current pattern (MIDI import / preset loads).
    pub fn replace_pattern(&mut self, pattern: Pattern) {
        self.snapshot_pattern();
        self.pattern = pattern;
        self.pattern
            .ensure_lengths(self.pattern.length.max(16));
        self.mark_pattern_dirty();
    }

    // ------------------------------------------------------------------
    // undo / redo
    // ------------------------------------------------------------------

    /// Records the current pattern for undo and flags the audio side.
    pub fn snapshot_pattern(&mut self) {
        self.undo.push(UndoEntry::Pattern(self.pattern.clone()));
        // Cap the history so a long editing session stays bounded.
        if self.undo.len() > 256 {
            let overflow = self.undo.len() - 256;
            self.undo.drain(0..overflow);
        }
        self.redo.clear();
    }

    /// Marks the audio side needing the new pattern (broadcast lazily).
    pub fn mark_pattern_dirty(&mut self) {
        self.pattern_dirty = true;
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            let UndoEntry::Pattern(previous) = previous;
            self.redo.push(UndoEntry::Pattern(self.pattern.clone()));
            self.pattern = previous;
            self.selected_step = self.selected_step.min(self.pattern.length.saturating_sub(1));
            self.pattern_dirty = true;
            self.status = "Undo".into();
        } else {
            self.status = "Nothing to undo".into();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            let UndoEntry::Pattern(next) = next;
            self.undo.push(UndoEntry::Pattern(self.pattern.clone()));
            self.pattern = next;
            self.selected_step = self.selected_step.min(self.pattern.length.saturating_sub(1));
            self.pattern_dirty = true;
            self.status = "Redo".into();
        } else {
            self.status = "Nothing to redo".into();
        }
    }

    // ------------------------------------------------------------------
    // sequencer helpers shared with the tab modules
    // ------------------------------------------------------------------

    /// Replaces the pattern lanes with lanes derived from the loaded kit
    /// midimap (delete/insert lanes in one undo step).
    pub(crate) fn adopt_kit_lanes(&mut self) {
        if let Some(summary) = &self.kit_summary {
            let lanes = summary.default_lanes(self.pattern.length);
            self.snapshot_pattern();
            self.pattern.lanes = lanes;
            self.mark_pattern_dirty();
            self.set_status("Lanes adopted from kit midimap");
        }
    }

    /// Sets the enabled flag of one grid cell and stamps the pattern.
    pub(crate) fn apply_step_paint(&mut self, lane_index: usize, step: usize, paint: bool) {
        let Some(lane) = self.pattern.lanes.get_mut(lane_index) else {
            return;
        };
        if step >= self.pattern.length {
            return;
        }
        lane.ensure_length(step + 1);
        let step_data = &mut lane.steps[step];
        step_data.enabled = paint;
        if paint {
            step_data.velocity = self.default_step_velocity;
        }
        self.mark_pattern_dirty();
    }

    /// Keeps the selected step inside the pattern length (long used
    /// after length changes).
    pub(crate) fn snap_selection_to_pattern(&mut self) {
        self.selected_step = self.selected_step.min(self.pattern.length.saturating_sub(1));
        self.selected_lane =
            self.selected_lane.min(self.pattern.lanes.len().saturating_sub(1));
    }

    /// Applies a mutation to one lane step (inspector path).
    pub(crate) fn with_lane_step(
        &mut self,
        lane_index: usize,
        step_index: usize,
        mutate: impl FnOnce(&mut dg_sequencer::Step),
    ) {
        let Some(lane) = self.pattern.lanes.get_mut(lane_index) else {
            return;
        };
        lane.ensure_length(self.pattern.length.max(step_index + 1));
        if let Some(step) = lane.steps.get_mut(step_index) {
            mutate(step);
        }
    }

    pub(crate) fn undo_pattern(&mut self) {
        self.undo();
    }

    pub(crate) fn redo_pattern(&mut self) {
        self.redo();
    }

    fn status_push(&mut self, text: &str) {
        self.status = text.to_string();
    }

    // ------------------------------------------------------------------
    // shortcuts ( kepboard handling, runs before the tabs draw)
    // ------------------------------------------------------------------

    fn handle_shortcuts(&mut self, ctx: &Context) {
        let ctrl = ctx.input(|input| input.modifiers.contains(Modifiers::CTRL));
        let (space, undo, redo) = ctx.input(|input| {
            (
                input.key_pressed(Key::Space),
                input.key_pressed(Key::Z),
                input.key_pressed(Key::Y),
            )
        });
        if space {
            // A toggle pressed when nothing plays is detected by the audio
            // side: SeqStart starts/continues, SeqStop stops.
            self.toggle_transport();
        } else if undo && ctrl {
            self.undo();
        } else if redo && ctrl {
            self.redo();
        }

        let lane_digits = ctx.input(|input| {
            for (digit, lane) in [
                (Key::Num1, 0usize),
                (Key::Num2, 1),
                (Key::Num3, 2),
                (Key::Num4, 3),
                (Key::Num5, 4),
                (Key::Num6, 5),
                (Key::Num7, 6),
                (Key::Num8, 7),
                (Key::Num9, 8),
            ] {
                if input.key_pressed(digit) {
                    return Some(lane);
                }
            }
            None
        });
        if let Some(lane) = lane_digits {
            self.selected_lane = lane.min(self.pattern.lanes.len().saturating_sub(1));
        }
    }

    /// Space: start/stop the sequencer. The GUI keeps its own transport
    /// flag (the audio side remains authoritative; pressing play twice
    /// from the UI flag is harmless for the engine which just re-seats
    /// its schedule).
    fn toggle_transport(&mut self) {
        self.playing = !self.playing;
        if self.playing {
            self.send(RtCommand::SeqStart);
            self.status = "Sequencer started".into();
        } else {
            self.send(RtCommand::SeqStop);
            self.status = "Sequencer stopped".into();
        }
    }
}

impl eframe::App for DrumApp {
    /// Runs *before* the UI pass: shortcuts and one-shot enables.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.handle_shortcuts(ctx);
        if self.pattern_dirty {
            // Audio side re-hydrates the pattern only when it changed.
            if let Ok(json) = serde_json::to_string(&self.pattern) {
                self.send(RtCommand::SeqPattern { json });
            }
            self.pattern_dirty = false;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.mode == Mode::Kit, "Kit").clicked() {
                    self.mode = Mode::Kit;
                }
                if ui
                    .selectable_label(self.mode == Mode::Sequencer, "Sequencer")
                    .clicked()
                {
                    self.mode = Mode::Sequencer;
                }
                if ui
                    .selectable_label(self.mode == Mode::Inspector, "Inspector")
                    .clicked()
                {
                    self.mode = Mode::Inspector;
                }
                ui.separator();
                if ui.button("▶ Play / ⏹ Stop").clicked() {
                    self.toggle_transport();
                }
                ui.separator();
                if ui.button("🔇 Panic").clicked() {
                    self.send(RtCommand::Panic);
                    self.status = "Panic: all voices faded".into();
                }
                ui.separator();
                let master = ui.add(
                    egui::Slider::new(&mut self.master_gain, 0.0..=2.0).text("Master"),
                );
                if master.changed() {
                    self.send(RtCommand::MasterGain { gain: self.master_gain });
                }
                ui.separator();
                ui.weak(&self.status);
            });
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Tempo: {:.1} BPM", self.tempo));
                ui.separator();
                ui.label(format!(
                    "Pattern: {} · {} steps · lanes: {}",
                    self.pattern.name,
                    self.pattern.length,
                    self.pattern.lanes.len(),
                ));
                ui.separator();
                ui.weak("[Space] transport · [Ctrl+Z/Y] undo/redo · [1-9] lane select");
            });
        });

        // The tab content lives in a central panel so the root ui paints
        // a proper background.
        egui::CentralPanel::default().show(ui, |ui| match self.mode {
            Mode::Kit | Mode::Default => crate::kit_tab::show(self, ui),
            Mode::Sequencer => crate::seq_tab::show(self, ui),
            Mode::Inspector => crate::inspector::show(self, ui),
        });
    }
}

// ---------------------------------------------------------------------
// shared helpers used by the tab modules
// ---------------------------------------------------------------------

impl DrumApp {
    pub(crate) fn producer(&mut self) -> std::sync::MutexGuard<'_, Producer<RtCommand>> {
        self.producer.lock().unwrap()
    }

    pub(crate) fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    pub(crate) fn send_kind(&mut self, kind: MessageType, note: u8, velocity: f32) {
        self.send_midi(MidiEvent {
            kind,
            note,
            velocity,
            offset: 0,
        });
    }
}
