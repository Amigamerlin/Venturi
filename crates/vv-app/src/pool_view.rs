//! Browser shared by the project's media pool and the user library: a tree of
//! folders and items with selection, rename, search, sort and drag and drop.
//! What renaming, moving or opening does is up to the caller, through the
//! returned [`PoolEvent`]s.

use super::*;
use media_pool::{FolderDrag, PoolId, PoolRow, PoolState, Rename, RenameTarget};

pub(crate) const DURATION_COL_W: f32 = 64.0;

/// Free background on the sides and below the items: always somewhere to
/// right click, even with the pool full.
const SIDE_PAD: i8 = 6;
const BOTTOM_PAD: f32 = 28.0;

/// Height of the pool header bar.
pub(crate) const HEADER_HEIGHT: f32 = 22.0;

/// Horizontal step per folder level.
const FOLDER_INDENT: f32 = 14.0;

pub(crate) enum Thumb {
    Texture(egui::TextureHandle),
    Offline,
    Timeline,
    /// Not ready yet, or none: audio gets a speaker.
    None,
}

pub(crate) struct PoolEntry<I, F> {
    pub id: I,
    pub name: String,
    pub folder: Option<F>,
    pub meta: vv_core::MediaMeta,
    pub thumb: Thumb,
    pub proxy: Option<proxy_worker::ProxyState>,
    /// A slow double click on the name starts a rename.
    pub renamable: bool,
}

pub(crate) struct PoolFolder<F> {
    pub id: F,
    pub name: String,
    pub parent: Option<F>,
}

pub(crate) enum PoolEvent<I, F, A> {
    Rename(I, String),
    RenameFolder(F, String),
    NewFolder(Option<F>),
    DeleteFolder(F),
    MoveItems(Vec<I>, Option<F>),
    MoveFolder(F, Option<F>),
    /// Double click on an item.
    Open(I),
    /// The payload is an [`ItemsDrag`]: for the timeline the caller turns
    /// it into a `timeline_ui::MediaDragSet`.
    DragStarted(Vec<I>),
    Action(A),
}

/// Entries of the context menus that only one pool has.
pub(crate) trait PoolMenus<I, F> {
    type Action;
    /// `selected`: how many items the menu acts on.
    fn item_menu(
        &self,
        ui: &mut egui::Ui,
        entry: &PoolEntry<I, F>,
        selected: usize,
        out: &mut Vec<MenuChoice<I, Self::Action>>,
    );
    /// Between "Rename" and "Delete folder".
    fn folder_menu(&self, _ui: &mut egui::Ui, _folder: &F, _out: &mut Vec<Self::Action>) {}
    /// After "New folder".
    fn background_menu(&self, _ui: &mut egui::Ui, _out: &mut Vec<Self::Action>) {}
}

pub(crate) enum MenuChoice<I, A> {
    Rename(I, String),
    Action(A),
}

/// The "Rename" entry of an item menu.
pub(crate) fn rename_button<I: Clone, F, A>(
    ui: &mut egui::Ui,
    entry: &PoolEntry<I, F>,
    out: &mut Vec<MenuChoice<I, A>>,
) {
    if ui.button(t!("pool.rename")).clicked() {
        out.push(MenuChoice::Rename(entry.id.clone(), entry.name.clone()));
        ui.close();
    }
}

/// Drag payload of items of a pool, in panel order.
pub(crate) struct ItemsDrag<I>(pub Vec<I>);

pub(crate) struct PoolView<'a, I, F, M> {
    pub folders: Vec<PoolFolder<F>>,
    pub entries: Vec<PoolEntry<I, F>>,
    pub menus: &'a M,
    pub kinetic_scroll: bool,
    pub item_hint: std::borrow::Cow<'a, str>,
    /// Shown instead of the items when there are none.
    pub empty_hint: Option<std::borrow::Cow<'a, str>>,
}

