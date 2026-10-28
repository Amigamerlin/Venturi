use super::*;

#[test]
fn mark_menu_items_follow_the_active_preview_without_a_timeline_fallback() {
    let mut app = VenturiApp::default();
    let timeline = app.ensure_timeline();
    let clip = app.session.project.alloc_clip_id();
    app.session.project.timelines[timeline].tracks[0]
        .clips
        .push(vv_core::Clip::from_source_range(
            clip,
            vv_core::ClipSource::SolidColor,
            0,
            100,
            0,
            vv_core::Rational::one(),
        ));
    let meta = vv_core::MediaMeta {
        duration_frames: 100,
        fps: vv_core::Rational::new(25, 1),
        width: 64,
        height: 48,
        has_video: true,
        has_audio: false,
        sample_rate: 0,
        channels: 0,
        audio_streams: 0,
        file: Default::default(),
    };
    let media = app.session.project.media_pool.insert(vv_core::MediaItem {
        path: "preview.mp4".into(),
        meta: meta.clone(),
        content_hash: 1,
        compound: None,
        folder: None,
    });
    app.browsing = Some(Browsed::Media(media));
    app.preview_meta = Some(meta.clone());
    app.preview_error = Some("cannot open preview".into());
    let check_enabled = |app: &mut VenturiApp, expected: bool| {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            for is_in in [true, false] {
                assert_eq!(app.show_mark_menu_item(ui, is_in).enabled(), expected);
            }
        });
        output.textures_delta.clear();
    };
    check_enabled(&mut app, false);
    click_mark_item(&mut app, false);
    assert_eq!(app.browse_marks.resolve(100), (0, 100));
    app.preview_error = None;
    app.preview_meta = None;
    check_enabled(&mut app, false);
    app.preview_meta = Some(meta);
    check_enabled(&mut app, true);
    app.browse_playhead = 20;
    click_mark_item(&mut app, true);
    app.browse_playhead = 59;
    click_mark_item(&mut app, false);
    assert_eq!(app.browse_marks.resolve(100), (20, 60));
    assert_eq!(app.timeline_state.export_marks.resolve(100), (0, 100));
    app.session.project.media_pool.remove(media);
    check_enabled(&mut app, false);
}

fn menu_frame(
    app: &mut VenturiApp,
    ctx: &egui::Context,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut input = egui::RawInput::default();
    input.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(1280.0, 900.0),
    ));
    input.events = events;
    let mut output = ctx.run_ui(input, |ui| app.show_menu_bar(ui));
    output.textures_delta.clear();
    output
}

fn menu_text_position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
    fn find(shape: &egui::epaint::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                Some(egui::Rect::from_min_size(text.pos, text.galley.size()).center())
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
            _ => None,
        }
    }
    output
        .shapes
        .iter()
        .find_map(|shape| find(&shape.shape, label))
        .unwrap_or_else(|| panic!("menu entry {label:?} was not rendered"))
}

fn click_menu_position(
    app: &mut VenturiApp,
    ctx: &egui::Context,
    pos: egui::Pos2,
) -> egui::FullOutput {
    for pressed in [true, false] {
        menu_frame(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    menu_frame(app, ctx, Vec::new())
}

#[test]
fn timeline_menu_registers_both_export_mark_actions() {
    let mut app = VenturiApp::default();
    let timeline = app.ensure_timeline();
    let id = app.session.project.alloc_clip_id();
    app.session.project.timelines[timeline].tracks[0]
        .clips
        .push(vv_core::Clip::from_source_range(
            id,
            vv_core::ClipSource::SolidColor,
            0,
            100,
            0,
            vv_core::Rational::one(),
        ));
    let ctx = egui::Context::default();
    for (action, frame) in [(Action::MarkIn, 20), (Action::MarkOut, 59)] {
        app.timeline_state.playhead = frame;
        let output = menu_frame(&mut app, &ctx, Vec::new());
        let pos = menu_text_position(&output, &t!("menu.timeline"));
        let output = click_menu_position(&mut app, &ctx, pos);
        let label = app.settings.keymap.menu_label(&action.label(), action);
        let pos = menu_text_position(&output, &label);
        click_menu_position(&mut app, &ctx, pos);
        assert_eq!(
            app.timeline_state.export_marks.resolve(100),
            if frame == 20 { (20, 100) } else { (20, 60) }
        );
    }
}

#[test]
fn mark_menu_items_stay_disabled_on_an_empty_timeline() {
    let mut app = VenturiApp::default();
    let timeline = app.ensure_timeline();
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        assert!(!app.show_mark_menu_item(ui, true).enabled());
        assert!(!app.show_mark_menu_item(ui, false).enabled());
    });
    output.textures_delta.clear();
    click_mark_item(&mut app, false);
    let id = app.session.project.alloc_clip_id();
    app.session.project.timelines[timeline].tracks[0]
        .clips
        .push(vv_core::Clip::from_source_range(
            id,
            vv_core::ClipSource::SolidColor,
            0,
            100,
            0,
            vv_core::Rational::one(),
        ));
    assert_eq!(app.timeline_state.export_marks.resolve(100), (0, 100));
}

