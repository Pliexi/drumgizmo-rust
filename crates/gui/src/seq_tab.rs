// SPDX-License-Identifier: GPL-3.0-or-later
//! Sequencer tab, Channel-Rack style (FL Studio / EzDrummer drum grid):
//! a fixed instrument-name column, a beat-numbered ruler aligned to the
//! step grid, large square step cells grouped in fours, a live playhead
//! column, left-click/drag painting, right-click/right-drag erasing,
//! middle-click choke articulation and scroll-wheel velocity trimming.

use std::collections::HashMap;

use egui::{Align2, Color32, FontId, Sense, Ui};

use dg_common::MessageType;
use dg_common::RtCommand;
use dg_sequencer::Division;

use crate::DrumApp;

/// The lengths offered by the grid length picker.
const LENGTHS: [usize; 4] = [16, 32, 64, 128];
/// Fixed width of the instrument-name column (FL rack feel: every row
/// aligns to the very same column origin).
const NAME_W: f32 = 138.0;
const ROW_H: f32 = 26.0;
const CELL_W: f32 = 26.0;
/// Extra gap after every beat group (4 steps) and every bar.
const GROUP_GAP: f32 = 6.0;
const BAR_GAP: f32 = 10.0;
const CELL_GAP: f32 = 2.0;

/// Palette: lane colors cycle through the accent hues; the FL rack feel
/// comes from the colored fill on armed steps.
fn lane_color(index: usize) -> Color32 {
    const HUES: [(u8, u8, u8); 8] = [
        (80, 140, 210),
        (80, 170, 200),
        (200, 120, 70),
        (150, 120, 210),
        (110, 160, 90),
        (200, 100, 130),
        (100, 110, 200),
        (170, 150, 80),
    ];
    let (r, g, b) = HUES[index % HUES.len()];
    Color32::from_rgb(r, g, b)
}

/// Draws the Sequencer tab.
pub fn show(app: &mut DrumApp, ui: &mut Ui) {
    ui.heading("Sequencer — Channel Rack");
    ui.separator();
    ui.add_space(2.0);

    if app.pattern.lanes.is_empty() {
        if app.kit_summary.is_some() {
            ui.weak("No lanes yet: press 'Adopt lanes from kit' below.");
            if ui.button("Adopt lanes from kit").clicked() {
                app.adopt_kit_lanes();
            }
        } else {
            ui.weak("Load a kit first (Kit tab) — lanes adopt automatically.");
        }
        return;
    }

    transport_row(app, ui);
    ui.separator();
    grid(app, ui);
    ui.separator();
    midi_io_row(app, ui);
}

// ----------------------------------------------------------------------
// transport row (visible play controls, FL rack like)
// ----------------------------------------------------------------------