pub(crate) fn show_pool<I: PoolId, F: PoolId, M: PoolMenus<I, F>>(
    ui: &mut egui::Ui,
    state: &mut PoolState<I, F>,
    view: PoolView<'_, I, F, M>,
) -> Vec<PoolEvent<I, F, M::Action>> {
    let mut events = Vec::new();
    if !egui::DragAndDrop::has_any_payload(ui.ctx()) {
        state.dragging.clear();
    }
    let search_changed = search_field(ui, &mut state.search, std::any::type_name::<I>());
    pool_header(ui, state);
    kinetic_pool_scroll(ui.ctx(), state, view.kinetic_scroll);
    let PoolView {
        folders,
        mut entries,
        menus,
        item_hint,
        empty_hint,
        ..
    } = view;
    let query = state.search.trim().to_string();
    if !query.is_empty() {
        entries.retain(|e| media_pool::matches_search(&e.name, &query));
    }
    // Hidden items must not stay selected: Del would delete them.
    if search_changed {
        state
            .selected
            .retain(|id| entries.iter().any(|e| e.id == *id));
    }
    media_pool::sort_items(
        &mut entries,
        state.sort,
        |e| e.name.as_str(),
        |e| e.meta.duration_frames as f64 / e.meta.fps.as_f64().max(1e-9),
    );
    let folder_list: Vec<(F, &str, Option<F>)> = folders
        .iter()
        .map(|f| (f.id.clone(), f.name.as_str(), f.parent.clone()))
        .collect();
    let items: Vec<(PoolEntry<I, F>, Option<F>)> = entries
        .into_iter()
        .map(|e| {
            let folder = e.folder.clone();
            (e, folder)
        })
        .collect();
    let empty = items.is_empty() && folders.is_empty();
    let rows = if query.is_empty() {
        media_pool::tree_rows(&folder_list, items, &state.expanded)
    } else {
        media_pool::search_rows(&folder_list, items)
    };
    let order: Vec<I> = rows
        .iter()
        .filter_map(|row| match row {
            PoolRow::Item { item, .. } => Some(item.id.clone()),
            PoolRow::Folder { .. } => None,
        })
        .collect();
    let mut menu_choices: Vec<MenuChoice<I, M::Action>> = Vec::new();
    let mut folder_actions: Vec<M::Action> = Vec::new();
    // `auto_shrink` off: the panel must fill the assigned width,
    // otherwise its resize springs back.
    let output = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if rows.is_empty() && !query.is_empty() {
                ui.weak(t!("pool.search_no_results"));
            } else if empty && let Some(hint) = &empty_hint {
                ui.weak(hint.as_ref());
            }
            // Interacted with before the items: in egui the last one wins, so a click
            // on an item does not start the selection rectangle. The clip rect
            // is the viewport: `available_rect_before_wrap` is viewport-sized
            // but scrolls away with the content.
            let bg = ui.interact(
                ui.clip_rect(),
                ui.id().with("media_pool_bg"),
                egui::Sense::click_and_drag(),
            );
            bg.context_menu(|ui| {
                if ui.button(t!("pool.new_folder")).clicked() {
                    events.push(PoolEvent::NewFolder(None));
                    ui.close();
                }
                menus.background_menu(ui, &mut folder_actions);
            });
            let mut item_rects: Vec<(I, egui::Rect)> = Vec::new();
            // The items leave a strip of background on the sides (and
            // `BOTTOM_PAD` below): with a full pool there would be no
            // free spot left to right click on.
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(SIDE_PAD, 0))
                .show(ui, |ui| {
                    for row in rows {
                        match row {
                            PoolRow::Folder { id, depth } => {
                                let folder = folders.iter().find(|f| f.id == id);
                                if let Some(folder) = folder {
                                    indented(ui, depth, |ui| {
                                        folder_row(
                                            ui,
                                            state,
                                            folder,
                                            &folder_list,
                                            menus,
                                            &mut folder_actions,
                                            &mut events,
                                        )
                                    });
                                }
                            }
                            PoolRow::Item { item: entry, depth } => {
                                let rect = item_row(
                                    ui,
                                    state,
                                    &entry,
                                    depth,
                                    &order,
                                    &folder_list,
                                    &item_hint,
                                    menus,
                                    &mut menu_choices,
                                    &mut events,
                                );
                                item_rects.push((entry.id, rect));
                            }
                        }
                    }
                });
            ui.add_space(BOTTOM_PAD);
            accept_drop(&bg, state, None, &folder_list, &mut events);
            if let Some((id, name, clicked_at)) = state.rename_pending.clone() {
                let wait = ui.ctx().options(|o| o.input_options.max_double_click_delay);
                let elapsed = ui.input(|i| i.time) - clicked_at;
                if elapsed > wait {
                    state.start_rename(RenameTarget::Item(id), name);
                } else {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs_f64(wait - elapsed));
                }
            }
            marquee(ui, &bg, state, &item_rects);
        });
    state.scroll_area = Some(media_pool::PoolScrollArea {
        id: output.id,
        viewport: output.inner_rect,
        max_offset: (output.content_size.y - output.inner_rect.height()).max(0.0),
    });
    for choice in menu_choices {
        match choice {
            MenuChoice::Rename(id, name) => state.start_rename(RenameTarget::Item(id), name),
            MenuChoice::Action(action) => events.push(PoolEvent::Action(action)),
        }
    }
    events.extend(folder_actions.into_iter().map(PoolEvent::Action));
    events
}

