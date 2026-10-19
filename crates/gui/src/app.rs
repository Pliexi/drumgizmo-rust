// SPDX-License-Identifier: GPL-3.0-or-later
//! The drumgizmo-rust eframe application: three tabs (Kit / Sequencer /
//! Inspector), undo/redo stacks, keyboard shortcuts and all realtime
//! commands going through the command ring producer.

use rtrb::Producer;

use egui::{Context, Key, Modifiers};

use dg_common::{AudioStatusSnapshot, MessageType, MidiEvent, RtCommand, SharedProducer};
use dg_engine::EngineWatch;
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
    /// Host-provided audio output snapshot (status bar).
    pub(crate) audio: Option<AudioStatusSnapshot>,
    /// Realtime transport mirror fed by the engine's watch.
    pub(crate) watch: Option<std::sync::Arc<EngineWatch>>,

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
    /// Cell of the last wheel-velocity edit (snapshot de-duplication).
    pub(crate) velocity_edited: Option<(usize, usize)>,
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
        Self::configure_theme_on(&cc.egui_ctx);
        let mut app = Self::new_for_host(
            producer,
            options.kit_path.clone(),
            options.master_gain,
            120.0,
            options.pattern.clone(),
        );
        app.audio = options.audio;
        app.watch = options.watch;
        app
    }

    /// Window-host independent app state: identical defaults to
    /// `new()`, but constructible from any GUI host (plugin editors
    /// create this inside `Editor::spawn` without eframe).
    pub fn new_for_host(
        producer: SharedProducer,
        kit_path: String,
        master_gain: f32,
        tempo: f64,
        pattern: Pattern,
    ) -> Self {
        let mut app = Self {
            producer,
            mode: Mode::Kit,
            status: "No kit loaded.".into(),
            kit_dir_entry: kit_path.clone(),
            kit_summary: None,
            kit_error: None,
            master_gain,
            voice_gains: Vec::new(),
            pattern,
            selected_lane: 0,
            selected_step: 0,
            default_step_velocity: 0.8,
            drag_paint: None,
            velocity_edited: None,
            drag_snapshot_pending: false,
            tempo,
            midi_import_path: String::new(),
            midi_export_path: String::new(),
            playing: false,
            undo: Vec::new(),
            redo: Vec::new(),
            pattern_dirty: false,
            audio: None,
            watch: None,
        };
        if !kit_path.is_empty() {
            app.request_kit_load(kit_path);
        }
        app
    }

    /// Dark palette, 1 px separations, larger glow text areas.
    pub fn configure_theme_on(ctx: &Context) {
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

    /// Pastes the system clipboard into the kit path editor. Uses
    /// arboard directly (wl-clipboard-rs on Wayland portals included),
    /// so it works even where the winit shortcut bridge is silent.
    pub(crate) fn paste_kit_path(&mut self) {
        let pasted = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_text())
            .map(|text| crate::kit_tab::normalize_kit_path(&text));
        match pasted {
            Ok(text) if !text.is_empty() => {
                self.kit_dir_entry = text;
                self.status = "Path pasted — press Load Kit or pick a file below.".into();
            }
            Ok(_) => {
                self.status = "Clipboard holds no text.".into();
            }
            Err(error) => {
                self.status = format!("Could not read clipboard: {error}");
            }
        }
    }

    /// True when the current pattern holds no enabled step (used by the
    /// demo-beat seeding decision on kit load).
    fn pattern_is_empty(&self) -> bool {
        !self
            .pattern
            .lanes
            .iter()
            .flat_map(|lane| lane.steps.iter().map(|step| step.enabled))
            .any(|enabled| enabled)
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
                self.send(RtCommand::LoadKit {
                    path: directory.clone(),
                });
                let lanes = summary.default_lanes(self.pattern.length);
                self.snapshot_pattern();
                self.pattern.lanes = lanes;
                // Empty pattern? Seed a demo beat: the user hears sound
                // from the transport immediately (instant "kit works"
                // feedback) and can overwrite/erase freely.
                let demonstration = self.pattern_is_empty();
                if demonstration {
                    let names: Vec<String> = self
                        .pattern
                        .lanes
                        .iter()
                        .map(|lane| summary.lane_name(lane.instrument_id))
                        .collect();
                    crate::demo::seed_pattern(&mut self.pattern, &names);
                }
                self.mark_pattern_dirty();
                self.voice_gains = vec![1.0; summary.instrument_count];
                self.selected_step = self
                    .selected_step
                    .min(self.pattern.length.saturating_sub(1));
                self.kit_summary = Some(summary);
                self.status = if demonstration {
                    format!("Kit loaded: {path} — demo beat seeded, press ▶")
                } else {
                    format!("Kit loaded: {path}")
                };
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
        self.pattern.ensure_lengths(self.pattern.length.max(16));
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
            self.selected_step = self
                .selected_step
                .min(self.pattern.length.saturating_sub(1));
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
            self.selected_step = self
                .selected_step
                .min(self.pattern.length.saturating_sub(1));
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

    /// Keeps the selected step inside the pattern length (long used
    /// after length changes).
    pub(crate) fn snap_selection_to_pattern(&mut self) {
        self.selected_step = self
            .selected_step
            .min(self.pattern.length.saturating_sub(1));
        self.selected_lane = self
            .selected_lane
            .min(self.pattern.lanes.len().saturating_sub(1));
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
        // While the user types into any text field (kit path, import
        // path...) the global shortcuts must yield to the editor:
        // otherwise every digit typed would also switch the selected
        // lane, and Space would toggle the transport instead of
        // appearing inside the field. Ctrl+V/Ctrl+Z in the field are
        // then handled by egui's own text editor.
        if ctx.egui_wants_keyboard_input() {
            return;
        }
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

    /// Sends the transport start; used by the visible buttons too.
    pub(crate) fn transport_start(&mut self) {
        if !self.playing {
            self.toggle_transport();
        }
    }

    /// Sends the transport stop; used by the visible buttons too.
    pub(crate) fn transport_stop(&mut self) {
        if self.playing {
            self.toggle_transport();
        }
    }
}

impl eframe::App for DrumApp {
    /// Runs *before* the UI pass: shortcuts and one-shot enables.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.handle_frame(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_ui(ui);
    }
}

