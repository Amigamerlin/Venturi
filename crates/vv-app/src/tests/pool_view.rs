use super::*;

#[test]
fn the_name_column_fills_what_the_duration_leaves_unless_resized() {
    assert_eq!(name_col_width(None, 300.0), 300.0 - DURATION_COL_W);
    assert_eq!(name_col_width(Some(150.0), 300.0), 150.0);
    assert_eq!(
        name_col_width(Some(1000.0), 300.0),
        300.0 - DURATION_COL_W,
        "the duration stays inside the panel"
    );
    assert_eq!(name_col_width(Some(10.0), 300.0), MIN_NAME_COL_W);
    assert_eq!(name_col_width(Some(150.0), 100.0), 100.0 - DURATION_COL_W);
}

struct NoMenus;

impl PoolMenus<String, String> for NoMenus {
    type Action = ();

    fn item_menu(
        &self,
        _ui: &mut egui::Ui,
        _entry: &PoolEntry<String, String>,
        _selected: usize,
        _out: &mut Vec<MenuChoice<String, ()>>,
    ) {
    }
}

/// A pool with folder `f` holding `in`, and `out` at the root.
struct Pool {
    ctx: egui::Context,
    state: PoolState<String, String>,
    items: Vec<(String, Option<String>)>,
    /// Folder ids with their parent; the name is the id.
    folders: Vec<(String, Option<String>)>,
    layout: PoolLayout,
    /// Width the pool took in the last frame.
    used_width: f32,
    time: f64,
}

impl Pool {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            state: PoolState::default(),
            items: vec![("in".into(), Some("f".into())), ("out".into(), None)],
            folders: vec![("f".into(), None)],
            layout: PoolLayout::Details,
            used_width: 0.0,
            time: 0.0,
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>, files: bool) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(300.0, 600.0),
            )),
            events,
            time: Some(self.time),
            hovered_files: if files {
                vec![egui::HoveredFile::default()]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let mut entries = Some(
            self.items
                .iter()
                .map(|(id, folder)| PoolEntry {
                    id: id.clone(),
                    name: id.clone(),
                    folder: folder.clone(),
                    meta: vv_core::MediaMeta {
                        duration_frames: 25,
                        fps: vv_core::Rational::new(25, 1),
                        width: 0,
                        height: 0,
                        has_video: true,
                        has_audio: false,
                        sample_rate: 0,
                        channels: 0,
                        audio_streams: 0,
                        file: Default::default(),
                    },
                    thumb: Thumb::None,
                    proxy: None,
                    renamable: true,
                })
                .collect(),
        );
        self.time += 1.0 / 60.0;
        let mut events = Vec::new();
        self.ctx
            .run_ui(input, |ui| {
                events = show_pool(
                    ui,
                    &mut self.state,
                    PoolView {
                        folders: self
                            .folders
                            .iter()
                            .map(|(id, parent)| PoolFolder {
                                id: id.clone(),
                                name: id.clone(),
                                parent: parent.clone(),
                            })
                            .collect(),
                        entries: entries.take().unwrap_or_default(),
                        menus: &NoMenus,
                        kinetic_scroll: false,
                        layout: &mut self.layout,
                        item_hint: "".into(),
                        empty_hint: None,
                    },
                );
                self.used_width = ui.min_rect().width();
            })
            .textures_delta
            .clear();
        for event in events {
            if let PoolEvent::MoveItems(ids, folder) = event {
                for (id, f) in &mut self.items {
                    if ids.contains(id) {
                        *f = folder.clone();
                    }
                }
            }
        }
    }

    fn hover_files(&mut self, pos: egui::Pos2) -> Option<String> {
        self.frame(vec![egui::Event::PointerMoved(pos)], true);
        self.state.file_drop_folder.clone()
    }

    fn click(&mut self, pos: egui::Pos2) {
        self.frame(vec![egui::Event::PointerMoved(pos)], false);
        self.frame(vec![button(pos, true)], false);
        self.frame(vec![button(pos, false)], false);
        // Out of double click reach of the next one.
        self.frame(
            vec![egui::Event::PointerMoved(egui::pos2(299.0, 599.0))],
            false,
        );
    }

    /// The item a click at `pos` selects.
    fn item_at(&mut self, pos: egui::Pos2) -> Option<String> {
        self.state.clear();
        self.click(pos);
        self.state.single_selected().cloned()
    }

    /// The top of the folder's row, found by hovering files over it.
    fn folder_top(&mut self) -> f32 {
        (0..600)
            .map(|y| y as f32)
            .find(|&y| self.hover_files(egui::pos2(40.0, y)).is_some())
            .expect("the folder row is shown")
    }
}

fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

#[test]
fn files_dragged_in_target_the_folder_of_the_row_under_the_pointer() {
    let mut pool = Pool::new();
    pool.state.expanded.insert("f".into());
    let mut targets: Vec<Option<String>> = Vec::new();
    let mut inside_samples = 0;
    for y in (0..600).step_by(4) {
        let target = pool.hover_files(egui::pos2(100.0, y as f32));
        if target.is_some() {
            inside_samples += 1;
        }
        if targets.last() != Some(&target) {
            targets.push(target);
        }
    }
    assert_eq!(targets, [None, Some("f".into()), None]);
    assert!(
        inside_samples * 4 > 30,
        "the folder's own items target it too, not just its row"
    );
}

