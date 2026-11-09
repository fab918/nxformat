use super::*;
use egui::{Color32, RichText};

impl App {
    fn choose_sources(&mut self, folder: bool) {
        if folder {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                self.scan(vec![path]);
            }
        } else if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Decrypted games", &["dnsp", "dxci", "nsp", "xci"])
            .pick_files()
        {
            self.scan(paths);
        }
    }

    fn choose_archive(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("NX archives", &["nx"])
            .pick_file()
        {
            self.open_archive(path);
        }
    }

    fn empty(&mut self, ui: &mut egui::Ui, archive: bool) {
        ui.add_space((ui.available_height() * 0.06).clamp(8.0, 40.0));
        ui.vertical_centered(|ui| {
            theme::logo(ui, 56.0);
            ui.add_space(6.0);
            ui.label(theme::tagline(if archive {
                "A closer look at your archive."
            } else {
                "Your games. One archive."
            }));
            ui.add_space(26.0);
            let width = ui.available_width().min(620.0);
            theme::card().show(ui, |ui| {
                ui.set_min_width(width - 40.0);
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    theme::icon(
                        ui,
                        if archive { Icon::Archive } else { Icon::Folder },
                        48.0,
                        ACCENT,
                    );
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(if archive {
                            "Drop an NX archive here"
                        } else {
                            "Drop an NX, a folder or decrypted files"
                        })
                        .size(19.0)
                        .strong(),
                    );
                    ui.label(theme::muted(if archive {
                        ".nx"
                    } else {
                        "NX archives · Extracted folders · DNSP · DXCI"
                    }));
                    ui.add_space(14.0);
                    if archive {
                        if theme::button(ui, Icon::Archive, "Open archive", true).clicked() {
                            self.choose_archive();
                        }
                    } else {
                        // Centre the two buttons with the row width measured last frame.
                        let row_id = ui.scope_id().with("source-buttons");
                        let row_width = ui.data(|d| d.get_temp::<f32>(row_id)).unwrap_or(300.0);
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - row_width) / 2.0).max(0.0));
                            let start = ui.cursor().min.x;
                            if theme::button(ui, Icon::Folder, "Choose folder", true).clicked() {
                                self.choose_sources(true);
                            }
                            let files = theme::button(ui, Icon::File, "Choose files", false);
                            if files.clicked() {
                                self.choose_sources(false);
                            }
                            let measured = files.rect.max.x - start;
                            if (measured - row_width).abs() > 0.5 {
                                ui.data_mut(|d| d.insert_temp(row_id, measured));
                                ui.ctx().request_repaint();
                            }
                        });
                    }
                    ui.add_space(12.0);
                });
            });
            ui.add_space(10.0);
            if !archive && ui.link("Already have an NX? Open its contents").clicked() {
                self.choose_archive();
            }
            ui.add_space(10.0);
            ui.label(
                theme::muted(format!(
                    "Processed locally. Source files stay untouched.  ·  v{}",
                    env!("CARGO_PKG_VERSION")
                ))
                .small(),
            );
            let created = if archive {
                None
            } else {
                self.last_output.clone()
            };
            if let Some(path) = created {
                ui.add_space(14.0);
                ui.label(RichText::new(&self.status).color(ACCENT));
                if theme::button(ui, Icon::Archive, "View created archive", false).clicked() {
                    self.open_archive(path);
                }
            } else if !self.status.is_empty() {
                ui.add_space(14.0);
                ui.label(theme::muted(&self.status));
            }
        });
    }

    fn source_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Sources");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    self.sources.clear();
                    self.textures.clear();
                    self.appearances.clear();
                    self.omitted_firmware.clear();
                    self.focused.clear();
                }
                if theme::button(ui, Icon::Folder, "Folder", false).clicked() {
                    self.choose_sources(true);
                }
                if theme::button(ui, Icon::Plus, "Files", false).clicked() {
                    self.choose_sources(false);
                }
            });
        });
        ui.add_space(12.0);
        let games = self.games();
        if !games.iter().any(|g| g.key == self.focused) {
            self.focused = games.first().map(|g| g.key.clone()).unwrap_or_default();
        }
        let mut removed = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for game in &games {
                    let focused = self.focused == game.key;
                    let frame = theme::card().stroke(egui::Stroke::new(
                        1.0,
                        if focused {
                            Color32::from_rgb(95, 81, 132)
                        } else {
                            theme::BORDER
                        },
                    ));
                    frame.show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let texture = self
                                .appearances
                                .get(&game.key)
                                .and_then(|a| a.texture.as_ref())
                                .or_else(|| {
                                    game.sources
                                        .iter()
                                        .find_map(|&i| self.textures.get(&self.sources[i].path))
                                });
                            if let Some(texture) = texture {
                                ui.add(
                                    egui::Image::new(texture)
                                        .fit_to_exact_size(egui::vec2(56.0, 56.0))
                                        .corner_radius(9),
                                );
                            } else {
                                theme::brand(ui, 56.0);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let remove = ui
                                        .add(
                                            egui::Button::new("")
                                                .fill(Color32::from_rgb(168, 40, 53))
                                                .min_size(egui::vec2(34.0, 34.0)),
                                        )
                                        .on_hover_text("Remove this game from the list");
                                    remove.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::Role::Button,
                                            true,
                                            "Remove this game from the list",
                                        )
                                    });
                                    theme::paint_icon(
                                        ui.painter(),
                                        egui::Rect::from_center_size(
                                            remove.rect.center(),
                                            egui::Vec2::splat(18.0),
                                        ),
                                        Icon::Trash,
                                        Color32::WHITE,
                                    );
                                    if remove.clicked() {
                                        removed = Some(game);
                                    }
                                    if ui.selectable_label(focused, "Configure").clicked() {
                                        self.focused = game.key.clone();
                                    }
                                    ui.with_layout(
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&game.name).size(18.0).strong(),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(&game.name);
                                            ui.label(
                                                theme::muted(if game.tree {
                                                    "Extracted folder".into()
                                                } else {
                                                    format!(
                                                        "{:016X} · {} source{}",
                                                        game.application,
                                                        game.sources.len(),
                                                        if game.sources.len() == 1 {
                                                            ""
                                                        } else {
                                                            "s"
                                                        }
                                                    )
                                                })
                                                .small(),
                                            );
                                        },
                                    );
                                },
                            );
                        });
                        ui.add_space(10.0);
                        for &index in &game.sources {
                            let source = &mut self.sources[index];
                            ui.horizontal(|ui| {
                                ui.add_enabled(
                                    source.supported,
                                    egui::Checkbox::without_text(&mut source.selected),
                                );
                                ui.vertical(|ui| {
                                    let types = source
                                        .contents
                                        .iter()
                                        .map(|c| {
                                            if c.display.is_empty() {
                                                format!("{} v{}", kind_name(&c.kind), c.version)
                                            } else {
                                                format!("{} {}", kind_name(&c.kind), c.display)
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" · ");
                                    ui.label(types);
                                    ui.add(
                                        egui::Label::new(
                                            theme::muted(file_name(&source.path)).small(),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(text(&source.path));
                                });
                                if !source.tree {
                                    ui.label(theme::muted(bytes(source.size)).small());
                                }
                            });
                            if !source.issue.is_empty() {
                                ui.colored_label(ui.visuals().warn_fg_color, &source.issue);
                            }
                        }
                        let firmware_bytes = self.firmware_bytes(game);
                        if firmware_bytes != 0 {
                            let mut include = !self.omits_firmware(game);
                            if ui.add_enabled(
                                self.include_firmware,
                                egui::Checkbox::new(&mut include, format!("Bundled firmware · {}", bytes(firmware_bytes))),
                            )
                            .on_disabled_hover_text("Enable bundled firmware in Configuration to include it.")
                            .on_hover_text("Keep the system-update partition. Omitting it skips exact reconstruction for this game.")
                            .changed() {
                                if include {
                                    self.omitted_firmware.remove(&game.key);
                                } else {
                                    self.omitted_firmware.insert(game.key.clone());
                                }
                            }
                            ui.label(
                                theme::muted(if self.omits_firmware(game) {
                                    if self.recipe {
                                        "Without firmware this game gets no rebuild data."
                                    } else {
                                        "Firmware omitted."
                                    }
                                } else {
                                    "With firmware the NX needs an emulator built with NXFormat 1.0.1 or later."
                                })
                                .small(),
                            );
                        }
                        ui.add_space(6.0);
                        if let Some(output) = &game.output {
                            ui.add(
                                egui::Label::new(
                                    theme::muted(format!("Output: {}", file_name(output))).small(),
                                )
                                .truncate(),
                            )
                            .on_hover_text(text(output));
                        } else {
                            ui.colored_label(ui.visuals().warn_fg_color, &game.problem);
                        }
                    });
                    ui.add_space(10.0);
                }
                for source in self.sources.iter().filter(|s| s.contents.is_empty()) {
                    theme::card().show(ui, |ui| {
                        ui.strong(file_name(&source.path));
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            if source.issue.is_empty() {
                                "Unsupported source"
                            } else {
                                &source.issue
                            },
                        );
                    });
                }
            });
        if let Some(game) = removed {
            self.remove_game(game);
        }
    }

    fn configuration(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui,|ui| {
            ui.horizontal(|ui| {theme::icon(ui,Icon::Settings,18.0,ACCENT);ui.label(RichText::new("Configuration").size(18.0).strong());});
            ui.add_space(16.0);
            ui.strong("Save location");
            let folder=self.output_dir.as_ref().map(|p|text(p)).unwrap_or_else(||"Beside each source".into());
            ui.add(egui::Label::new(theme::muted(&folder)).truncate()).on_hover_text(&folder);
            ui.horizontal(|ui| {
                if ui.button("Choose folder…").clicked(){if let Some(path)=rfd::FileDialog::new().pick_folder(){self.output_dir=Some(path);}}
                if self.output_dir.is_some() && ui.small_button("Reset").clicked(){self.output_dir=None;}
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.strong("Compression");
                theme::info(
                    ui,
                    &["Compression", "None: store game data without compression.", "Small / Balanced / Maximum: increasing compression effort.", "Balanced is recommended for most games."],
                );
            });
            ui.horizontal(|ui| {
                if ui.selectable_label(self.no_compress,"None").clicked(){self.no_compress=true;}
                for (level,label) in [(1,"Small"),(3,"Balanced"),(13,"Maximum")] {
                    if ui.selectable_label(!self.no_compress && self.level==level,label).clicked(){self.no_compress=false;self.level=level;}
                }
            });
            ui.label(theme::muted("Balanced is recommended for most games.").small());
            let games=self.games();
            let has_trees=games.iter().any(|g|g.tree);
            // Rebuild data and firmware exist only for DNSP/DXCI packages.
            let has_packages=games.iter().any(|g|self.native_package(g));
            if games.iter().any(|game| self.firmware_bytes(game) != 0) {
                ui.checkbox(&mut self.include_firmware, "Include bundled firmware")
                    .on_hover_text("Allow bundled firmware for all games. Choose which games include it in Sources.");
            }
            if has_packages {
                ui.checkbox(&mut self.recipe, "Keep original rebuild data")
                    .on_hover_text("Keep a recipe to rebuild native package files byte for byte.");
                if self.recipe && games.iter().any(|game| self.omits_firmware(game)) {
                    ui.label(theme::muted("Rebuild data is skipped for games whose firmware is omitted.").small());
                }
            }
            if has_trees {
                ui.add_space(10.0);ui.strong("Extracted folder platform");
                egui::ComboBox::from_id_salt("tree-platform").selected_text(["Automatic","Switch 1","Switch 2"][self.platform]).show_ui(ui,|ui|{
                    for (i,label) in ["Automatic","Switch 1","Switch 2"].iter().enumerate(){ui.selectable_value(&mut self.platform,i,*label);}
                });
                            }
            if let Some(game)=games.iter().find(|g|g.key==self.focused) {
                let has_icon=game.sources.iter().any(|&i|self.sources[i].icon.is_some());
                if game.missing_title || !has_icon {
                    ui.add_space(14.0);ui.separator();ui.add_space(6.0);
                    ui.strong("Make it yours");
                    ui.label(theme::muted("Fill in missing display information.").small());
                    let appearance=self.appearances.entry(game.key.clone()).or_default();
                    if game.missing_title {ui.add(egui::TextEdit::singleline(&mut appearance.title).hint_text(game.name.as_str()).desired_width(f32::INFINITY));}
                    if !has_icon {
                        if let Some(texture)=&appearance.texture {ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(80.0,80.0)).corner_radius(9));}
                        let key=game.key.clone();
                        let choose=theme::button(ui,Icon::Image,if appearance.icon.is_some(){"Change artwork"}else{"Choose artwork"},false).clicked();
                        if appearance.icon.is_some() && ui.small_button("Remove artwork").clicked(){appearance.icon=None;appearance.texture=None;}
                        if choose {if let Some(path)=rfd::FileDialog::new().add_filter("Artwork",&["png","jpg","jpeg"]).pick_file(){self.load_artwork(key,path);}}
                        ui.label(theme::muted("Square · 256 × 256 px recommended\nJPEG or PNG · Crop before saving").small());
                    }
                }
            }
            ui.add_space(20.0);
            let ready=games.iter().filter(|g|g.output.is_some()).count();
            ui.add_enabled_ui(ready>0,|ui| {
                if theme::button(ui,Icon::Arrow,&format!("Convert {ready} game{}",if ready==1 {""} else {"s"}),true).clicked(){self.last_output=None;self.convert();}
            });
            self.activity(ui);
        });
    }

    /// The last status line and the command log.
    fn activity(&self, ui: &mut egui::Ui) {
        if !self.status.is_empty() {
            ui.add_space(10.0);
            ui.add(egui::Label::new(theme::muted(&self.status).small()).wrap());
        }
        if !self.log.is_empty() {
            egui::CollapsingHeader::new(
                theme::muted(format!("Activity ({})", self.log.len())).small(),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(110.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    });
            });
        }
    }

    fn progress_view(&mut self, ui: &mut egui::Ui) {
        let Some(running) = &self.running else {
            return;
        };
        let fraction = if running.total > 0 {
            (running.done as f64 / running.total as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        ui.add_space((ui.available_height() * 0.16).min(80.0));
        ui.vertical_centered(|ui| {
            theme::brand(ui, 64.0);
            ui.add_space(20.0);
            ui.heading(&running.job.label);
            ui.add_space(12.0);
            if running.total > 0 {
                ui.label(
                    RichText::new(format!("{:.0}%", fraction * 100.0))
                        .size(48.0)
                        .color(ACCENT),
                );
            } else {
                ui.spinner();
                ui.label(theme::muted("Preparing and checking your content…"));
            }
            ui.add_space(12.0);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(ui.available_width().min(480.0))
                    .fill(ACCENT)
                    .animate(running.total == 0),
            );
            ui.add_space(10.0);
            ui.label(theme::muted(format!(
                "{} elapsed{}",
                format_elapsed(running.started.elapsed().as_secs()),
                if self.queue.is_empty() {
                    String::new()
                } else {
                    format!(" · {} queued", self.queue.len())
                }
            )));
            if running.total > 0 {
                ui.label(theme::muted(format!(
                    "{} / {}",
                    bytes(running.done),
                    bytes(running.total)
                )));
            }
            ui.add_space(20.0);
            ui.add_enabled_ui(!running.cancel.is_cancelled(), |ui| {
                if ui.button("Cancel operation").clicked() {
                    running.cancel.cancel();
                }
            });
            if running.cancel.is_cancelled() {
                ui.label(theme::muted("Cancelling safely…"));
            }
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn archive_view(&mut self, ui: &mut egui::Ui) {
        let Some(archive) = &self.archive else {
            self.empty(ui, true);
            return;
        };
        let path = text(&archive.path);
        let mut action = None;
        ui.horizontal(|ui| {
            if let Some(texture) = &self.archive_cover {
                ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(egui::vec2(80.0, 80.0))
                        .corner_radius(12),
                );
            } else {
                theme::brand(ui, 80.0);
            }
            ui.vertical(|ui| {
                let title = archive
                    .snapshots
                    .iter()
                    .find(|s| !s.name.is_empty())
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| file_name(&archive.path));
                ui.heading(title);
                ui.label(theme::muted(format!(
                    "{} · {} snapshots",
                    bytes(archive.size),
                    archive.snapshots.len()
                )));
                ui.add(egui::Label::new(theme::muted(file_name(&archive.path)).small()).truncate())
                    .on_hover_text(&path);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Open another…").clicked() {
                    action = Some(("open".into(), vec![]));
                }
            });
        });
        ui.add_space(18.0);
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, Icon::Plus, "Add update / DLC", false).clicked() {
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter("Decrypted games", &["dnsp", "dxci", "nsp", "xci"])
                    .pick_files()
                {
                    let mut args = vec!["add".into(), path.clone()];
                    args.extend(files.iter().map(|p| text(p)));
                    if !self.recipe {
                        args.push("--no-recipe".into());
                    }
                    action = Some(("Adding content".into(), args));
                }
            }
            if theme::button(ui, Icon::Check, "Verify", false).clicked() {
                action = Some((
                    "Verifying archive".into(),
                    vec!["verify".into(), path.clone()],
                ));
            }
            if ui
                .button("Compact")
                .on_hover_text("Reclaim space left by removed content.")
                .clicked()
            {
                action = Some((
                    "Compacting archive".into(),
                    vec!["compact".into(), path.clone()],
                ));
            }
        });
        if archive.reclaimable > 0 {
            ui.label(theme::muted(format!(
                "{} can be reclaimed by compaction",
                bytes(archive.reclaimable)
            )));
        }
        ui.add_space(16.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for snapshot in &archive.snapshots {
                theme::card().inner_margin(14).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        theme::icon(
                            ui,
                            if snapshot.kind == "base" {
                                Icon::Archive
                            } else {
                                Icon::File
                            },
                            22.0,
                            ACCENT,
                        );
                        ui.vertical(|ui| {
                            ui.strong(format!(
                                "{} {}",
                                kind_name(&snapshot.kind),
                                if snapshot.display.is_empty() {
                                    format!("v{}", snapshot.version)
                                } else {
                                    snapshot.display.clone()
                                }
                            ));
                            ui.label(
                                theme::muted(format!(
                                    "{:016X}",
                                    if snapshot.kind == "base" {
                                        snapshot.application
                                    } else {
                                        snapshot.title
                                    }
                                ))
                                .small(),
                            );
                        });
                        if snapshot.kind != "base" {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Remove").clicked() {
                                        self.remove_confirmation = Some((
                                            format!(
                                                "Remove {} v{}?",
                                                kind_name(&snapshot.kind),
                                                snapshot.version
                                            ),
                                            vec![
                                                "remove".into(),
                                                path.clone(),
                                                snapshot.identity.clone(),
                                            ],
                                        ));
                                    }
                                },
                            );
                        }
                    });
                });
                ui.add_space(8.0);
            }
            if !archive.originals.is_empty() {
                ui.add_space(12.0);
                ui.strong("Original files");
                ui.label(theme::muted("Rebuild the original packages byte for byte."));
            }
            for original in &archive.originals {
                ui.horizontal(|ui| {
                    ui.label(&original.name);
                    ui.label(theme::muted(bytes(original.size)));
                    if ui.button("Rebuild…").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            action = Some((
                                format!("Rebuilding {}", original.name),
                                vec![
                                    "rebuild".into(),
                                    path.clone(),
                                    text(&folder),
                                    original.id.clone(),
                                ],
                            ));
                        }
                    }
                });
            }
        });
        if ui.button("Back to sources").clicked() {
            self.archive = None;
            self.archive_cover = None;
        }
        self.activity(ui);
        if let Some((label, args)) = action {
            if label == "open" {
                self.choose_archive();
            } else {
                self.edit(label, args);
            }
        }
    }
}