fn marquee<I: PoolId, F: PoolId>(
    ui: &egui::Ui,
    bg: &egui::Response,
    state: &mut PoolState<I, F>,
    item_rects: &[(I, egui::Rect)],
) {
    if bg.drag_started() {
        if let Some(pos) = bg.interact_pointer_pos() {
            state.marquee = Some((pos, pos));
        }
    } else if bg.dragged() {
        if let (Some((_, end)), Some(pos)) = (&mut state.marquee, bg.interact_pointer_pos()) {
            *end = pos;
        }
    } else if bg.drag_stopped() {
        if let Some((start, end)) = state.marquee.take() {
            let rect = egui::Rect::from_two_pos(start, end);
            let hits = item_rects
                .iter()
                .filter(|(_, r)| r.intersects(rect))
                .map(|(id, _)| id.clone());
            state.select_only(hits);
        }
    } else if bg.clicked() {
        state.rename_pending = None;
        state.clear();
    }
    if let Some((start, end)) = state.marquee {
        let rect = egui::Rect::from_two_pos(start, end);
        ui.painter()
            .rect_filled(rect, 0.0, crate::theme::ACCENT_TRANSLUCENT);
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, crate::theme::ACCENT),
            egui::StrokeKind::Inside,
        );
    }
}

/// Whether the payload being dragged holds items of this pool: as an
/// [`ItemsDrag`], or already turned into a `MediaDragSet`.
fn carries_own_items<I: PoolId, F>(ctx: &egui::Context, state: &PoolState<I, F>) -> bool {
    !state.dragging.is_empty()
        && (egui::DragAndDrop::has_payload_of_type::<ItemsDrag<I>>(ctx)
            || egui::DragAndDrop::has_payload_of_type::<timeline_ui::MediaDragSet>(ctx))
}

/// Drops onto the pool: its own items or a folder dragged into `folder`.
/// The payload type is checked first: `dnd_release_payload` takes the
/// payload even when its type does not match.
fn accept_drop<I: PoolId, F: PoolId, A>(
    resp: &egui::Response,
    state: &mut PoolState<I, F>,
    folder: Option<&F>,
    folders: &[(F, &str, Option<F>)],
    events: &mut Vec<PoolEvent<I, F, A>>,
) {
    if !resp.contains_pointer() || !resp.ctx.input(|i| i.pointer.any_released()) {
        return;
    }
    if carries_own_items(&resp.ctx, state) {
        egui::DragAndDrop::clear_payload(&resp.ctx);
        events.push(PoolEvent::MoveItems(
            std::mem::take(&mut state.dragging),
            folder.cloned(),
        ));
    } else if egui::DragAndDrop::has_payload_of_type::<FolderDrag<F>>(&resp.ctx)
        && let Some(dragged) = resp.dnd_release_payload::<FolderDrag<F>>()
        && media_pool::can_move_folder(folders, &dragged.0, folder)
        && folders
            .iter()
            .find(|(id, ..)| *id == dragged.0)
            .is_some_and(|(_, _, parent)| parent.as_ref() != folder)
    {
        events.push(PoolEvent::MoveFolder(dragged.0.clone(), folder.cloned()));
        state.expanded.extend(folder.cloned());
    }
}

