// SPDX-License-Identifier: GPL-3.0-or-later
//! Sequencer tab: transport row (tempo, division, swing, humanize,
//! accent/ghost/flam params), lanes × steps grid with drag painting, bar
//! markers, MIDI import/export and undo/redo buttons.

use std::collections::HashMap;

use egui::{Color32, Sense, StrokeKind, Ui};

use dg_common::RtCommand;
use dg_sequencer::Division;

use crate::DrumApp;

/// The lengths offered by the grid length picker.
const LENGTHS: [usize; 4] = [16, 32, 64, 128];
/// Width/height of one step cell.
const CELL: f32 = 22.0;
const CELL_H: f32 = 18.0;

/// Draws the Sequencer tab.
pub fn show(app: &mut DrumApp, ui: &mut Ui) {
    ui.heading("Sequencer");
    ui.separator();
    ui.add_space(2.0);

    if app.pattern.lanes.is_empty() {
        if app.kit_summary.is_some() {
            ui.weak(
                "No lanes yet: press 'Adopt lanes from kit' to create one \
                 lane per midimap-bound instrument.",
            );
            if ui.button("Adopt lanes from kit").clicked() {
                app.adopt_kit_lanes();
            }
        } else {
            ui.weak(
                "Load a kit first (Kit tab), then the lanes adopt from \
                 its midimap bindings.",
            );
        }
    }

    transport_row(app, ui);
    ui.separator();
    grid(app, ui);
    ui.separator();
    midi_io_row(app, ui);
}

// ----------------------------------------------------------------------
// transport + pattern parameters
// ----------------------------------------------------------------------

fn transport_row(app: &mut DrumApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
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
        ui.label("Division:");
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

        ui.separator();
        let tempo = ui.add(
            egui::Slider::new(&mut app.tempo, 30.0..=360.0)
                .fixed_decimals(1)
                .text("BPM"),
        );
        if tempo.drag_stopped() || tempo.lost_focus() {
            let bpm = app.tempo;
            app.producer()
                .push(RtCommand::SeqTempo { bpm })
                .map_err(|_full| ())
                .ok();
            app.set_status(format!("Tempo → {bpm:.1} BPM"));
        }
    });

    ui.horizontal(|ui| {
        let swing = ui.add(egui::Slider::new(&mut app.pattern.swing, 0.0..=1.0).text("Swing"));
        let humanize_timing = ui.add(
            egui::Slider::new(&mut app.pattern.humanize_timing, 0.0..=1.0).text("Humanize Δt"),
        );
        let humanize_velocity = ui.add(
            egui::Slider::new(&mut app.pattern.humanize_velocity, 0.0..=1.0).text("Humanize vel"),
        );
        if swing.changed() || humanize_timing.changed() || humanize_velocity.changed() {
            app.mark_pattern_dirty();
        }
    });
    ui.horizontal(|ui| {
        let accent =
            ui.add(egui::Slider::new(&mut app.pattern.accent_gain, 0.5..=3.0).text("Accent gain"));
        let ghost =
            ui.add(egui::Slider::new(&mut app.pattern.ghost_gain, 0.0..=1.5).text("Ghost gain"));
        let flam_time = ui.add(egui::Slider::new(&mut app.pattern.flam_time, 0.0..=0.4).text("Flam Δ"));
        let flam_velocity =
            ui.add(egui::Slider::new(&mut app.pattern.flam_velocity, 0.1..=1.2).text("Flam gain"));
        if accent.changed() || ghost.changed() || flam_time.changed() || flam_velocity.changed() {
            app.mark_pattern_dirty();
        }
    });

    ui.horizontal(|ui| {
        if ui.button("↶ Undo").clicked() {
            app.undo_pattern();
        }
        if ui.button("↷ Redo").clicked() {
            app.redo_pattern();
        }
        ui.weak("[Space] Start/Stop · [Ctrl+Z/Y] undo/redo · [1-9] lane select");
    });
}

// ----------------------------------------------------------------------
// grid
// ----------------------------------------------------------------------

fn grid(app: &mut DrumApp, ui: &mut Ui) {
    if app.pattern.lanes.is_empty() {
        return;
    }
    let length = app.pattern.length;
    ui.add_space(4.0);
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        // Ruler: bar starts marked with the step number, others dimmed.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Lane").strong().small());
            for step in 0..length {
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(CELL - 2.0, CELL_H),
                    Sense::click(),
                );
                let bar_start = bar_start_index(app.pattern.division, step);
                let painter = ui.painter_at(rect);
                let text = if bar_start {
                    format!("{}", step + 1)
                } else {
                    "·".to_string()
                };
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(9.0),
                    if bar_start {
                        Color32::from_rgb(200, 200, 210)
                    } else {
                        Color32::from_rgb(90, 92, 100)
                    },
                );
                if response.clicked() {
                    app.selected_step = step;
                    app.mode = crate::Mode::Inspector;
                }
            }
        });

        // One row per lane.
        for lane_index in 0..app.pattern.lanes.len() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let selected_lane = app.selected_lane == lane_index;
                let note = app.pattern.lanes[lane_index].midi_note;
                let instrument = app
                    .kit_summary
                    .as_ref()
                    .map(|summary| summary.instrument_name(app.pattern.lanes[lane_index].instrument_id))
                    .unwrap_or("Instrument")
                    .to_owned();
                let lane_label = format!(
                    "{}{} ({})",
                    instrument,
                    if selected_lane { " ▶" } else { "" },
                    note
                );
                if ui.selectable_label(selected_lane, lane_label).clicked() {
                    app.selected_lane = lane_index;
                }

                draw_steps(app, ui, lane_index);
            });
        }
        ui.add_space(6.0);
        ui.weak(format!(
            "Selected: lane {} step {} · click = toggle · drag = paint · right-click = choke toggle",
            app.selected_lane, app.selected_step,
        ));
        let _ = length;
    });
}

