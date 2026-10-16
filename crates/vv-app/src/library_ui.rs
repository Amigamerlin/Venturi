//! "Library" tab of the media pool: assets shared by all projects, in the
//! same browser as the project's pool. Dragging assets imports them into the
//! project's pool and carries them like pool items.

use super::*;
use media_pool::{PoolState, RenameTarget};
use pool_view::{MenuChoice, PoolEntry, PoolEvent, PoolFolder, PoolMenus, PoolView, Thumb};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PoolTab {
    #[default]
    Project,
    Library,
}

#[derive(Default)]
pub(crate) struct LibraryUiState {
    pub tab: PoolTab,
    pub pool: PoolState<String, String>,
    /// What each asset became in a project, per `(epoch, asset id)`: a
    /// second drag reuses it instead of duplicating a compound clip.
    imports: HashMap<(u64, String), Vec<MediaId>>,
    /// Assets whose deletion waits for a confirmation.
    pub confirm_delete: Option<Vec<String>>,
    /// Whether each asset misses a file: checked once, until `offline` is
    /// cleared.
    offline: HashMap<String, bool>,
}

struct LibraryMenus;

enum LibraryAction {
    AddToProject,
    Relink(String),
    Delete,
}

impl PoolMenus<String, String> for LibraryMenus {
    type Action = LibraryAction;

    fn item_menu(
        &self,
        ui: &mut egui::Ui,
        entry: &PoolEntry<String, String>,
        _selected: usize,
        out: &mut Vec<MenuChoice<String, LibraryAction>>,
    ) {
        if ui.button(t!("library.add_to_project")).clicked() {
            out.push(MenuChoice::Action(LibraryAction::AddToProject));
            ui.close();
        }
        pool_view::rename_button(ui, entry, out);
        if matches!(entry.thumb, Thumb::Offline)
            && ui
                .button(t!("library.relink"))
                .on_hover_text(t!("library.relink_hint"))
                .clicked()
        {
            out.push(MenuChoice::Action(LibraryAction::Relink(entry.id.clone())));
            ui.close();
        }
        ui.separator();
        ui.menu_button(t!("library.delete"), |ui| {
            if ui.button(t!("library.delete_confirm")).clicked() {
                out.push(MenuChoice::Action(LibraryAction::Delete));
                ui.close();
            }
        });
    }
}

impl VenturiApp {
    pub(crate) fn poll_library(&mut self) {
        let errors = self.library.poll();
        if !errors.is_empty() {
            self.project_error =
                Some(t!("library.save_failed", error = errors.join("\n")).into_owned());
        }
    }

