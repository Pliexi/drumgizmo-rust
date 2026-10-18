// SPDX-License-Identifier: GPL-3.0-or-later
//! Step inspector tab: per-step velocity/probability/micro-timing,
//! accent/ghost/choke/flam toggles, and the ratchet (repeat) settings of
//! the step selected in the sequencer grid. Slider drags count as one
//! undo edit (snapshot on drag end).

use egui::ScrollArea;

use dg_sequencer::Step;

use crate::DrumApp;

/// Draws the Inspector tab.
pub fn show(app: &mut DrumApp, ui: &mut egui::Ui) {
    ui.heading("Inspector");
    ui.separator();
    ui.add_space(2.0);

    if app.pattern.lanes.is_empty() {
        ui.weak("No lanes: load a kit (Kit tab) or import a pattern.");
        return;
    }
    let lane_count = app.pattern.lanes.len();
    app.selected_lane = app.selected_lane.min(lane_count - 1);
    app.selected_step = app.selected_step.min(app.pattern.length - 1);

    ui.horizontal(|ui| {
        ui.label("Lane:");
        ui.add(egui::Slider::new(&mut app.selected_lane, 0..=lane_count - 1).integer());
        if ui.button("◀ step").clicked() {
            app.selected_step = app.selected_step.saturating_sub(1);
        }
        ui.label(format!(
            "Step {} / {}",
            app.selected_step + 1,
            app.pattern.length
        ));
        if ui.button("step ▶").clicked() {
            app.selected_step = (app.selected_step + 1).min(app.pattern.length - 1);
        }
    });

    let instrument_name = app
        .kit_summary
        .as_ref()
        .map(|summary| summary.instrument_name(app.pattern.lanes[app.selected_lane].instrument_id))
        .unwrap_or("Instrument")
        .to_owned();
    let midi_note = app.pattern.lanes[app.selected_lane].midi_note;
    let step_index = app.selected_step;
    ui.add_space(8.0);
    ui.strong(format!(
        "{instrument_name} · MIDI note {midi_note} · step {step_index}"
    ));

    if app.pattern.lanes[app.selected_lane].steps.len() <= step_index {
        ui.add_space(4.0);
        ui.weak("Step is beyond the lane length; click the grid to add it.");
        return;
    }
    if !app.pattern.lanes[app.selected_lane].steps[step_index].enabled {
        ui.add_space(4.0);
        ui.weak("Step is off — enable below or click it in the sequencer grid.");
    }

    let before = app.pattern.lanes[app.selected_lane].steps[step_index].clone();
    let mut step = before.clone();
    ui.add_space(4.0);
    step_sliders(ui, &mut step, app);
    step_toggles(ui, &mut step, app);
    if step != before {
        app.with_lane_step(app.selected_lane, step_index, |slot| {
            *slot = step.clone();
        });
        app.mark_pattern_dirty();
    }

    ui.add_space(12.0);
    ScrollArea::vertical().show(ui, |ui| {
        ui.strong("Pattern context");
        ui.weak(format!(
            "Accent gain {:.2} · ghost gain {:.2} · flam Δ{:.03} ×{:.2} · \
             swing {:.2} · humanize {:.2}/{:.2}",
            app.pattern.accent_gain,
            app.pattern.ghost_gain,
            app.pattern.flam_time,
            app.pattern.flam_velocity,
            app.pattern.swing,
            app.pattern.humanize_timing,
            app.pattern.humanize_velocity,
        ));
    });
}

/// Slider view of the step values (drag end = one undo snapshot).
fn step_sliders(ui: &mut egui::Ui, step: &mut Step, _app: &mut DrumApp) {
    let velocity = ui.add(egui::Slider::new(&mut step.velocity, 0.05..=1.0).text("Velocity"));
    let probability =
        ui.add(egui::Slider::new(&mut step.probability, 0.03..=1.0).text("Probability"));
    let micro = ui
        .add(egui::Slider::new(&mut step.micro_offset, -0.45..=0.45).text("Micro offset (steps)"));
    if velocity.drag_stopped() || probability.drag_stopped() || micro.drag_stopped() {
        _app.set_status("Step edited (drag finished)");
    }
}

/// Toggle row for the step articulation flags and the ratchet controls.
fn step_toggles(ui: &mut egui::Ui, step: &mut Step, app: &mut DrumApp) {
    ui.horizontal(|ui| {
        ui.toggle_value(&mut step.enabled, "On");
        ui.toggle_value(&mut step.accent, "Accent");
        ui.toggle_value(&mut step.ghost, "Ghost");
        ui.toggle_value(&mut step.choke, "Choke");
        ui.toggle_value(&mut step.flam, "Flam");
    });
    ui.add_space(4.0);
    ui.weak("Repeat (ratchet): the step is divided into equal parts, extra hits gain falloff^k.");
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if let Some(repeat) = step.repeat.as_mut() {
            ui.add(
                egui::Slider::new(&mut repeat.count, 2..=8)
                    .integer()
                    .text("Ratchet"),
            );
            ui.add(egui::Slider::new(&mut repeat.falloff, 0.05..=0.95).text("Falloff"));
            if ui.button("Remove ratchet").clicked() {
                app.snapshot_pattern();
                step.repeat = None;
            }
        } else if ui.button("Add ratchet ×2").clicked() {
            app.snapshot_pattern();
            step.repeat = Some(dg_sequencer::Repeat::default());
        }
        if ui.button("velocity → 0.8").clicked() {
            app.snapshot_pattern();
            step.velocity = 0.8;
        }
        if ui.button("Clear step").clicked() {
            app.snapshot_pattern();
            *step = Step::default();
        }
    });
}
