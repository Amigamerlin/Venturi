use super::*;
use crate::{History, MediaItem, MediaMeta, Track};

const ORIGIN: ClipboardOrigin = ClipboardOrigin {
    instance: 7,
    epoch: 1,
};

fn meta() -> MediaMeta {
    MediaMeta {
        duration_frames: 100,
        fps: Rational::new(30, 1),
        width: 64,
        height: 48,
        has_video: true,
        has_audio: false,
        sample_rate: 48_000,
        channels: 2,
        audio_streams: 1,
        file: Default::default(),
    }
}

fn file(path: &str, content_hash: u64) -> MediaItem {
    MediaItem {
        path: path.into(),
        meta: meta(),
        content_hash,
        compound: None,
        folder: None,
    }
}

fn media_clip(project: &mut Project, media: MediaId, start: FrameIdx) -> Clip {
    Clip::from_source_range(
        project.alloc_clip_id(),
        ClipSource::Media(media),
        0,
        10,
        start,
        Rational::one(),
    )
}

fn entry(clip: Clip) -> ClipboardEntry {
    ClipboardEntry {
        track_kind: TrackKind::Video,
        track_number: 1,
        relative_start: clip.timeline_start,
        clip,
        timeline_fps: Rational::new(30, 1),
        link_tag: None,
    }
}

/// A compound clip whose timeline uses `inner`.
fn compound(project: &mut Project, name: &str, inner: MediaId) -> MediaId {
    let mut track = Track::new(TrackKind::Video);
    track.clips = vec![media_clip(project, inner, 0)];
    let timeline = project.timelines.insert(Timeline {
        name: name.into(),
        fps: Rational::new(30, 1),
        resolution: (64, 48),
        tracks: vec![track],
        markers: Vec::new(),
        master: Default::default(),
    });
    let item = project.timeline_item(timeline, &project.timelines[timeline].clone(), None);
    let id = project.media_pool.insert(item);
    project.sync_compound_meta(id);
    id
}

fn paste(payload: ClipboardPayload, project: &mut Project) -> Pasted {
    let text = payload.to_text();
    let payload = ClipboardPayload::from_text(&text)
        .expect("a payload")
        .expect("well formed");
    let mut add = AddEntities::new(crate::CommandLabel::PasteClips);
    let pasted = payload.merge_into(project, &mut add);
    History::default().do_command(project, Box::new(add));
    pasted
}

fn source_of(entry: &ClipboardEntry) -> MediaId {
    match entry.clip.source {
        ClipSource::Media(id) => id,
        _ => panic!("not a media clip"),
    }
}

#[test]
fn foreign_text_is_not_a_payload() {
    assert!(ClipboardPayload::from_text("hello").is_none());
    assert!(ClipboardPayload::from_text("venturi:clip").is_none());
}

#[test]
fn a_newer_version_is_refused() {
    let text = "venturi-clipboard/2\n{}";
    assert!(matches!(
        ClipboardPayload::from_text(text),
        Some(Err(PayloadError::UnsupportedVersion(2)))
    ));
}

#[test]
fn pasted_media_point_to_the_same_files_at_the_root() {
    let mut source = Project::default();
    // Shifts the ids, so they collide with the destination's.
    source.media_pool.insert(file("/src/unused.mp4", 1));
    let a = source.media_pool.insert(file("/footage/a.mp4", 2));
    let clip = media_clip(&mut source, a, 5);
    let payload = ClipboardPayload::new(ORIGIN, &source, &[], &[entry(clip)]);

    let mut dest = Project::default();
    let other = dest.media_pool.insert(file("/dest/other.mp4", 3));
    let folder = dest.folders.insert(crate::MediaFolder {
        name: "F".into(),
        parent: None,
    });
    dest.media_pool.get_mut(other).unwrap().folder = Some(folder);
    let pasted = paste(payload, &mut dest);

    assert_eq!(dest.media_pool.len(), 2);
    let new = source_of(&pasted.clips[0]);
    assert_ne!(new, other);
    let item = &dest.media_pool[new];
    assert_eq!(item.path, std::path::Path::new("/footage/a.mp4"));
    assert_eq!(item.folder, None);
    assert_eq!(pasted.clips[0].relative_start, 5);
}