fn bar_start_index(division: Division, step: usize) -> bool {
    let per_bar = (4.0 / division.beats_per_step()).round() as usize;
    per_bar > 0 && step % per_bar == 0
}

/// One row of step cells with click / right-click / drag painting.
fn draw_steps(app: &mut DrumApp, ui: &mut Ui, lane_index: usize) {
    let length = app.pattern.length;
    for step in 0..length {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(CELL - 2.0, CELL_H),
            Sense::click(),
        );
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        // Click toggles (a new edit): snapshot before the change.
        if response.clicked() {
            app.snapshot_pattern();
            let paint = !app
                .pattern
                .lanes
                .get(lane_index)
                .and_then(|lane| lane.steps.get(step))
                .map(|step_data| step_data.enabled)
                .unwrap_or(false);
            app.apply_step_paint(lane_index, step, paint);
            app.selected_lane = lane_index;
            app.selected_step = step;
        }

        // Right-click toggles the choke articulation.
        if response.secondary_clicked() {
            app.snapshot_pattern();
            let lane = &mut app.pattern.lanes[lane_index];
            lane.ensure_length(step + 1);
            lane.steps[step].choke = !lane.steps[step].choke;
            app.mark_pattern_dirty();
            app.selected_lane = lane_index;
            app.selected_step = step;
        }

        // Drag painting: the drag state is resolved once by the first
        // dragged-into cell; subsequent hovered cells receive that state.
        let primary_down = ui.input(|input| input.pointer.primary_down());
        if response.dragged() && app.drag_paint.is_none() {
            let current = app
                .pattern
                .lanes
                .get(lane_index)
                .and_then(|lane| lane.steps.get(step))
                .map(|step_data| step_data.enabled)
                .unwrap_or(false);
            app.drag_paint = Some(!current);
            app.drag_snapshot_pending = true;
        }
        if primary_down && app.drag_paint.is_some() && response.hovered() {
            if app.drag_snapshot_pending {
                app.drag_snapshot_pending = false;
                app.snapshot_pattern();
            }
            let paint = app.drag_paint.unwrap_or(true);
            app.apply_step_paint(lane_index, step, paint);
        }
        if app.drag_paint.is_some() && !primary_down {
            app.drag_paint = None;
        }

        paint_cell(ui, rect, app, lane_index, step);
    }
}

/// Paints one step cell: bar starts brighter, enabled cells filled by
/// velocity (accent orange, ghost grey-blue, normal blue), small glyph
/// for ratchet ×n / flam.
fn paint_cell(ui: &mut Ui, rect: egui::Rect, app: &DrumApp, lane_index: usize, step: usize) {
    let step_data = app
        .pattern
        .lanes
        .get(lane_index)
        .and_then(|lane| lane.steps.get(step));
    let bar_start = bar_start_index(app.pattern.division, step);
    let selected = app.selected_lane == lane_index && app.selected_step == step;

    let mut fill = Color32::from_rgb(38, 40, 46);
    let mut stroke_color = if bar_start {
        Color32::from_rgb(120, 122, 132)
    } else {
        Color32::from_rgb(52, 54, 60)
    };
    if let Some(step_data) = step_data {
        if step_data.enabled {
            let mix: u8 = ((step_data.velocity.clamp(0.0, 1.0)) * 25.0) as u8;
            let base = if step_data.accent {
                Color32::from_rgb(240, 150, 60)
            } else if step_data.ghost {
                Color32::from_rgb(105, 118, 138)
            } else {
                Color32::from_rgb(60, 120 + mix, 170 + mix)
            };
            fill = base;
            stroke_color = if selected {
                Color32::from_rgb(255, 255, 240)
            } else {
                stroke_color
            };
        } else if selected {
            stroke_color = Color32::from_rgb(180, 182, 192);
        }
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, fill);
    painter.rect_stroke(rect, 2.0, (2.0, stroke_color), StrokeKind::Middle);
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
                    egui::Align2::CENTER_CENTER,
                    glyph,
                    egui::FontId::proportional(9.0),
                    Color32::from_rgb(14, 14, 16),
                );
            }
        }
    }
}

// ----------------------------------------------------------------------
// MIDI import / export
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
                        Err(error) => {
                            app.set_status(format!("MIDI import failed: {error:?}"))
                        }
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