fn transport_row(app: &mut DrumApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        // Big transport button: play state also mirrors the engine watch.
        let engine_playing = app
            .watch
            .as_ref()
            .map(|watch| watch.playing.load(std::sync::atomic::Ordering::Acquire))
            .unwrap_or(app.playing);
        let label = if engine_playing { "⏹" } else { "▶" };
        let button = ui.add(
            egui::Button::new(egui::RichText::new(label).size(18.0)).min_size([46.0, 28.0].into()),
        );
        if button.clicked() {
            if engine_playing {
                app.transport_stop();
            } else {
                app.transport_start();
            }
        }
        if button.clicked() || ui.button("⏹ Stop").clicked() {
            app.transport_stop();
        }

        ui.label("BPM");
        let tempo = ui.add(
            egui::DragValue::new(&mut app.tempo)
                .speed(0.5)
                .range(30.0..=360.0),
        );
        if tempo.changed() || tempo.lost_focus() {
            let bpm = app.tempo;
            app.producer()
                .push(RtCommand::SeqTempo { bpm })
                .map_err(|_full| ())
                .ok();
        }

        ui.separator();
        ui.label("Steps:");
        for length in LENGTHS {
            if ui
                .selectable_label(app.pattern.length == length, format!("{length}"))
                .clicked()
                && app.pattern.length != length
            {
                app.snapshot_pattern();
                app.pattern.ensure_lengths(length);
                app.snap_selection_to_pattern();
                app.mark_pattern_dirty();
            }
        }

        ui.separator();
        ui.label("Grid:");
        for division in [
            Division::Quarter,
            Division::Eighth,
            Division::Sixteenth,
            Division::ThirtySecond,
            Division::SixtyFourth,
        ] {
            if ui
                .selectable_label(app.pattern.division == division, format!("{division:?}"))
                .clicked()
                && app.pattern.division != division
            {
                app.snapshot_pattern();
                app.pattern.division = division;
                app.mark_pattern_dirty();
            }
        }
    });

    ui.horizontal(|ui| {
        let swing = ui.add(egui::Slider::new(&mut app.pattern.swing, 0.0..=1.0).text("Swing"));
        let accent = ui
            .add(egui::Slider::new(&mut app.pattern.accent_gain, 0.5..=3.0).text("Accent"));
        let humanize_timing = ui.add(
            egui::Slider::new(&mut app.pattern.humanize_timing, 0.0..=1.0).text("Humanize Δt"),
        );
        if swing.changed() || accent.changed() || humanize_timing.changed() {
            app.mark_pattern_dirty();
        }
        if ui.button("↶").on_hover_text("Undo (Ctrl+Z)").clicked() {
            app.undo_pattern();
        }
        if ui.button("↷").on_hover_text("Redo (Ctrl+Y)").clicked() {
            app.redo_pattern();
        }
        ui.weak("click/drag = paint · right = erase · middle = choke · wheel = velocity · Shift+click = accent");
    });
}

// ----------------------------------------------------------------------
// channel-rack grid
// ----------------------------------------------------------------------

fn grid(app: &mut DrumApp, ui: &mut Ui) {
    let length = app.pattern.length;
    let division = app.pattern.division;
    let per_bar = (4.0 / division.beats_per_step()).round().max(1.0) as usize;
    let group = (per_bar / 4).max(1);

    // Playhead step (engine mirror) once per frame.
    let (watch_playing, watch_step) = read_watch(app, length);

    ui.add_space(4.0);
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        // Ruler: one block per bar with the bar number, aligned to the
        // exact same column offsets the step rows use.
        // Ruler: same spacing profile as the step rows so columns line up
        // exactly; bar starts carry their number.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            lane_label_area(ui);
            for step in 0..length {
                if step % per_bar == 0 && step > 0 {
                    ui.add_space(BAR_GAP);
                } else if step % group == 0 && step > 0 {
                    ui.add_space(GROUP_GAP);
                } else if step > 0 {
                    ui.add_space(CELL_GAP);
                }
                let label = if step % per_bar == 0 {
                    format!("{}", step / per_bar + 1)
                } else {
                    String::new()
                };
                let (rect, _response) =
                    ui.allocate_exact_size(egui::vec2(CELL_W, 14.0), Sense::hover());
                let color = if watch_playing && step == watch_step {
                    Color32::from_rgb(255, 220, 130)
                } else {
                    Color32::from_rgb(150, 152, 162)
                };
                if !label.is_empty() {
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        label,
                        FontId::proportional(10.0),
                        color,
                    );
                }
                if watch_playing && step == watch_step {
                    ui.painter()
                        .rect_filled(rect, 2.0, Color32::from_rgb(70, 90, 120));
                }
            }
        });

        // One row per lane: label cell then steps.
        for lane_index in 0..app.pattern.lanes.len() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                lane_label(app, ui, lane_index);
                step_row(
                    app,
                    ui,
                    lane_index,
                    per_bar,
                    group,
                    watch_playing,
                    watch_step,
                );
            });
        }

        ui.add_space(6.0);
        ui.weak(format!(
            "Selected: lane {} step {} · {} steps, {} per bar",
            app.selected_lane
                .min(app.pattern.lanes.len().saturating_sub(1)),
            app.selected_step.min(length.saturating_sub(1)),
            length,
            per_bar,
        ));
    });
}