#[test]
fn a_file_already_in_the_pool_is_reused_by_path_or_content() {
    let mut source = Project::default();
    let a = source.media_pool.insert(file("/footage/a.mp4", 10));
    let b = source.media_pool.insert(file("/old/b.mp4", 20));
    let clips = [
        media_clip(&mut source, a, 0),
        media_clip(&mut source, b, 10),
    ];
    let payload = ClipboardPayload::new(ORIGIN, &source, &[], &clips.map(entry));

    let mut dest = Project::default();
    let dest_a = dest.media_pool.insert(file("/footage/a.mp4", 0));
    let dest_b = dest.media_pool.insert(file("/moved/b.mp4", 20));
    let pasted = paste(payload, &mut dest);

    assert_eq!(dest.media_pool.len(), 2);
    assert_eq!(source_of(&pasted.clips[0]), dest_a);
    assert_eq!(source_of(&pasted.clips[1]), dest_b);
}

#[test]
fn an_unknown_hash_does_not_match_another_unknown_hash() {
    let mut source = Project::default();
    let a = source.media_pool.insert(file("/footage/a.mp4", 0));
    let payload = ClipboardPayload::new(ORIGIN, &source, &[a], &[]);

    let mut dest = Project::default();
    dest.media_pool.insert(file("/footage/z.mp4", 0));
    let pasted = paste(payload, &mut dest);

    assert_eq!(dest.media_pool.len(), 2);
    assert_eq!(
        dest.media_pool[pasted.media[0]].path,
        std::path::Path::new("/footage/a.mp4")
    );
}

#[test]
fn a_compound_clip_brings_its_timeline_and_media_under_a_free_name() {
    let mut source = Project::default();
    let inner = source.media_pool.insert(file("/footage/inner.mp4", 5));
    let nested = compound(&mut source, "Compound Clip 1", inner);
    let clip = media_clip(&mut source, nested, 0);
    let payload = ClipboardPayload::new(ORIGIN, &source, &[], &[entry(clip)]);

    let mut dest = Project::default();
    let own_inner = dest.media_pool.insert(file("/dest/x.mp4", 9));
    compound(&mut dest, "Compound Clip 1", own_inner);
    let pasted = paste(payload, &mut dest);

    let item = &dest.media_pool[source_of(&pasted.clips[0])];
    assert_eq!(item.path, std::path::Path::new("Compound Clip 1 2"));
    let timeline = &dest.timelines[item.compound.expect("a compound clip")];
    assert_eq!(timeline.name, "Compound Clip 1 2");
    let ClipSource::Media(used) = timeline.tracks[0].clips[0].source else {
        panic!("not a media clip");
    };
    assert_eq!(
        dest.media_pool[used].path,
        std::path::Path::new("/footage/inner.mp4")
    );
}

#[test]
fn an_offline_clip_stays_offline() {
    let mut source = Project::default();
    let gone = source.media_pool.insert(file("/footage/gone.mp4", 1));
    let clip = media_clip(&mut source, gone, 0);
    source.media_pool.remove(gone);
    let payload = ClipboardPayload::new(ORIGIN, &source, &[], &[entry(clip)]);

    let mut dest = Project::default();
    dest.media_pool.insert(file("/dest/a.mp4", 2));
    dest.media_pool.insert(file("/dest/b.mp4", 3));
    let pasted = paste(payload, &mut dest);

    assert!(!dest.media_pool.contains_key(source_of(&pasted.clips[0])));
}

/// The library copies every file of a compound clip and points the payload
/// at the copies.
#[test]
fn the_files_of_a_payload_include_those_inside_compound_clips() {
    let mut project = Project::default();
    let inner = project.media_pool.insert(file("/media/inner.mp4", 1));
    let outer = compound(&mut project, "Compound Clip 1", inner);
    let mut payload = ClipboardPayload::new(ORIGIN, &project, &[outer], &[]);

    assert_eq!(
        payload
            .media_items()
            .map(|m| m.compound.is_some())
            .collect::<Vec<_>>(),
        vec![true]
    );
    for item in payload.files_mut() {
        item.path = "/library/inner.mp4".into();
    }

    let mut target = Project::default();
    paste(payload, &mut target);
    let paths: Vec<_> = target
        .media_pool
        .values()
        .filter(|m| m.compound.is_none())
        .map(|m| m.path.clone())
        .collect();
    assert_eq!(paths, vec![std::path::PathBuf::from("/library/inner.mp4")]);
}
