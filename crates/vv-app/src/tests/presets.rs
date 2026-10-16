use super::*;
use vv_core::{ClipSource, Interpolation, Rational, Track, TransformParam};

fn app_with_timeline(fps: i32, resolution: (u32, u32)) -> (VenturiApp, usize) {
    let mut app = VenturiApp::default();
    let timeline_id = app.session.project.timelines.insert(vv_core::Timeline {
        name: "T".into(),
        fps: Rational::new(fps, 1),
        resolution,
        tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        markers: Vec::new(),
        master: Default::default(),
    });
    app.timeline_id = Some(timeline_id);
    (app, 0)
}

fn add_clip(app: &mut VenturiApp, track: usize, start: FrameIdx, len: FrameIdx) -> ClipId {
    let id = app.session.project.alloc_clip_id();
    let clip = Clip::from_source_range(id, ClipSource::SolidColor, 0, len, start, Rational::one());
    let timeline_id = app.timeline_id.unwrap();
    app.session.project.timelines[timeline_id].tracks[track].insert_sorted(clip);
    id
}

fn clip_of(app: &VenturiApp, track: usize, id: ClipId) -> &Clip {
    app.session.project.timelines[app.timeline_id.unwrap()]
        .clip(track, id)
        .unwrap()
}

fn clip_mut(app: &mut VenturiApp, track: usize, id: ClipId) -> &mut Clip {
    let timeline_id = app.timeline_id.unwrap();
    app.session.project.timelines[timeline_id]
        .clip_mut(track, id)
        .unwrap()
}

/// Saves a preset of `attributes` from the clip through the dialog.
fn save_preset(
    app: &mut VenturiApp,
    track: usize,
    id: ClipId,
    attributes: &[Attribute],
    anchor: KeyframeAnchor,
) -> AttributePreset {
    app.open_save_preset_dialog(&[(track, id)]);
    let mut dialog = app.save_preset.take().expect("the dialog opens");
    dialog.preset.name = "Punch in".into();
    dialog.preset.anchor = anchor;
    dialog.preset.attributes = attributes.to_vec();
    dialog.preset
}

fn zoom_keyframes(clip: &Clip) -> Vec<FrameIdx> {
    clip.effects
        .transform
        .track(TransformParam::ZoomX)
        .keyframes()
        .iter()
        .map(|k| k.0)
        .collect()
}

#[test]
fn end_anchored_keyframes_keep_their_distance_from_the_end() {
    let (mut app, track) = app_with_timeline(30, (1920, 1080));
    let source = add_clip(&mut app, track, 0, 20);
    clip_mut(&mut app, track, source)
        .effects
        .transform
        .track_mut(TransformParam::ZoomX)
        .upsert(15, 2.0, Interpolation::Linear);
    let preset = save_preset(
        &mut app,
        track,
        source,
        &[Attribute::Zoom],
        KeyframeAnchor::End,
    );
    let target = add_clip(&mut app, track, 100, 40);

    app.apply_preset(&preset, &[(track, target)]);

    assert_eq!(zoom_keyframes(clip_of(&app, track, target)), vec![35]);
}

#[test]
fn keyframes_keep_their_time_on_a_timeline_of_another_fps() {
    let (mut app, track) = app_with_timeline(30, (1920, 1080));
    let source = add_clip(&mut app, track, 0, 60);
    clip_mut(&mut app, track, source)
        .effects
        .transform
        .track_mut(TransformParam::ZoomX)
        .upsert(15, 2.0, Interpolation::Linear);
    let preset = save_preset(
        &mut app,
        track,
        source,
        &[Attribute::Zoom],
        KeyframeAnchor::Start,
    );

    let (mut other, track) = app_with_timeline(60, (1920, 1080));
    let target = add_clip(&mut other, track, 0, 120);
    other.apply_preset(&preset, &[(track, target)]);

    assert_eq!(
        zoom_keyframes(clip_of(&other, track, target)),
        vec![30],
        "half a second in"
    );
}