fn folder_row<I: PoolId, F: PoolId, M: PoolMenus<I, F>>(
    ui: &mut egui::Ui,
    state: &mut PoolState<I, F>,
    folder: &PoolFolder<F>,
    folders: &[(F, &str, Option<F>)],
    menus: &M,
    actions: &mut Vec<M::Action>,
    events: &mut Vec<PoolEvent<I, F, M::Action>>,
) {
    let id = &folder.id;
    let expanded = state.expanded.contains(id);
    let rect = ui
        .allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover())
        .0;
    let own_drag = carries_own_items(ui.ctx(), state);
    let target = RenameTarget::Folder(id.clone());
    let renaming = state.renaming.as_mut().filter(|r| r.target == target);
    let sense = if renaming.is_some() {
        egui::Sense::hover()
    } else {
        egui::Sense::click_and_drag()
    };
    let resp = ui.interact(rect, ui.id().with(("media_pool_folder", id)), sense);
    let drop_hover = (own_drag && resp.contains_pointer())
        || resp
            .dnd_hover_payload::<FolderDrag<F>>()
            .is_some_and(|d| d.0 != *id);
    let visuals = ui.visuals();
    if drop_hover {
        ui.painter()
            .rect_filled(rect, 3.0, crate::theme::ACCENT_TRANSLUCENT);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let color = visuals.text_color();
    let arrow = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::vec2(8.0, 8.0),
    );
    let points = if expanded {
        vec![arrow.left_top(), arrow.right_top(), arrow.center_bottom()]
    } else {
        vec![arrow.left_top(), arrow.right_center(), arrow.left_bottom()]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
    let icon = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 18.0, rect.center().y - 5.0),
        egui::vec2(15.0, 11.0),
    );
    let icon_color = visuals.weak_text_color();
    ui.painter().rect_filled(
        egui::Rect::from_min_size(icon.left_top() - egui::vec2(0.0, 2.0), egui::vec2(6.0, 3.0)),
        1.0,
        icon_color,
    );
    ui.painter().rect_filled(icon, 1.5, icon_color);
    let text_rect = rect.with_min_x(rect.left() + 38.0);
    match renaming {
        Some(rename) => {
            let done = ui
                .scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
                    rename_field(ui, rename)
                })
                .inner;
            if let Some(new_name) = done {
                state.renaming = None;
                if let Some(name) = new_name
                    && name != folder.name
                {
                    events.push(PoolEvent::RenameFolder(id.clone(), name));
                }
            }
        }
        None => {
            ui.painter().text(
                text_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                &folder.name,
                egui::FontId::proportional(14.0),
                color,
            );
        }
    }
    if state.selected_folder.as_ref() == Some(id) {
        paint_selected(ui, rect, 3.0);
    }
    if resp.clicked() {
        state.select_folder(id.clone());
        state.rename_pending = None;
        if expanded {
            state.expanded.remove(id);
        } else {
            state.expanded.insert(id.clone());
        }
    }
    resp.dnd_set_drag_payload(FolderDrag(id.clone()));
    if resp.dragged() {
        media_pool_ui::show_drag_ghost(ui, resp.id, &folder.name);
    }
    accept_drop(&resp, state, Some(id), folders, events);
    resp.context_menu(|ui| {
        if ui.button(t!("pool.new_folder")).clicked() {
            events.push(PoolEvent::NewFolder(Some(id.clone())));
            ui.close();
        }
        if ui.button(t!("pool.rename")).clicked() {
            state.start_rename(target.clone(), folder.name.clone());
            ui.close();
        }
        menus.folder_menu(ui, id, actions);
        ui.separator();
        if ui
            .button(t!("pool.delete_folder"))
            .on_hover_text(t!("pool.delete_folder_hint"))
            .clicked()
        {
            events.push(PoolEvent::DeleteFolder(id.clone()));
            ui.close();
        }
    });
}

