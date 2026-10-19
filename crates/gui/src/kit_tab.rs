// SPDX-License-Identifier: GPL-3.0-or-later
//! Kit tab: path entry (with clipboard paste, drag&drop and an inline
//! directory browser), load button, and the instrument list with per
//! instrument gain sliders and midi preview clicks.

use egui::ScrollArea;

use dg_common::{MessageType, RtCommand};

use crate::DrumApp;

/// Cleans a pasted/typed path: trims whitespace, strips surrounding
/// quotes file managers love to include, and expands a leading `~`.
pub(crate) fn normalize_kit_path(raw: &str) -> String {
    let mut path = raw.trim().to_string();
    if path.len() >= 2
        && ((path.starts_with('"') && path.ends_with('"'))
            || (path.starts_with('\'') && path.ends_with('\'')))
    {
        path = path[1..path.len() - 1].trim().to_string();
    }
    if path == "~" {
        return std::env::var("HOME").unwrap_or(path);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            let joined = format!("{home}/{}", rest.trim_start_matches('/'));
            return normalize_trailing_slash(joined);
        }
    }
    normalize_trailing_slash(path)
}

/// Keeps `/` alone intact, otherwise drops one trailing slash.
fn normalize_trailing_slash(mut path: String) -> String {
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// Directory listing behind the inline browser: children of the visible
/// path's parent (or the path itself when it is a directory). Directories
/// first, then drumkit/instrument XML files; hidden names are skipped.
pub(crate) fn browse_entries(entry: &str) -> Vec<(String, bool)> {
    let normalized = normalize_kit_path(entry);
    let base = std::path::PathBuf::from(&normalized);
    let base = if base.is_dir() {
        base
    } else {
        base.parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/"))
    };
    let mut dirs: Vec<(String, bool)> = Vec::new();
    let mut xmls: Vec<(String, bool)> = Vec::new();
    let Ok(read) = std::fs::read_dir(&base) else {
        return dirs;
    };
    for child in read.flatten() {
        let name = child.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = child.path().is_dir();
        let is_xml = !is_dir && name.to_ascii_lowercase().ends_with(".xml");
        if is_dir {
            dirs.push((child.path().to_string_lossy().into_owned(), true));
        } else if is_xml {
            xmls.push((child.path().to_string_lossy().into_owned(), false));
        }
    }
    dirs.sort();
    xmls.sort();
    let mut entries = dirs;
    entries.extend(xmls);
    // Bound the list so a huge / keeps the frame cheap.
    entries.truncate(120);
    entries
}

/// Draws the Kit tab content.
pub fn show(app: &mut DrumApp, ui: &mut egui::Ui) {
    {
        ui.heading("Kit");
        ui.separator();

        // Drag & drop a kit.xml / folder from any file manager.
        let dropped = ui.ctx().input(|input| input.raw.dropped_files.clone());
        if let Some(file) = dropped.first() {
            let path = file.path().display().to_string();
            app.set_status(format!("Dropped: {path}"));
            app.request_kit_load(path);
        }
        let hovered = ui.ctx().input(|input| input.raw.hovered_files.clone());
        if !hovered.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(150, 200, 255),
                "Release to load this kit…",
            );
        }

        ui.horizontal(|ui| {
            ui.label("Kit path:");
            ui.add(egui::TextEdit::singleline(&mut app.kit_dir_entry).desired_width(320.0));
            if ui.button("Load Kit").clicked() {
                app.request_kit_load(normalize_kit_path(&app.kit_dir_entry));
            }
            if ui
                .button("Paste")
                .on_hover_text("Insert the path from the system clipboard (Ctrl+V)")
                .clicked()
            {
                app.paste_kit_path();
            }
            // Native portal/GTK file chooser: exactly the explorer the
            // desktop provides, not another homemade tree.
            if ui
                .button("📂 Open file chooser…")
                .on_hover_text("Pick a kit directory in the system file dialog")
                .clicked()
            {
                match rfd::FileDialog::new()
                    .add_filter("DrumGizmo kit", &["xml"])
                    .set_title("Select kit directory (or drumkit.xml)")
                    .pick_folder()
                {
                    Some(path) => {
                        let path = path.display().to_string();
                        app.kit_dir_entry = path.clone();
                        app.request_kit_load(crate::kit_tab::normalize_kit_path(&path));
                    }
                    None => {
                        // The portal refused (missing xdg-desktop-portal,
                        // headless session…) — keep the manual paths.
                        app.set_status(
                            "File chooser unavailable: type a path, or use the directory browser",
                        );
                    }
                }
            }
        });
        ui.weak(
            "Give the drumkit directory (or its drumkit.xml); the engine loads\
             asynchronously while the UI shows the instruments right away. Drag\
             a file here, or use Paste — even under Wayland portals.",
        );

        if let Some(error) = app.kit_error.clone() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 90, 90),
                format!("Error: {error}"),
            );
        }

        // Inline directory browser: click around the filesystem without
        // leaving the window ( Centre-click works even when the desktop
        // clipboard is unavailable).
        egui::CollapsingHeader::new("Browse directories")
            .default_open(false)
            .show(ui, |ui| {
                ui.set_min_width(320.0);
                for (path, is_dir) in browse_entries(&app.kit_dir_entry) {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone());
                    let label = if is_dir { format!("▸ {name}") } else { name };
                    if ui.button(label).clicked() {
                        if is_dir {
                            app.kit_dir_entry = path.clone();
                        } else {
                            app.request_kit_load(path);
                        }
                    }
                }
            });
        ui.separator();
        ui.separator();
        ui.add_space(6.0);

        if let Some(summary) = app.kit_summary.clone() {
            ui.heading(&summary.name);
            ui.label(format!("Path: {}", summary.path));
            ui.label(format!(
                "Instruments: {} · Channels: {} · Kit rate: {:?}",
                summary.instrument_count, summary.channel_count, summary.samplerate,
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
                                        .push(RtCommand::VoiceGain {
                                            instrument: id,
                                            gain,
                                        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizer_trims_and_strips_quotes() {
        assert_eq!(
            normalize_kit_path("   ' /home/pepper/kits/drums '  "),
            "/home/pepper/kits/drums"
        );
        assert_eq!(normalize_kit_path("\" /home/p/drums \""), "/home/p/drums");
        assert_eq!(normalize_kit_path("/plain"), "/plain");
    }

    #[test]
    fn normalizer_expands_tilde_with_injected_home() {
        // Direct overseas exercise of the homedir branches through a tempdir
        // HOME so the test never depends on the runner's environment.
        let home = std::env::temp_dir().join("dg-gui-home-test");
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(home.join("kits")).unwrap();

        unsafe { std::env::set_var("HOME", &home) };
        assert_eq!(normalize_kit_path("~"), home.display().to_string());
        assert_eq!(
            normalize_kit_path("~/kits/"),
            home.join("kits").display().to_string()
        );
        unsafe { std::env::remove_var("HOME") };
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn browse_lists_dirs_first_and_skips_dotfiles() {
        let base = std::env::temp_dir().join("dg-gui-browse-test");
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("b_dir")).unwrap();
        std::fs::create_dir_all(base.join("a_dir/.hidden")).unwrap();
        std::fs::write(base.join("drumkit.xml"), b"<drumkit/>").unwrap();
        std::fs::write(base.join("readme.txt"), b"x").unwrap();
        std::fs::write(base.join(".secret"), b"x").unwrap();

        let entries = browse_entries(base.to_str().unwrap());
        let names: Vec<&str> = entries
            .iter()
            .map(|(path, _)| {
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap()
            })
            .collect();
        assert_eq!(
            names,
            ["a_dir", "b_dir", "drumkit.xml"],
            "dirs first, xml files after, dotfiles skipped"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