fn format_elapsed(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect::<Vec<_>>()
        });
        if !dropped.is_empty() {
            self.drop_files(dropped);
        }
        if self.art_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        let busy = self.busy();
        if !busy && self.archive.is_none() && !self.sources.is_empty() {
            egui::Panel::right("configuration")
                .exact_size(356.0)
                .resizable(false)
                .frame(egui::Frame::NONE.fill(theme::CARD).inner_margin(20))
                .show(ui, |ui| {
                    self.configuration(ui);
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BG).inner_margin(24))
            .show(ui, |ui| {
                if busy {
                    self.progress_view(ui);
                } else if self.archive.is_some() {
                    self.archive_view(ui);
                } else if self.sources.is_empty() {
                    self.empty(ui, false);
                } else {
                    self.source_list(ui);
                }
            });
        let crop_answer = self.crop.as_mut().and_then(|c| c.show(&ctx));
        if let Some(accept) = crop_answer {
            let crop = self.crop.take().unwrap();
            if accept {
                let bytes = crop.art.crop(crop.center, crop.zoom);
                let appearance = self.appearances.entry(crop.key).or_default();
                if let Ok(mut art) = artwork::Artwork::decode(&bytes) {
                    appearance.texture = Some(art.texture(&ctx).clone());
                    appearance.icon = Some(Arc::from(bytes));
                }
            }
        }
        if let Some((label, args)) = self.remove_confirmation.clone() {
            egui::Modal::new(egui::Id::unique("confirm-remove")).show(&ctx,|ui| {
                ui.set_width(340.0);ui.heading(&label);ui.label("This removes the selected snapshot from this archive. Your original source files are unaffected.");
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() { self.remove_confirmation = None; }
                        if ui.button("Remove snapshot").clicked() {
                            self.remove_confirmation = None;
                            self.edit(label,args);
                        }
                    });
            });
        }
        let hovered = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovered && !busy {
            let rect = ctx.viewport_rect().shrink(8.0);
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("file-drop"),
            ));
            painter.rect_filled(rect, 16.0, Color32::from_rgba_unmultiplied(25, 22, 38, 235));
            painter.rect_stroke(
                rect,
                16.0,
                egui::Stroke::new(2.0, ACCENT),
                egui::StrokeKind::Inside,
            );
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop to open",
                egui::FontId::proportional(28.0),
                TEXT,
            );
        }
    }
}