/// Returns the rect of the row.
#[allow(clippy::too_many_arguments)]
fn item_row<I: PoolId, F: PoolId, M: PoolMenus<I, F>>(
    ui: &mut egui::Ui,
    state: &mut PoolState<I, F>,
    entry: &PoolEntry<I, F>,
    depth: usize,
    order: &[I],
    folders: &[(F, &str, Option<F>)],
    hint: &str,
    menus: &M,
    menu_choices: &mut Vec<MenuChoice<I, M::Action>>,
    events: &mut Vec<PoolEvent<I, F, M::Action>>,
) -> egui::Rect {
    let id = &entry.id;
    let meta = &entry.meta;
    let target = RenameTarget::Item(id.clone());
    let renaming = state.renaming.as_mut().filter(|r| r.target == target);
    let is_renaming = renaming.is_some();
    let mut rename_done = None;
    let mut label_rect = egui::Rect::NOTHING;
    let group_resp = indented(ui, depth, |ui| {
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                paint_thumb(ui, &entry.thumb, meta);
                ui.vertical(|ui| {
                    match renaming {
                        Some(rename) => rename_done = rename_field(ui, rename),
                        None => label_rect = ui.label(&entry.name).rect,
                    }
                    ui.small(meta_line(entry));
                });
                let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
                ui.allocate_ui_with_layout(
                    row,
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        // An image has no real duration.
                        let duration_label = if meta.is_image() {
                            "—".to_string()
                        } else {
                            format_duration(meta.duration_frames, meta.fps.as_f64())
                        };
                        ui.add_sized(
                            egui::vec2(DURATION_COL_W, ui.available_height()),
                            egui::Label::new(egui::RichText::new(duration_label).monospace()),
                        );
                        proxy_badge(ui, entry.proxy);
                    },
                );
            });
        })
        .response
    });
    let rect = group_resp.rect;
    if entry.proxy == Some(proxy_worker::ProxyState::Ready) {
        let rect = rect.shrink(1.0);
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height())),
            1.0,
            timeline_ui::PROXY_COLOR,
        );
    }
    let interact_id = ui.id().with("media_pool_item").with(id);
    // While renaming, the item must not steal the clicks of the text field.
    let sense = if is_renaming {
        egui::Sense::hover()
    } else {
        egui::Sense::click_and_drag()
    };
    let resp = ui.interact(rect, interact_id, sense).on_hover_text(hint);
    if let Some(new_name) = rename_done {
        state.renaming = None;
        if let Some(name) = new_name
            && name != entry.name
        {
            events.push(PoolEvent::Rename(id.clone(), name));
        }
    }
    if state.reveal.as_ref() == Some(id) {
        state.reveal = None;
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
    }
    if state.selected.contains(id) {
        paint_selected(ui, rect, 4.0);
    }
    if resp.clicked() {
        let modifiers = ui.input(|i| i.modifiers);
        let on_name = entry.renamable
            && modifiers.is_none()
            && state.single_selected() == Some(id)
            && resp
                .interact_pointer_pos()
                .is_some_and(|pos| label_rect.contains(pos));
        state.click(id.clone(), modifiers, order);
        state.rename_pending =
            on_name.then(|| (id.clone(), entry.name.clone(), ui.input(|i| i.time)));
    }
    // Right click or drag outside the selection replaces it (as on the
    // timeline, see `timeline_ui::drag_group_for`).
    if (resp.secondary_clicked() || resp.drag_started()) && !state.selected.contains(id) {
        state.click(id.clone(), egui::Modifiers::NONE, order);
    }
    resp.context_menu(|ui| {
        menus.item_menu(ui, entry, state.selected.len().max(1), menu_choices);
    });
    if resp.drag_started() {
        state.rename_pending = None;
        // The whole selection, in panel order: the timeline appends them
        // one after the other.
        let dragged: Vec<I> = order
            .iter()
            .filter(|i| state.selected.contains(i))
            .cloned()
            .collect();
        state.dragging = dragged.clone();
        egui::DragAndDrop::set_payload(ui.ctx(), ItemsDrag(dragged.clone()));
        events.push(PoolEvent::DragStarted(dragged));
    }
    accept_drop(&resp, state, entry.folder.as_ref(), folders, events);
    if resp.double_clicked() {
        state.rename_pending = None;
        events.push(PoolEvent::Open(id.clone()));
    }
    if resp.dragged() {
        let count = state.dragging.len();
        let ghost = if count > 1 {
            t!("pool.items", count = count).into_owned()
        } else {
            entry.name.clone()
        };
        media_pool_ui::show_drag_ghost(ui, interact_id, &ghost);
    }
    rect
}

