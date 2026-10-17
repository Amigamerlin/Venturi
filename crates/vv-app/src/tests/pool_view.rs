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

#[test]
fn files_dragged_in_target_the_folder_of_the_row_under_the_pointer() {
    let ctx = egui::Context::default();
    let mut state = PoolState::<String, String>::default();
    state.expanded.insert("f".into());
    let entry = |id: &str, folder: Option<&str>| PoolEntry {
        id: id.into(),
        name: id.into(),
        folder: folder.map(Into::into),
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
    };
    let mut targets: Vec<Option<String>> = Vec::new();
    let mut inside_samples = 0;
    for y in (0..600).step_by(4) {
        let pos = egui::pos2(100.0, y as f32);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(300.0, 600.0),
            )),
            events: vec![egui::Event::PointerMoved(pos)],
            hovered_files: vec![egui::HoveredFile::default()],
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            show_pool(
                ui,
                &mut state,
                PoolView {
                    folders: vec![PoolFolder {
                        id: "f".into(),
                        name: "f".into(),
                        parent: None,
                    }],
                    entries: vec![entry("in", Some("f")), entry("out", None)],
                    menus: &NoMenus,
                    kinetic_scroll: false,
                    item_hint: "".into(),
                    empty_hint: None,
                },
            );
        })
        .textures_delta
        .clear();
        if state.file_drop_folder.is_some() {
            inside_samples += 1;
        }
        if targets.last() != Some(&state.file_drop_folder) {
            targets.push(state.file_drop_folder.clone());
        }
    }
    assert_eq!(targets, [None, Some("f".into()), None]);
    assert!(
        inside_samples * 4 > 30,
        "the folder's own items target it too, not just its row"
    );
}
