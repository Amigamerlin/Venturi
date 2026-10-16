//! "Library" tab of the media pool: assets shared by all projects. Dragging
//! one imports it into the project's pool and carries it like a pool item.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PoolTab {
    #[default]
    Project,
    Library,
}

#[derive(Default)]
pub(crate) struct LibraryUiState {
    pub tab: PoolTab,
    search: String,
    /// Asset being renamed and its new name.
    pub renaming: Option<(String, String)>,
    /// What each asset became in a project, per `(epoch, asset id)`: a
    /// second drag reuses it instead of duplicating a compound clip.
    imports: HashMap<(u64, String), Vec<MediaId>>,
    /// Payload of the asset being dragged, set when the drag starts.
    drag: Option<timeline_ui::MediaDragSet>,
}

impl VenturiApp {
    pub(crate) fn poll_library(&mut self) {
        let errors = self.library.poll();
        if !errors.is_empty() {
            self.project_error =
                Some(t!("library.save_failed", error = errors.join("\n")).into_owned());
        }
    }

    /// One asset per selected pool item, files copied into the library.
    pub(crate) fn add_selected_media_to_library(&mut self) {
        let origin = self.clipboard_origin();
        for &id in &self.media_pool_state.selected {
            let Some(item) = self.session.project.media_pool.get(id) else {
                continue;
            };
            let name = file_label(&item.path);
            let payload = vv_core::clipboard::ClipboardPayload::new(
                origin,
                &self.session.project,
                &[id],
                &[],
            );
            self.library.add_asset(name, payload);
        }
    }

    /// The asset's pool items in this project, imported if needed.
    fn import_library_asset(&mut self, asset_id: &str) -> Vec<MediaId> {
        let key = (self.session.epoch(), asset_id.to_owned());
        if let Some(media) = self.library_ui.imports.get(&key)
            && media
                .iter()
                .all(|&id| self.session.project.media_pool.contains_key(id))
        {
            return media.clone();
        }
        let Some(asset) = self.library.assets.iter().find(|a| a.id == asset_id) else {
            return Vec::new();
        };
        let mut add = vv_core::AddEntities::new(vv_core::CommandLabel::ImportMedia);
        let pasted = asset
            .payload
            .clone()
            .merge_into(&mut self.session.project, &mut add);
        self.add_pasted_entities(add);
        self.library_ui.imports.insert(key, pasted.media.clone());
        pasted.media
    }

    fn add_library_asset_to_project(&mut self, asset_id: &str) {
        let media = self.import_library_asset(asset_id);
        if let Some(&first) = media.first() {
            self.library_ui.tab = PoolTab::Project;
            self.media_pool_state.select_only(media);
            self.reveal_in_media_pool(first);
        }
    }