fn paint_selected(ui: &egui::Ui, rect: egui::Rect, rounding: f32) {
    ui.painter().rect_stroke(
        rect,
        rounding,
        egui::Stroke::new(2.0, egui::Color32::WHITE),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .rect_filled(rect, rounding, egui::Color32::from_white_alpha(18));
}

fn paint_thumb(ui: &mut egui::Ui, thumb: &Thumb, meta: &vv_core::MediaMeta) {
    let thumb_size = egui::vec2(64.0, 36.0);
    let (rect, _) = ui.allocate_exact_size(thumb_size, egui::Sense::hover());
    if let Thumb::Texture(texture) = thumb {
        let tex_size = texture.size_vec2();
        let scale = (thumb_size.x / tex_size.x).min(thumb_size.y / tex_size.y);
        ui.painter().rect_filled(rect, 2.0, egui::Color32::BLACK);
        egui::Image::new(texture)
            .fit_to_exact_size(tex_size * scale)
            .paint_at(
                ui,
                egui::Rect::from_center_size(rect.center(), tex_size * scale),
            );
        return;
    }
    ui.painter()
        .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    match thumb {
        Thumb::Offline => paint_offline_thumbnail(ui.painter(), rect),
        Thumb::Timeline => paint_film_icon(
            ui.painter(),
            rect,
            ui.visuals().weak_text_color(),
            ui.visuals().extreme_bg_color,
        ),
        _ if !meta.has_video => {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "🔊",
                egui::FontId::proportional(18.0),
                ui.visuals().weak_text_color(),
            );
        }
        _ => {}
    }
}

fn meta_line<I, F>(entry: &PoolEntry<I, F>) -> std::borrow::Cow<'static, str> {
    let meta = &entry.meta;
    if meta.is_image() {
        t!("pool.meta_image", width = meta.width, height = meta.height)
    } else if meta.has_video {
        format!(
            "{}x{} · {:.2}fps · {}",
            meta.width,
            meta.height,
            meta.fps.as_f64(),
            if meta.has_audio {
                t!("pool.audio")
            } else {
                t!("pool.muted")
            }
        )
        .into()
    } else {
        t!(
            "pool.meta_audio",
            rate = meta.sample_rate,
            channels = meta.channels
        )
    }
}

fn proxy_badge(ui: &mut egui::Ui, proxy: Option<proxy_worker::ProxyState>) {
    match proxy {
        Some(proxy_worker::ProxyState::Generating(f)) => {
            media_pool_ui::proxy_progress_ring(ui, Some(f)).on_hover_text(t!(
                "pool.proxy_progress",
                percent = format!("{:.0}", f * 100.0)
            ));
        }
        Some(proxy_worker::ProxyState::Queued) => {
            media_pool_ui::proxy_progress_ring(ui, None).on_hover_text(t!("pool.proxy_queued"));
        }
        Some(proxy_worker::ProxyState::Failed) => {
            ui.colored_label(egui::Color32::RED, "!")
                .on_hover_text(t!("pool.proxy_failed"));
        }
        _ => {}
    }
}

/// Column header: every cell is clickable in full (not just the text), as
/// in a file manager list.
fn pool_header<I: PoolId, F: PoolId>(ui: &mut egui::Ui, state: &mut PoolState<I, F>) {
    use media_pool::SortKey;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, HEADER_HEIGHT), egui::Sense::hover());
    let duration_w = DURATION_COL_W.min(width);
    let (name_rect, duration_rect) = (
        egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.right() - duration_w, rect.bottom()),
        ),
        egui::Rect::from_min_max(
            egui::pos2(rect.right() - duration_w, rect.top()),
            rect.right_bottom(),
        ),
    );
    let sort = state.sort;
    for (key, label, cell) in [
        (SortKey::Name, t!("pool.name"), name_rect),
        (SortKey::Duration, t!("pool.duration"), duration_rect),
    ] {
        let resp = ui.interact(
            cell,
            ui.id().with(("media_pool_header", key as u8)),
            egui::Sense::click(),
        );
        let active = sort.key == key;
        let bg = if resp.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else if active {
            ui.visuals().widgets.active.weak_bg_fill
        } else {
            ui.visuals().widgets.inactive.weak_bg_fill
        };
        ui.painter().rect_filled(cell, 0.0, bg);
        let text_color = ui.visuals().strong_text_color();
        ui.painter().text(
            cell.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            text_color,
        );
        if active {
            // Small triangle drawn by hand instead of a character: the ones
            // in system fonts are tall and pointy, this one is squashed.
            let c = egui::pos2(cell.right() - 10.0, cell.center().y);
            let (w, h) = (4.5, 2.5);
            let points = if sort.ascending {
                vec![
                    egui::pos2(c.x - w, c.y + h),
                    egui::pos2(c.x + w, c.y + h),
                    egui::pos2(c.x, c.y - h),
                ]
            } else {
                vec![
                    egui::pos2(c.x - w, c.y - h),
                    egui::pos2(c.x + w, c.y - h),
                    egui::pos2(c.x, c.y + h),
                ]
            };
            ui.painter().add(egui::Shape::convex_polygon(
                points,
                text_color,
                egui::Stroke::NONE,
            ));
        }
        if resp.clicked() {
            state.toggle_sort(key);
        }
    }
}