    fn library_result(&mut self, result: Result<(), String>) {
        if let Err(e) = result {
            self.project_error = Some(t!("library.save_failed", error = e).into_owned());
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
            self.library.add_asset(name, None, payload);
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
        if let Some(project) = self.session.path().map(Path::to_path_buf) {
            self.copy_library_media_into_project(&project);
        }
        pasted.media
    }

    /// The media the project takes from the library get their own copy in
    /// `library/` next to `project_file`, in the background: the project
    /// stays whole in its folder, whatever happens to the library. An
    /// unsaved project has no folder yet: it gets the copies when saved.
    pub(crate) fn copy_library_media_into_project(&mut self, project_file: &Path) {
        let (Some(media_dir), Some(project_dir)) =
            (self.library.media_dir(), project_file.parent())
        else {
            return;
        };
        let dest = project_dir.join("library");
        let epoch = self.session.epoch();
        let from_library: Vec<(MediaId, PathBuf)> = self
            .session
            .project
            .media_pool
            .iter()
            .filter(|(_, item)| item.compound.is_none() && item.path.starts_with(&media_dir))
            .map(|(id, item)| (id, item.path.clone()))
            .collect();
        for (id, path) in from_library {
            self.project_copier
                .get_or_insert_with(library_copy::ProjectCopier::spawn)
                .enqueue(epoch, id, path, dest.clone());
        }
    }

    /// Points the media at their finished copies. With nothing else
    /// unsaved the project file is written again, so that it no longer
    /// needs the library.
    pub(crate) fn poll_library_copies(&mut self) {
        let Some(copier) = &mut self.project_copier else {
            return;
        };
        let done = copier.poll();
        let idle = copier.progress().is_none();
        let mut moved = false;
        for copy in done {
            match copy.result {
                Ok(path) => {
                    // Not a command: the file is the same, only where it
                    // lives changes, and an undo must not point back at the
                    // library.
                    if copy.epoch == self.session.epoch()
                        && let Some(item) = self.session.project.media_pool.get_mut(copy.media)
                        && item.path == copy.src
                    {
                        item.path = path;
                        moved = true;
                    }
                }
                Err(e) => {
                    self.project_error =
                        Some(t!("library.project_copy_failed", error = e).into_owned());
                }
            }
        }
        if !moved {
            return;
        }
        self.media_pool_state.invalidate_offline();
        if idle
            && !self.session.has_unsaved_changes()
            && let Some(path) = self.session.path().map(Path::to_path_buf)
            && let Err(e) = self.session.save_to(&path)
        {
            self.project_error = Some(t!("project.save_failed", error = e).into_owned());
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_for_library_copies(&mut self) {
        while self
            .project_copier
            .as_ref()
            .is_some_and(|c| c.progress().is_some())
        {
            self.poll_library_copies();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn show_library_copy_progress(&self, ui: &mut egui::Ui) {
        let Some(progress) = self.project_copier.as_ref().and_then(|c| c.progress()) else {
            return;
        };
        ui.add(egui::ProgressBar::new(progress.fraction).text(t!(
            "library.copying_to_project",
            name = progress.name,
            percent = format!("{:.0}", progress.fraction * 100.0)
        )))
        .on_hover_text(t!(
            "library.copying_to_project_hint",
            count = progress.files
        ));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }

    pub(crate) fn relink_library_asset(&mut self, id: &str, base_dir: &Path) {
        self.library_ui.offline.remove(id);
        match self.library.relink_asset(id, base_dir) {
            Ok(0) => self.relink_message = Some(t!("library.relink_done").into_owned()),
            Ok(missing) => {
                self.relink_message =
                    Some(t!("library.relink_partial", missing = missing).into_owned())
            }
            Err(e) => self.library_result(Err(e)),
        }
    }

    fn add_library_assets_to_project(&mut self, assets: &[String]) {
        let media: Vec<MediaId> = assets
            .iter()
            .flat_map(|id| self.import_library_asset(id))
            .collect();
        if let Some(&first) = media.first() {
            self.reveal_in_media_pool(first);
            self.media_pool_state.select_only(media);
        }
    }

    pub(crate) fn show_pool_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let tab = self.library_ui.tab;
            ui.selectable_value(
                &mut self.library_ui.tab,
                PoolTab::Project,
                t!("pool.tab_project"),
            );
            ui.selectable_value(
                &mut self.library_ui.tab,
                PoolTab::Library,
                t!("pool.tab_library"),
            )
            .on_hover_text(t!("library.tab_hint"));
            // A hidden selection would still take Ctrl+C.
            if self.library_ui.tab != tab {
                self.media_pool_state.clear();
                self.library_ui.pool.clear();
            }
        });
        self.show_library_copy_progress(ui);
    }

    fn library_selection(&self) -> Vec<String> {
        self.library_ui.pool.selected.iter().cloned().collect()
    }

    /// Del on the Library tab: deleting removes the copied files, so it
    /// asks first.
    pub(crate) fn delete_selected_library_assets(&mut self) {
        if !self.library_ui.pool.selected.is_empty() {
            self.library_ui.confirm_delete = Some(self.library_selection());
        }
    }

    pub(crate) fn select_all_library_assets(&mut self) {
        let all: Vec<String> = self.library.assets.iter().map(|a| a.id.clone()).collect();
        self.library_ui.pool.select_only(all);
    }

    pub(crate) fn rename_selected_library_item(&mut self) {
        let pool = &mut self.library_ui.pool;
        if let Some(folder) = pool.selected_folder.clone() {
            if let Some(f) = self.library.folder(&folder) {
                pool.start_rename(RenameTarget::Folder(folder), f.name.clone());
            }
        } else if let Some(id) = pool.single_selected().cloned()
            && let Some(asset) = self.library.assets.iter().find(|a| a.id == id)
        {
            pool.start_rename(RenameTarget::Item(id), asset.name.clone());
        }
    }

    /// Enter on the Library tab.
    pub(crate) fn add_selected_library_assets_to_project(&mut self) {
        let selected = self.library_selection();
        self.add_library_assets_to_project(&selected);
    }

    fn remove_library_assets(&mut self, ids: &[String]) {
        for id in ids {
            let result = self.library.remove_asset(id);
            self.library_result(result);
        }
    }

    fn new_library_folder(&mut self, parent: Option<String>) {
        let taken: Vec<&str> = self
            .library
            .folders
            .iter()
            .filter(|f| f.parent == parent)
            .map(|f| f.name.as_str())
            .collect();
        let name = new_folder_name(&taken);
        match self.library.add_folder(name.clone(), parent.clone()) {
            Ok(folder) => {
                let pool = &mut self.library_ui.pool;
                pool.expanded.extend(parent);
                pool.start_rename(RenameTarget::Folder(folder), name);
            }
            Err(e) => self.library_result(Err(e)),
        }
    }

    pub(crate) fn show_library(&mut self, ui: &mut egui::Ui) {
        for name in self.library.pending() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t!("library.copying", name = name));
            });
        }
        if self.library.pending().next().is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        let pool = &mut self.library_ui.pool;
        let checked = &mut self.library_ui.offline;
        checked.retain(|id, _| self.library.assets.iter().any(|a| a.id == *id));
        let mut entries = Vec::new();
        for asset in &self.library.assets {
            let Some(item) = asset.item() else {
                continue;
            };
            let offline = *checked
                .entry(asset.id.clone())
                .or_insert_with(|| asset.is_offline());
            if item.compound.is_none()
                && item.meta.has_video
                && !offline
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
            let thumb = if item.compound.is_some() {
                Thumb::Timeline
            } else if offline {
                Thumb::Offline
            } else {
                match self.thumbnails.get(&item.content_hash).cloned().flatten() {
                    Some(texture) => Thumb::Texture(texture),
                    None => Thumb::None,
                }
            };
            entries.push(PoolEntry {
                id: asset.id.clone(),
                name: asset.name.clone(),
                folder: asset.folder.clone(),
                meta: item.meta.clone(),
                thumb,
                proxy: None,
                renamable: true,
            });
        }
        let folders = self
            .library
            .folders
            .iter()
            .map(|f| PoolFolder {
                id: f.id.clone(),
                name: f.name.clone(),
                parent: f.parent.clone(),
            })
            .collect();
        let events = pool_view::show_pool(
            ui,
            pool,
            PoolView {
                folders,
                entries,
                menus: &LibraryMenus,
                kinetic_scroll: self.settings.kinetic_scroll_media_pool,
                item_hint: t!("library.item_hint"),
                empty_hint: Some(t!("library.empty_hint")),
            },
        );
        for event in events {
            match event {
                PoolEvent::Rename(id, name) => {
                    let result = self.library.rename_asset(&id, name);
                    self.library_result(result);
                }
                PoolEvent::RenameFolder(id, name) => {
                    let result = self.library.rename_folder(&id, name);
                    self.library_result(result);
                }
                PoolEvent::NewFolder(parent) => self.new_library_folder(parent),
                PoolEvent::DeleteFolder(id) => {
                    let result = self.library.remove_folder(&id);
                    self.library_result(result);
                }
                PoolEvent::MoveItems(ids, folder) => {
                    let result = self.library.move_assets(&ids, folder);
                    self.library_result(result);
                }
                PoolEvent::MoveFolder(id, parent) => {
                    let result = self.library.move_folder(&id, parent);
                    self.library_result(result);
                }
                PoolEvent::Open(id) => self.add_library_assets_to_project(&[id]),
                // Imported only once the drag leaves the library, see below.
                PoolEvent::DragStarted(_) => {}
                PoolEvent::Action(LibraryAction::AddToProject) => {
                    self.add_selected_library_assets_to_project()
                }
                PoolEvent::Action(LibraryAction::Relink(id)) => self
                    .spawn_file_dialog(project_io::DialogKind::RelinkLibraryAsset(id), |dlg| {
                        dlg.pick_folder()
                    }),
                PoolEvent::Action(LibraryAction::Delete) => {
                    let selected = self.library_selection();
                    self.remove_library_assets(&selected);
                }
            }
        }
        self.import_dragged_assets(ui.ctx());
        let library = &self.library;
        self.library_ui.pool.forget(
            |id| library.assets.iter().any(|a| a.id == *id),
            |id| library.folder(id).is_some(),
        );
        self.show_library_delete_confirm(ui.ctx());
    }

    /// Assets dragged out of the library become pool items, so that the
    /// timeline can take them: moving them between the library's own
    /// folders imports nothing.
    fn import_dragged_assets(&mut self, ctx: &egui::Context) {
        let Some(drag) = egui::DragAndDrop::payload::<pool_view::ItemsDrag<String>>(ctx) else {
            return;
        };
        let inside = self
            .library_ui
            .pool
            .scroll_area
            .is_some_and(|area| timeline_ui::pointer_over(ctx, area.viewport));
        if inside {
            return;
        }
        let media: Vec<MediaId> = drag
            .0
            .iter()
            .flat_map(|id| self.import_library_asset(id))
            .collect();
        let set = timeline_ui::MediaDragSet {
            items: media
                .iter()
                .map(|&id| {
                    timeline_ui::MediaDrag::whole(id, &self.session.project.media_pool[id].meta)
                })
                .collect(),
        };
        egui::DragAndDrop::set_payload(ctx, set);
    }

    fn show_library_delete_confirm(&mut self, ctx: &egui::Context) {
        let Some(ids) = &self.library_ui.confirm_delete else {
            return;
        };
        let count = ids.len();
        let mut choice = None;
        egui::Modal::new(egui::Id::new("library_delete_confirm")).show(ctx, |ui| {
            ui.label(if count == 1 {
                t!("library.delete_question_one")
            } else {
                t!("library.delete_question_many", count = count)
            });
            ui.horizontal(|ui| {
                if ui.button(t!("common.cancel")).clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    choice = Some(false);
                }
                if ui.button(t!("library.delete")).clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    choice = Some(true);
                }
            });
        });
        if let Some(delete) = choice {
            let ids = self.library_ui.confirm_delete.take().unwrap_or_default();
            if delete {
                self.remove_library_assets(&ids);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/library_ui.rs"]
mod tests;