/// Engine mirror snapshot, clamped to the pattern length.
fn read_watch(app: &DrumApp, length: usize) -> (bool, usize) {
    app.watch
        .as_ref()
        .map(|watch| {
            (
                watch.playing.load(std::sync::atomic::Ordering::Acquire),
                (watch.step_index.load(std::sync::atomic::Ordering::Acquire) as usize)
                    % length.max(1),
            )
        })
        .unwrap_or((app.playing, 0))
}

/// Name cell of one lane row: swatch, name, note — click previews the
/// instrument, right-click mutes/re-arms the lane (velocity 0 toggling).
fn lane_label(app: &mut DrumApp, ui: &mut Ui, lane_index: usize) {
    let note = app.pattern.lanes[lane_index].midi_note;
    let instrument = app
        .kit_summary
        .as_ref()
        .map(|summary| {
            summary
                .instrument_name(app.pattern.lanes[lane_index].instrument_id)
                .to_string()
        })
        .unwrap_or_else(|| format!("Instrument {lane_index}"));
    let color = lane_color(lane_index);

    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(NAME_W, ROW_H), Sense::click() | Sense::drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        3.0,
        if app.selected_lane == lane_index {
            Color32::from_rgb(40, 44, 54)
        } else {
            Color32::from_rgb(30, 32, 38)
        },
    );
    let swatch = egui::Rect::from_min_size(
        rect.min + [6.0, (ROW_H - 12.0) / 2.0].into(),
        [12.0, 12.0].into(),
    );
    painter.rect_filled(swatch, 2.0, color);
    painter.text(
        [rect.min.x + 24.0, rect.center().y].into(),
        Align2::LEFT_CENTER,
        format!("{instrument} ·{note}"),
        FontId::proportional(12.0),
        if app.selected_lane == lane_index {
            Color32::from_rgb(230, 232, 240)
        } else {
            Color32::from_rgb(150, 152, 162)
        },
    );

    if response.clicked() {
        app.selected_lane = lane_index;
        preview_hit(app, note);
    }
}

/// Ruler helper: name-sized placeholder keeps the ruler + rows aligned.
fn lane_label_area(ui: &mut Ui) {
    ui.allocate_exact_size(egui::vec2(NAME_W, ROW_H), Sense::hover());
}

/// Preview hit at a sensible loud velocity.
fn preview_hit(app: &mut DrumApp, note: u8) {
    app.send_kind(MessageType::OnSet, note, 114.0);
}