pub(crate) struct SideTab<'a, T> {
    pub tab: T,
    pub hint: std::borrow::Cow<'a, str>,
    pub icon: fn(&egui::Painter, egui::Pos2, egui::Color32),
}

/// Vertical strip of icon tabs on the left of `area`. Returns the rest of
/// `area` and the response of each tab, for the callers that take drops on
/// them.
pub(crate) fn side_tabs<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    area: egui::Rect,
    current: &mut T,
    tabs: &[SideTab<'_, T>],
) -> (egui::Rect, Vec<egui::Response>) {
    const STRIP_WIDTH: f32 = 30.0;
    let strip = area.with_max_x(area.left() + STRIP_WIDTH);
    ui.painter()
        .rect_filled(strip, 0.0, ui.visuals().extreme_bg_color);
    ui.painter().vline(
        strip.right(),
        strip.y_range(),
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    let mut responses = Vec::new();
    for (i, tab) in tabs.iter().enumerate() {
        let rect = egui::Rect::from_min_size(
            strip.left_top() + egui::vec2(0.0, 2.0 + i as f32 * STRIP_WIDTH),
            egui::vec2(STRIP_WIDTH, STRIP_WIDTH),
        );
        let resp = ui
            .interact(
                rect,
                ui.id().with(("side_tab", std::any::type_name::<T>(), i)),
                egui::Sense::click(),
            )
            .on_hover_text(tab.hint.as_ref());
        if resp.clicked() {
            *current = tab.tab;
        }
        let active = *current == tab.tab;
        let color = if active {
            ui.visuals().strong_text_color()
        } else if resp.hovered() {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        if active {
            ui.painter().rect_filled(
                rect.with_max_x(rect.left() + 2.0),
                0.0,
                crate::theme::ACCENT,
            );
        }
        (tab.icon)(ui.painter(), rect.center(), color);
        responses.push(resp);
    }
    (area.with_min_x(strip.right() + 4.0), responses)
}

/// The content next to `side_tabs`, clipped to `rect`: a background that
/// senses clicks over its clip rect must not cover the tabs.
pub(crate) fn beside_tabs<R>(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.set_clip_rect(rect.intersect(ui.clip_rect()));
        add(ui)
    })
    .inner
}

/// Returns whether the query changed.
/// `salt` tells apart the search fields sharing a parent.
pub(crate) fn search_field(ui: &mut egui::Ui, search: &mut String, salt: &str) -> bool {
    let before = search.clone();
    // Right to left: the text field takes what the button leaves, margins
    // included, so it never pushes the panel wider.
    let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(
        row,
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            if !search.is_empty()
                && ui
                    .small_button("×")
                    .on_hover_text(t!("pool.search_clear"))
                    .clicked()
            {
                search.clear();
            }
            // Fixed id: the button showing up must not move its focus away.
            let resp = ui.add(
                egui::TextEdit::singleline(search)
                    .id_salt(("search_field", salt))
                    .hint_text(t!("pool.search"))
                    .desired_width(f32::INFINITY),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                search.clear();
            }
        },
    );
    *search != before
}