impl DrumApp {
    /// Frame pass that must run before any drawing (works for eframe
    /// and for host-embedded editors alike).
    pub fn handle_frame(&mut self, ctx: &Context) {
        self.handle_shortcuts(ctx);
        if self.pattern_dirty {
            // Audio side re-hydrates the pattern only when it changed.
            if let Ok(json) = serde_json::to_string(&self.pattern) {
                self.send(RtCommand::SeqPattern { json });
            }
            self.pattern_dirty = false;
        }
    }

    /// Full UI pass on any root `Ui` — the plugin editors drive this
    /// directly from their host-provided frame.
    pub fn draw_ui(&mut self, ui: &mut egui::Ui) {
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
                let master =
                    ui.add(egui::Slider::new(&mut self.master_gain, 0.0..=2.0).text("Master"));
                if master.changed() {
                    self.send(RtCommand::MasterGain {
                        gain: self.master_gain,
                    });
                }
                ui.separator();
                ui.weak(&self.status);
            });
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Audio output snapshot from the host: the reason silence
                // has a face (device missing, stream refused, or OK).
                if let Some(audio) = &self.audio {
                    if audio.running {
                        ui.label(format!(
                            "🎧 {} Hz / {} ch",
                            audio.sample_rate, audio.channels
                        ));
                    } else {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 140, 60),
                            format!("🎧 silent: {}", audio.note),
                        );
                    }
                    ui.separator();
                }
                // Engine kit state mirror.
                let kit_generation = self
                    .watch
                    .as_ref()
                    .map(|watch| {
                        watch
                            .kit_generation
                            .load(std::sync::atomic::Ordering::Acquire)
                    })
                    .unwrap_or(0);
                if kit_generation == 0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 90, 90),
                        "⚠ no kit loaded (Kit tab)",
                    );
                } else {
                    ui.label(format!("🥁 kit v{}", kit_generation));
                }
                ui.separator();
                ui.label(format!("Tempo: {:.1} BPM", self.tempo));
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

/// What a host plugin editor needs to mirror into its persistent state
/// (nice-plug `#[persist]` fields on save).
pub struct EditorSnapshot {
    /// The kit path currently displayed in the entry (persists the
    /// user's intent even when the load failed).
    pub kit_path_entry: String,
    /// Path of the kit the engine actually received a summary for.
    pub loaded_kit_path: Option<String>,
    /// Current pattern (serialized by the host wrapper).
    pub pattern: Pattern,
    /// Master gain slider value 0.0..=8.0.
    pub master_gain: f32,
    /// Tempo slider value in BPM.
    pub tempo: f64,
}

impl DrumApp {
    pub(crate) fn producer(&mut self) -> std::sync::MutexGuard<'_, Producer<RtCommand>> {
        self.producer.lock().unwrap()
    }

    /// Persistence snapshot for host embeddings (run after each frame).
    pub fn editor_snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            kit_path_entry: self.kit_dir_entry.clone(),
            loaded_kit_path: self
                .kit_summary
                .as_ref()
                .map(|summary| summary.path.clone()),
            pattern: self.pattern.clone(),
            master_gain: self.master_gain,
            tempo: self.tempo,
        }
    }

    /// The seque transport switch shared with the editor shell.
    pub fn transport_running(&self) -> bool {
        self.playing
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
