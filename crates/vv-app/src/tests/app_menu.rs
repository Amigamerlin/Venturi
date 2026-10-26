use super::*;

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