/// Name text field of the item being renamed: `Some(Some(name))` to
/// confirm, `Some(None)` to cancel, `None` while still editing.
fn rename_field<I, F>(ui: &mut egui::Ui, rename: &mut Rename<I, F>) -> Option<Option<String>> {
    let edit_id = ui.id().with("media_pool_rename");
    let mut output = egui::TextEdit::singleline(&mut rename.text)
        .id(edit_id)
        .desired_width(ui.available_width() - DURATION_COL_W)
        .show(ui);
    if rename.just_started {
        rename.just_started = false;
        output.response.request_focus();
        let end = egui::text::CCursor::new(rename.text.chars().count());
        output
            .state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                end,
            )));
        output.state.store(ui.ctx(), edit_id);
    }
    if !output.response.lost_focus() {
        return None;
    }
    let name = rename.text.trim();
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) || name.is_empty() {
        Some(None)
    } else {
        Some(Some(name.to_string()))
    }
}

fn indented<R>(ui: &mut egui::Ui, depth: usize, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    if depth == 0 {
        return add(ui);
    }
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * FOLDER_INDENT);
        ui.vertical(add).inner
    })
    .inner
}

/// Thumbnail of a timeline in the pool: a film strip.
fn paint_film_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    hole_color: egui::Color32,
) {
    let film = egui::Rect::from_center_size(rect.center(), egui::vec2(44.0, 28.0));
    painter.rect_filled(film, 2.0, color);
    const HOLES: usize = 6;
    let step = film.width() / HOLES as f32;
    for i in 0..HOLES {
        let x = film.left() + step * (i as f32 + 0.5);
        for y in [film.top() + 3.5, film.bottom() - 3.5] {
            painter.rect_filled(
                egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(4.0, 3.0)),
                0.5,
                hole_color,
            );
        }
    }
    let frames = film.shrink2(egui::vec2(3.0, 8.0));
    let gap = 3.0;
    let frame_w = (frames.width() - gap) / 2.0;
    for i in 0..2 {
        let left = frames.left() + i as f32 * (frame_w + gap);
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(left, frames.top()),
                egui::vec2(frame_w, frames.height()),
            ),
            1.0,
            hole_color,
        );
    }
}

/// Thumbnail of a media whose file is missing.
fn paint_offline_thumbnail(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 2.0, egui::Color32::BLACK);
    let c = rect.center();
    let (half_w, h) = (12.0, 21.0);
    let (top, bottom) = (c.y - h / 2.0, c.y + h / 2.0);
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x, top),
            egui::pos2(c.x + half_w, bottom),
            egui::pos2(c.x - half_w, bottom),
        ],
        crate::theme::ERROR,
        egui::Stroke::NONE,
    ));
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(c.x - 1.25, top + 7.0),
            egui::pos2(c.x + 1.25, bottom - 6.0),
        ),
        1.0,
        egui::Color32::BLACK,
    );
    painter.circle_filled(egui::pos2(c.x, bottom - 3.0), 1.4, egui::Color32::BLACK);
}

/// Touchpad swipe on the pool, applied before its ScrollArea's `show` like
/// the timeline's (`timeline_ui::sync_timeline_scroll`): the swipe's speed
/// becomes an inertia that keeps scrolling after release.
pub(crate) fn kinetic_pool_scroll<I, F>(
    ctx: &egui::Context,
    state: &mut PoolState<I, F>,
    enabled: bool,
) {
    if !enabled {
        state.scroll_vel = 0.0;
        return;
    }
    let Some(area) = state.scroll_area else {
        return;
    };
    let Some(mut scroll_state) = egui::containers::scroll_area::State::load(ctx, area.id) else {
        return;
    };
    let dt = ctx.input(|i| i.stable_dt).min(0.1);
    let wheel = if timeline_ui::pointer_over(ctx, area.viewport) {
        ctx.input(|i| i.smooth_scroll_delta.y)
    } else {
        0.0
    };
    if wheel != 0.0 {
        scroll_state.offset.y = (scroll_state.offset.y - wheel).clamp(0.0, area.max_offset);
        state.scroll_vel = if dt > 0.0 {
            -timeline_ui::KINETIC_VELOCITY_GAIN * wheel / dt
        } else {
            0.0
        };
        ctx.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
        scroll_state.store(ctx, area.id);
    } else if timeline_ui::apply_kinetic_scroll(
        &mut scroll_state.offset.y,
        &mut state.scroll_vel,
        area.max_offset,
        dt,
    ) {
        scroll_state.store(ctx, area.id);
        ctx.request_repaint();
    }
}