    pub(crate) fn show_pool_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.library_ui.tab,
                PoolTab::Project,
                t!("pool.tab_project"),
            );
            // A hidden pool selection would still take Del and Ctrl+C.
            if ui
                .selectable_value(
                    &mut self.library_ui.tab,
                    PoolTab::Library,
                    t!("pool.tab_library"),
                )
                .on_hover_text(t!("library.tab_hint"))
                .clicked()
            {
                self.media_pool_state.select_only([]);
            }
        });
    }

    pub(crate) fn show_library(&mut self, ui: &mut egui::Ui) {
        ui.add(
            egui::TextEdit::singleline(&mut self.library_ui.search)
                .hint_text(t!("pool.search"))
                .desired_width(f32::INFINITY),
        );
        let query = self.library_ui.search.trim().to_owned();
        let mut add_to_project = None;
        let mut rename = None;
        let mut delete = None;
        let mut drag_started = None;
        egui::ScrollArea::vertical()
            .id_salt("library_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for name in self.library.pending() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("library.copying", name = name));
                    });
                }
                if self.library.assets.is_empty() && self.library.pending().next().is_none() {
                    ui.weak(t!("library.empty_hint"));
                }
                for asset in &self.library.assets {
                    if !query.is_empty() && !media_pool::matches_search(&asset.name, &query) {
                        continue;
                    }
                    let Some(item) = asset.item() else {
                        continue;
                    };
                    if item.compound.is_none()
                        && item.meta.has_video
                        && !self.thumbnails.contains_key(&item.content_hash)
                    {
                        self.thumbnails.insert(item.content_hash, None);
                        self.thumbnail_worker
                            .get_or_insert_with(thumbnail_worker::ThumbnailWorker::spawn)
                            .enqueue(
                                item.path.clone(),
                                item.content_hash,
                                item.meta.duration_frames as f64 / item.meta.fps.as_f64(),
                            );
                    }
                    let thumbnail = self.thumbnails.get(&item.content_hash).cloned().flatten();
                    let id = ui.id().with(("library_asset", &asset.id));
                    let resp = library_row(ui, id, &asset.name, item, thumbnail.as_ref());
                    if resp.double_clicked() {
                        add_to_project = Some(asset.id.clone());
                    }
                    if resp.drag_started() {
                        drag_started = Some(asset.id.clone());
                    }
                    if resp.dragged() {
                        if let Some(set) = &self.library_ui.drag {
                            resp.dnd_set_drag_payload(set.clone());
                        }
                        show_drag_ghost(ui, id, &asset.name);
                    }
                    resp.context_menu(|ui| {
                        if ui.button(t!("library.add_to_project")).clicked() {
                            add_to_project = Some(asset.id.clone());
                            ui.close();
                        }
                        ui.menu_button(t!("pool.rename"), |ui| {
                            let name = self
                                .library_ui
                                .renaming
                                .get_or_insert_with(|| (asset.id.clone(), asset.name.clone()));
                            if name.0 != asset.id {
                                *name = (asset.id.clone(), asset.name.clone());
                            }
                            let edit = ui.text_edit_singleline(&mut name.1);
                            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                rename = Some(name.clone());
                                ui.close();
                            }
                        });
                        ui.separator();
                        ui.menu_button(t!("library.delete"), |ui| {
                            if ui.button(t!("library.delete_confirm")).clicked() {
                                delete = Some(asset.id.clone());
                                ui.close();
                            }
                        });
                    });
                }
            });
        if self.library.pending().next().is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        if let Some(asset_id) = drag_started {
            let media = self.import_library_asset(&asset_id);
            self.library_ui.drag = Some(timeline_ui::MediaDragSet {
                items: media
                    .iter()
                    .map(|&id| {
                        timeline_ui::MediaDrag::whole(id, &self.session.project.media_pool[id].meta)
                    })
                    .collect(),
            });
            if let Some(set) = &self.library_ui.drag {
                egui::DragAndDrop::set_payload(ui.ctx(), set.clone());
            }
        }
        if let Some(asset_id) = add_to_project {
            self.add_library_asset_to_project(&asset_id);
        }
        let result = if let Some((id, name)) = rename {
            self.library_ui.renaming = None;
            match name.trim() {
                "" => Ok(()),
                name => self.library.rename_asset(&id, name.to_owned()),
            }
        } else if let Some(id) = delete {
            self.library.remove_asset(&id)
        } else {
            Ok(())
        };
        if let Err(e) = result {
            self.project_error = Some(t!("library.save_failed", error = e).into_owned());
        }
    }
}

fn library_row(
    ui: &mut egui::Ui,
    id: egui::Id,
    name: &str,
    item: &vv_core::MediaItem,
    thumbnail: Option<&egui::TextureHandle>,
) -> egui::Response {
    let meta = &item.meta;
    let group = ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            let thumb_size = egui::vec2(64.0, 36.0);
            let (rect, _) = ui.allocate_exact_size(thumb_size, egui::Sense::hover());
            match thumbnail {
                Some(texture) => {
                    let tex_size = texture.size_vec2();
                    let scale = (thumb_size.x / tex_size.x).min(thumb_size.y / tex_size.y);
                    ui.painter().rect_filled(rect, 2.0, egui::Color32::BLACK);
                    egui::Image::new(texture)
                        .fit_to_exact_size(tex_size * scale)
                        .paint_at(
                            ui,
                            egui::Rect::from_center_size(rect.center(), tex_size * scale),
                        );
                }
                None => {
                    ui.painter()
                        .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                    let icon = if item.compound.is_some() {
                        "🎞"
                    } else if !meta.has_video {
                        "🔊"
                    } else {
                        ""
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        icon,
                        egui::FontId::proportional(18.0),
                        ui.visuals().weak_text_color(),
                    );
                }
            }
            ui.vertical(|ui| {
                ui.label(name);
                ui.small(if item.compound.is_some() {
                    t!("library.compound")
                } else if meta.is_image() {
                    t!("pool.meta_image", width = meta.width, height = meta.height)
                } else if meta.has_video {
                    format!(
                        "{}x{} · {:.2}fps",
                        meta.width,
                        meta.height,
                        meta.fps.as_f64()
                    )
                    .into()
                } else {
                    t!(
                        "pool.meta_audio",
                        rate = meta.sample_rate,
                        channels = meta.channels
                    )
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !meta.is_image() {
                    ui.monospace(format_duration(meta.duration_frames, meta.fps.as_f64()));
                }
            });
        });
    });
    ui.interact(group.response.rect, id, egui::Sense::click_and_drag())
        .on_hover_text(t!("library.item_hint"))
}