#[test]
fn the_folder_arrow_opens_it_without_dropping_the_selection() {
    let mut pool = Pool::new();
    pool.state.selected.insert("out".into());
    let arrow = egui::pos2(f32::from(SIDE_PAD) + 8.0, pool.folder_top() + 12.0);
    pool.click(arrow);
    assert!(pool.state.expanded.contains("f"));
    assert!(pool.state.selected.contains("out"));
    assert_eq!(pool.state.selected_folder, None);

    pool.click(arrow + egui::vec2(100.0, 0.0));
    assert!(!pool.state.expanded.contains("f"));
    assert_eq!(pool.state.selected_folder.as_deref(), Some("f"));
}

#[test]
fn items_moved_into_a_closed_folder_open_it_and_stay_selected() {
    let mut pool = Pool::new();
    let folder = egui::pos2(150.0, pool.folder_top() + 12.0);
    let out = (folder.y as i32 + 20..600)
        .map(|y| egui::pos2(150.0, y as f32))
        .find(|&pos| {
            pool.click(pos);
            pool.state.selected.contains("out")
        })
        .expect("the root item is shown");

    pool.frame(vec![egui::Event::PointerMoved(out)], false);
    pool.frame(vec![button(out, true)], false);
    for step in 1..=10 {
        let pos = out.lerp(folder, step as f32 / 10.0);
        pool.frame(vec![egui::Event::PointerMoved(pos)], false);
    }
    pool.frame(vec![button(folder, false)], false);
    pool.frame(Vec::new(), false);

    assert_eq!(pool.items[1], ("out".into(), Some("f".into())));
    assert!(pool.state.expanded.contains("f"));
    assert!(pool.state.selected.contains("out"));
}

fn root_items(layout: PoolLayout) -> Pool {
    let mut pool = Pool::new();
    pool.items = vec![("a".into(), None), ("b".into(), None)];
    pool.folders.clear();
    pool.layout = layout;
    pool
}

#[test]
fn the_grid_puts_items_side_by_side() {
    for (layout, side_by_side) in [
        (PoolLayout::Grid, true),
        (PoolLayout::List, false),
        (PoolLayout::Details, false),
    ] {
        let mut pool = root_items(layout);
        let first = (0..600)
            .step_by(4)
            .map(|y| egui::pos2(40.0, y as f32))
            .find(|&pos| pool.item_at(pos).as_deref() == Some("a"))
            .expect("the first item is shown");
        // In the second column.
        let beside = egui::pos2(150.0, first.y);
        assert_eq!(
            pool.item_at(beside).as_deref() == Some("b"),
            side_by_side,
            "{layout:?}"
        );
    }
}

#[test]
fn the_list_takes_less_room_per_item_than_the_details() {
    let height = |layout| {
        let mut pool = root_items(layout);
        (0..600)
            .filter(|&y| pool.item_at(egui::pos2(150.0, y as f32)).as_deref() == Some("a"))
            .count()
    };
    let (list, details) = (height(PoolLayout::List), height(PoolLayout::Details));
    assert!(
        list > 10 && list < details,
        "list {list}, details {details}"
    );
}

#[test]
fn the_layout_buttons_switch_the_layout() {
    let mut pool = Pool::new();
    // Right to left: grid, list, details, then the search field.
    pool.click(egui::pos2(290.0, 10.0));
    assert_eq!(pool.layout, PoolLayout::Grid);
    pool.click(egui::pos2(268.0, 10.0));
    assert_eq!(pool.layout, PoolLayout::List);
}

#[test]
fn the_pool_never_asks_for_more_width_than_it_has() {
    for layout in [PoolLayout::Details, PoolLayout::List, PoolLayout::Grid] {
        let mut pool = root_items(layout);
        pool.items = (0..12).map(|i| (format!("item {i}"), None)).collect();
        pool.frame(Vec::new(), false);
        pool.frame(Vec::new(), false);
        assert!(pool.used_width <= 300.0, "{layout:?}: {}", pool.used_width);
    }
}

#[test]
fn grid_folders_are_tiles_opened_by_a_double_click() {
    let mut pool = Pool::new();
    pool.layout = PoolLayout::Grid;
    pool.folders.push(("sub".into(), Some("f".into())));
    let top = pool.folder_top();
    // The folder tile comes first, the root item beside it.
    let folder = egui::pos2(40.0, top + 30.0);
    assert_eq!(
        pool.item_at(egui::pos2(150.0, folder.y)).as_deref(),
        Some("out")
    );

    pool.click(folder);
    assert_eq!(pool.state.selected_folder.as_deref(), Some("f"));
    // Far enough from the clicks above not to make a triple click.
    for _ in 0..60 {
        pool.frame(vec![egui::Event::PointerMoved(folder)], false);
    }
    for _ in 0..2 {
        pool.frame(vec![button(folder, true)], false);
        pool.frame(vec![button(folder, false)], false);
    }
    pool.frame(Vec::new(), false);
    assert_eq!(pool.state.grid_folder.as_deref(), Some("f"));

    // Inside: the subfolder, then the item; dropped files land in `f`.
    assert_eq!(
        pool.item_at(egui::pos2(150.0, folder.y)).as_deref(),
        Some("in")
    );
    assert_eq!(
        pool.hover_files(egui::pos2(150.0, 500.0)).as_deref(),
        Some("f")
    );
}