/// One row of step cells with FL painting semantics.
fn step_row(
    app: &mut DrumApp,
    ui: &mut Ui,
    lane_index: usize,
    per_bar: usize,
    group: usize,
    watch_playing: bool,
    watch_step: usize,
) {
    let length = app.pattern.length;
    for step in 0..length {
        // Spacing: bar > group > cell.
        if step % per_bar == 0 && step > 0 {
            ui.add_space(BAR_GAP);
        } else if step % group == 0 && step > 0 {
            ui.add_space(GROUP_GAP);
        } else if step > 0 {
            ui.add_space(CELL_GAP);
        }

        let (rect, response) = ui.allocate_exact_size(egui::vec2(CELL_W, ROW_H), Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        // Primary click: toggle (paint state may follow).
        if response.clicked() {
            app.snapshot_pattern();
            let current = step_enabled(app, lane_index, step);
            app.drag_paint = Some(!current);
            if !current {
                apply_paint(app, lane_index, step);
            } else {
                apply_clear(app, lane_index, step);
            }
            app.selected_lane = lane_index;
            app.selected_step = step;
        }

        // Right click: erase.
        if response.secondary_clicked() {
            app.snapshot_pattern();
            app.drag_paint = Some(false);
            apply_clear(app, lane_index, step);
            app.selected_lane = lane_index;
        }

        // Middle click: choke toggle (articulation modifier).
        if response.middle_clicked() {
            app.snapshot_pattern();
            let lane = &mut app.pattern.lanes[lane_index];
            lane.ensure_length(step + 1);
            lane.steps[step].choke = !lane.steps[step].choke;
            app.mark_pattern_dirty();
        }

        // Velocity with the wheel on hovered cells.
        if response.hovered() && step_enabled(app, lane_index, step) {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll.abs() > 0.01 {
                let key = (lane_index, step);
                if app.velocity_edited.as_ref() != Some(&key) {
                    app.snapshot_pattern();
                    app.velocity_edited = Some(key);
                }
                let lane = &mut app.pattern.lanes[lane_index];
                if let Some(step_data) = lane.steps.get_mut(step) {
                    step_data.velocity = (step_data.velocity + scroll * 0.002).clamp(0.05, 1.0);
                }
                app.mark_pattern_dirty();
            }
        }

        // Drag painting: once a paint mode exists, every neighbouring
        // hovered cell applies it while a button stays down.
        let pointer_down =
            ui.input(|input| input.pointer.primary_down() || input.pointer.secondary_down());
        if response.dragged() && app.drag_paint.is_none() {
            let current = step_enabled(app, lane_index, step);
            app.drag_paint = Some(!current);
            app.drag_snapshot_pending = true;
        }
        if pointer_down && app.drag_paint.is_some() && response.hovered() {
            let paint = app.drag_paint.unwrap_or(true);
            if app.drag_snapshot_pending {
                app.drag_snapshot_pending = false;
                app.snapshot_pattern();
            }
            if paint {
                apply_paint(app, lane_index, step);
            } else {
                apply_clear(app, lane_index, step);
            }
        }
        if app.drag_paint.is_some() && !pointer_down {
            app.drag_paint = None;
        }

        // Playhead highlight comes before the cell fill (overlay).
        let playhead = watch_playing && step == watch_step;
        paint_cell(ui, rect, app, lane_index, step, playhead);
    }
}

fn step_enabled(app: &DrumApp, lane_index: usize, step: usize) -> bool {
    app.pattern
        .lanes
        .get(lane_index)
        .and_then(|lane| lane.steps.get(step))
        .map(|step_data| step_data.enabled)
        .unwrap_or(false)
}

/// Sets a step on (velocity default) in an undo-safe manner.
fn apply_paint(app: &mut DrumApp, lane_index: usize, step: usize) {
    let default_velocity = app.default_step_velocity;
    if let Some(lane) = app.pattern.lanes.get_mut(lane_index) {
        lane.ensure_length(step + 1);
        let step_data = &mut lane.steps[step];
        step_data.enabled = true;
        step_data.velocity = default_velocity;
    }
    app.mark_pattern_dirty();
}

/// Clears enabled (choke markers with the step: cleaner rack visuals).
fn apply_clear(app: &mut DrumApp, lane_index: usize, step: usize) {
    if let Some(lane) = app.pattern.lanes.get_mut(lane_index) {
        if let Some(step_data) = lane.steps.get_mut(step) {
            step_data.enabled = false;
            step_data.choke = false;
        }
    }
    app.mark_pattern_dirty();
}

/// Paints one step cell: bar starts brighter, enabled cells filled by
/// panel lane color (FL-style: accent brighter), small glyphs.
fn paint_cell(
    ui: &mut Ui,
    rect: egui::Rect,
    app: &DrumApp,
    lane_index: usize,
    step: usize,
    playhead: bool,
) {
    let step_data = app
        .pattern
        .lanes
        .get(lane_index)
        .and_then(|lane| lane.steps.get(step));
    let per_bar = (4.0 / app.pattern.division.beats_per_step())
        .round()
        .max(1.0) as usize;
    let bar_start = step % per_bar == 0;
    let selected = app.selected_lane == lane_index && app.selected_step == step;

    let mut fill = if bar_start {
        Color32::from_rgb(44, 46, 54)
    } else {
        Color32::from_rgb(34, 36, 42)
    };
    let mut stroke_color = if bar_start {
        Color32::from_rgb(120, 122, 132)
    } else {
        Color32::from_rgb(50, 52, 60)
    };

    if let Some(step_data) = step_data {
        if step_data.enabled {
            let intensity = step_data.velocity.clamp(0.0, 1.0);
            let base = lane_color(lane_index);
            // Velocity brightens the lane color (saturating adds, u8 safe).
            let lift = (28.0 + 70.0 * intensity) as u8;
            fill = Color32::from_rgb(
                base.r().saturating_add(lift),
                base.g().saturating_add(lift / 2),
                base.b().saturating_add(lift / 4),
            );
            if step_data.accent {
                stroke_color = Color32::from_rgb(255, 210, 120);
            }
            if step_data.ghost {
                stroke_color = Color32::from_rgb(110, 118, 148);
            }
        }
    }
    if playhead {
        stroke_color = Color32::from_rgb(255, 220, 130);
    }
    if selected {
        stroke_color = Color32::from_rgb(255, 255, 240);
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, fill);
    painter.rect_stroke(rect, 3.0, (2.0, stroke_color), egui::StrokeKind::Middle);

    if let Some(step_data) = step_data {
        if step_data.enabled {
            let glyph = if let Some(repeat) = &step_data.repeat {
                format!("×{}", repeat.count)
            } else if step_data.flam {
                "f".to_string()
            } else if step_data.choke {
                "c".to_string()
            } else {
                String::new()
            };
            if !glyph.is_empty() {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    glyph,
                    FontId::proportional(9.0),
                    Color32::from_rgb(14, 14, 16),
                );
            }
        } else if step_data.choke {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "c",
                FontId::proportional(9.0),
                Color32::from_rgb(120, 122, 132),
            );
        }
    }
}