fn click_mark_item(app: &mut VenturiApp, is_in: bool) {
    let ctx = egui::Context::default();
    let mut rect = egui::Rect::NOTHING;
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        rect = app.show_mark_menu_item(ui, is_in).rect;
    });
    output.textures_delta.clear();
    let mut input = egui::RawInput::default();
    input.events = vec![
        egui::Event::PointerMoved(rect.center()),
        egui::Event::PointerButton {
            pos: rect.center(),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
    ];
    let mut output = ctx.run_ui(input.clone(), |ui| {
        app.show_mark_menu_item(ui, is_in);
    });
    output.textures_delta.clear();
    input.events[1] = egui::Event::PointerButton {
        pos: rect.center(),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    };
    let mut output = ctx.run_ui(input, |ui| {
        app.show_mark_menu_item(ui, is_in);
    });
    output.textures_delta.clear();
}

#[test]
fn mark_menu_items_set_the_active_timeline_export_range() {
    let mut app = VenturiApp::default();
    let timeline_id = app.ensure_timeline();
    let id = app.session.project.alloc_clip_id();
    app.session.project.timelines[timeline_id].tracks[0]
        .clips
        .push(vv_core::Clip::from_source_range(
            id,
            vv_core::ClipSource::SolidColor,
            0,
            100,
            0,
            vv_core::Rational::one(),
        ));
    app.timeline_state.playhead = 20;
    click_mark_item(&mut app, true);
    app.timeline_state.playhead = 59;
    click_mark_item(&mut app, false);
    assert_eq!(app.timeline_state.export_marks.resolve(100), (20, 60));
}

#[test]
fn mark_menu_items_are_disabled_without_active_content() {
    let mut app = VenturiApp::default();
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        assert!(!app.show_mark_menu_item(ui, true).enabled());
        assert!(!app.show_mark_menu_item(ui, false).enabled());
    });
    output.textures_delta.clear();
}

/// Checks that clicking the File menu's Exit item requests a normal close.
#[test]
fn exit_menu_item_requests_window_close() {
    let app = VenturiApp::default();
    let ctx = egui::Context::default();
    let mut first_frame = egui::RawInput::default();
    first_frame.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(800.0, 600.0),
    ));
    let mut exit_rect = None;
    let mut output = ctx.run_ui(first_frame, |ui| {
        exit_rect = Some(app.show_exit_menu_item(ui).rect);
    });
    output.textures_delta.clear();
    let exit_rect = exit_rect.expect("Exit item was drawn");
    let position = exit_rect.center();

    let mut press = egui::RawInput::default();
    press.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(800.0, 600.0),
    ));
    press.events.extend([
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    let mut output = ctx.run_ui(press, |ui| {
        app.show_exit_menu_item(ui);
    });
    output.textures_delta.clear();

    let mut release = egui::RawInput::default();
    release.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(800.0, 600.0),
    ));
    release.events.extend([
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    let mut output = ctx.run_ui(release, |ui| {
        app.show_exit_menu_item(ui);
    });

    let requested_close = output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .is_some_and(|viewport| viewport.commands.contains(&egui::ViewportCommand::Close));
    output.textures_delta.clear();
    assert!(requested_close);
}
