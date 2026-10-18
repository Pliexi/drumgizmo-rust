// SPDX-License-Identifier: GPL-3.0-or-later
//! Kit tab: path entry, load button, and the instrument list with per
//! instrument gain sliders and midi preview clicks.

use egui::ScrollArea;

use dg_common::{MessageType, RtCommand};

use crate::DrumApp;

/// Draws the Kit tab content.
pub fn show(app: &mut DrumApp, ui: &mut egui::Ui) {
    {
        ui.heading("Kit");
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Kit path:");
            ui.text_edit_singleline(&mut app.kit_dir_entry);
            if ui.button("Load Kit").clicked() {
                app.request_kit_load(app.kit_dir_entry.clone());
            }
        });
        ui.weak(
            "Give the drumkit directory (or its drumkit.xml); the engine loads\
             asynchronously while the UI shows the instruments right away.",
        );

        if let Some(error) = app.kit_error.clone() {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), format!("Error: {error}"));
        }
        ui.separator();
        ui.add_space(6.0);

        if let Some(summary) = app.kit_summary.clone() {
            ui.heading(&summary.name);
            ui.label(format!("Path: {}", summary.path));
            ui.label(format!(
                "Instruments: {} · Channels: {} · Kit rate: {:?}",
                summary.instrument_count,
                summary.channel_count,
                summary.samplerate,
            ));
            if !summary.instruments.is_empty() {
                ui.add_space(12.0);
                ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("instrument_grid")
                        .striped(true)
                        .spacing([16.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("Id");
                            ui.strong("Instrument");
                            ui.strong("Group");
                            ui.strong("MIDI");
                            ui.strong("Gain");
                            ui.strong("Click");
                            ui.end_row();

                            for inst in &summary.instruments {
                                ui.label(format!("{}", inst.instrument_id));
                                ui.label(&inst.name);
                                ui.label(if inst.group.is_empty() {
                                    "—".to_owned()
                                } else {
                                    inst.group.clone()
                                });
                                ui.label(match inst.midi_note {
                                    Some(note) => format!("{note}"),
                                    None => "— (unbound)".to_owned(),
                                });

                                // Per instrument voice gain: local state, the
                                // command goes out when the drag ends (no
                                // command storm while dragging).
                                let id = inst.instrument_id;
                                let mut gain = app.voice_gains.get(id).copied().unwrap_or(1.0);
                                let slider = ui
                                    .add(egui::Slider::new(&mut gain, 0.0..=2.0).fixed_decimals(2));
                                if let Some(slot) = app.voice_gains.get_mut(id) {
                                    *slot = gain;
                                }
                                if slider.drag_stopped() || slider.lost_focus() {
                                    app.producer()
                                        .push(RtCommand::VoiceGain { instrument: id, gain })
                                        .map_err(|_| ())
                                        .ok();
                                    app.set_status(format!("Gain({}) = {gain:.2}", inst.name));
                                }

                                // Click: preview hit straight over the ring.
                                let note = inst.midi_note.unwrap_or(36);
                                if ui.button("Hit").clicked() {
                                    app.send_kind(MessageType::OnSet, note, 110.0);
                                }
                                ui.end_row();
                            }
                        });
                });
            }
        } else {
            ui.weak("No kit loaded yet — type a drumkit directory above.");
        }
    }
}