// ----------------------------------------------------------------------
// MIDI import / export (kept from the previous layout)
// ----------------------------------------------------------------------

fn midi_io_row(app: &mut DrumApp, ui: &mut Ui) {
    ui.add_space(6.0);
    ui.strong("MIDI");
    ui.horizontal(|ui| {
        ui.label("Import:");
        ui.text_edit_singleline(&mut app.midi_import_path);
        if ui.button("Load SMF → pattern").clicked() {
            let path = app.midi_import_path.clone();
            if path.is_empty() {
                app.set_status("No MIDI path given");
            } else {
                match std::fs::read(&path) {
                    Ok(bytes) => match dg_midi::parse_smf(&bytes) {
                        Ok(file) => {
                            // Note → lane slot mapping: existing lanes
                            // keep their slots by midi note; new notes
                            // append after them.
                            let division = app.pattern.division;
                            let length = app.pattern.length;
                            let mut mapping: HashMap<u8, usize> = HashMap::new();
                            let mut next_lane = app.pattern.lanes.len();
                            for note in &file.notes {
                                let existing = app
                                    .pattern
                                    .lanes
                                    .iter()
                                    .position(|lane| lane.midi_note == note.note);
                                mapping
                                    .entry(note.note)
                                    .or_insert(existing.unwrap_or_else(|| {
                                        let lane = next_lane;
                                        next_lane += 1;
                                        lane
                                    }));
                            }
                            let beats = length as f64 * division.beats_per_step();
                            let pattern = dg_midi::pattern_from_midi(
                                &file,
                                length,
                                division,
                                beats,
                                |note| mapping.get(&note).copied(),
                            );
                            app.replace_pattern(pattern);
                            app.set_status(format!(
                                "MIDI imported from {path} ({} notes)",
                                file.notes.len()
                            ));
                        }
                        Err(error) => app.set_status(format!("MIDI import failed: {error:?}")),
                    },
                    Err(error) => app.set_status(format!("MIDI read failed: {error}")),
                }
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Export:");
        ui.text_edit_singleline(&mut app.midi_export_path);
        if ui.button("Save pattern → SMF").clicked() {
            let path = app.midi_export_path.clone();
            if path.is_empty() {
                app.set_status("No export path given");
            } else {
                let notes = dg_midi::pattern_to_midi(&app.pattern);
                match dg_midi::write_smf(&notes, dg_midi::MIDI_TICKS_PER_BEAT, app.tempo) {
                    Ok(bytes) => {
                        if std::fs::write(&path, &bytes).is_ok() {
                            app.set_status(format!(
                                "MIDI exported to {path} ({} notes)",
                                notes.len()
                            ));
                        } else {
                            app.set_status(format!("MIDI write failed: {path}"));
                        }
                    }
                    Err(error) => app.set_status(format!("MIDI export failed: {error:?}")),
                }
            }
        }
    });
}