#[test]
fn positions_follow_the_resolution_of_the_target_timeline() {
    let (mut app, track) = app_with_timeline(30, (1920, 1080));
    let source = add_clip(&mut app, track, 0, 20);
    let transform = &mut clip_mut(&mut app, track, source).effects.transform;
    transform.track_mut(TransformParam::PositionX).default = 100.0;
    transform.track_mut(TransformParam::PositionY).default = -50.0;
    let preset = save_preset(
        &mut app,
        track,
        source,
        &[Attribute::Position],
        KeyframeAnchor::Start,
    );

    let (mut other, track) = app_with_timeline(30, (960, 2160));
    let target = add_clip(&mut other, track, 0, 20);
    other.apply_preset(&preset, &[(track, target)]);

    let transform = &clip_of(&other, track, target).effects.transform;
    assert_eq!(transform.track(TransformParam::PositionX).default, 50.0);
    assert_eq!(transform.track(TransformParam::PositionY).default, -100.0);
}

#[test]
fn appended_filters_keep_the_ones_of_the_clip() {
    let (mut app, track) = app_with_timeline(30, (1920, 1080));
    let source = add_clip(&mut app, track, 0, 20);
    clip_mut(&mut app, track, source)
        .effects
        .filters
        .push(vv_core::ClipFilter::new(vv_core::FilterKind::DropShadow));
    let mut preset = save_preset(
        &mut app,
        track,
        source,
        &[Attribute::Filters],
        KeyframeAnchor::Start,
    );
    let target = add_clip(&mut app, track, 100, 20);
    clip_mut(&mut app, track, target)
        .effects
        .filters
        .push(vv_core::ClipFilter::new(vv_core::FilterKind::Exposure));

    preset.append_filters = true;
    app.apply_preset(&preset, &[(track, target)]);
    let kinds: Vec<_> = clip_of(&app, track, target)
        .effects
        .filters
        .iter()
        .map(|f| f.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            vv_core::FilterKind::Exposure,
            vv_core::FilterKind::DropShadow
        ]
    );

    preset.append_filters = false;
    app.apply_preset(&preset, &[(track, target)]);
    assert_eq!(clip_of(&app, track, target).effects.filters.len(), 1);
}

#[test]
fn applying_a_preset_is_a_single_undo_step_on_every_target() {
    let (mut app, track) = app_with_timeline(30, (1920, 1080));
    let source = add_clip(&mut app, track, 0, 20);
    clip_mut(&mut app, track, source)
        .effects
        .transform
        .track_mut(TransformParam::ZoomX)
        .default = 2.0;
    let preset = save_preset(
        &mut app,
        track,
        source,
        &[Attribute::Zoom],
        KeyframeAnchor::Start,
    );
    let targets = [
        (track, add_clip(&mut app, track, 100, 20)),
        (track, add_clip(&mut app, track, 200, 20)),
    ];

    app.apply_preset(&preset, &targets);
    let zoom = |app: &VenturiApp, id| {
        clip_of(app, track, id)
            .effects
            .transform
            .track(TransformParam::ZoomX)
            .default
    };
    assert!(targets.iter().all(|&(_, id)| zoom(&app, id) == 2.0));

    app.session.history.undo(&mut app.session.project);
    assert!(targets.iter().all(|&(_, id)| zoom(&app, id) == 1.0));
}

#[test]
fn the_hotkey_applies_its_preset_to_the_selection() {
    let (mut app, track) = app_with_timeline(30, (1920, 1080));
    let source = add_clip(&mut app, track, 0, 20);
    clip_mut(&mut app, track, source)
        .effects
        .transform
        .track_mut(TransformParam::Opacity)
        .default = 40.0;
    let mut preset = save_preset(
        &mut app,
        track,
        source,
        &[Attribute::Opacity],
        KeyframeAnchor::Start,
    );
    preset.hotkey = Some(4);
    app.library.add_preset(preset).unwrap();
    let target = add_clip(&mut app, track, 100, 20);
    app.timeline_state
        .set_selection(std::collections::BTreeSet::from([(track, target)]), None);

    app.apply_preset_hotkey(3);
    app.apply_preset_hotkey(4);

    assert_eq!(
        clip_of(&app, track, target)
            .effects
            .transform
            .track(TransformParam::Opacity)
            .default,
        40.0
    );
}
