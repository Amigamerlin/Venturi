use super::*;
use vv_audio::mixer::{mix_range, timeline_frame_to_sample};
use vv_core::{
    AudioEffect, AudioEffectKind, Clip, ClipId, ClipSource, Equalizer, MediaItem, MediaMeta,
    MultibandCompressor, Rational, Track, TrackKind,
};

const FPS: u64 = 25;
const FRAME_SAMPLES: u64 = PROJECT_SAMPLE_RATE as u64 / FPS;

fn source(name: &str, args: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join("vv-audio-render-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    vv_media::test_support::ffmpeg(args, &path);
    path
}

fn chirp(name: &str, rate: u32, secs: u32) -> PathBuf {
    let lavfi = format!("aevalsrc=0.8*sin(300*t*t):sample_rate={rate}:duration={secs}");
    source(name, &["-f", "lavfi", "-i", &lavfi])
}

fn media(project: &mut Project, path: PathBuf, sample_rate: u32, audio_streams: u16) -> MediaId {
    project.media_pool.insert(MediaItem {
        path,
        meta: MediaMeta {
            duration_frames: 10_000,
            fps: Rational::new(FPS as i32, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate,
            channels: 1,
            audio_streams,
            file: Default::default(),
        },
        content_hash: 1,
        compound: None,
        folder: None,
    })
}

fn clip(id: u64, media: MediaId, source_in: u64, len: u64, start: u64) -> Clip {
    Clip::from_source_range(
        ClipId(id),
        ClipSource::Media(media),
        source_in as i64,
        (source_in + len) as i64,
        start as i64,
        Rational::one(),
    )
}

fn timeline(tracks: Vec<Track>) -> Timeline {
    Timeline {
        name: "t".into(),
        fps: Rational::new(FPS as i32, 1),
        resolution: (4, 2),
        tracks,
        markers: Vec::new(),
        master: Default::default(),
    }
}

fn audio_track(clips: Vec<Clip>) -> Track {
    Track {
        clips,
        ..Track::new(TrackKind::Audio)
    }
}

fn samples(frames: std::ops::Range<u64>) -> Range<u64> {
    frames.start * FRAME_SAMPLES..frames.end * FRAME_SAMPLES
}

/// The mix of `range` in one call, every file decoded whole from its start.
fn mixed_whole(project: &Project, tl: &Timeline, range: Range<u64>) -> Vec<f32> {
    let mut decode = |path: &Path, stream: usize| -> Option<SampleBlocks> {
        let mut samples = Vec::new();
        let formats = vv_media::decode_audio_streams_streaming(
            path,
            &[stream],
            Some(PROJECT_SAMPLE_RATE),
            |_, channels, chunk| {
                remix_channels_into(chunk, channels, PROJECT_CHANNELS, &mut samples);
                ControlFlow::Continue(())
            },
        )
        .ok()?;
        formats[0]?;
        Some(SampleBlocks::contiguous(
            Arc::new(samples),
            0,
            PROJECT_CHANNELS,
        ))
    };
    let snapshot = MixSnapshot::from_timeline(
        project,
        tl,
        PROJECT_SAMPLE_RATE,
        PROJECT_CHANNELS,
        &mut decode,
    );
    let mut out = vec![0.0; (range.end - range.start) as usize * PROJECT_CHANNELS as usize];
    mix_range(&snapshot, range.start, &mut out);
    out
}

fn rendered(project: &Project, tl: &Timeline, range: Range<u64>) -> Vec<f32> {
    let mut out = Vec::new();
    render_offline(project, tl, range, |chunk| {
        out.extend_from_slice(chunk);
        ControlFlow::Continue(())
    })
    .unwrap();
    out
}

fn effect(kind: AudioEffectKind) -> AudioEffect {
    AudioEffect::new(kind)
}

fn boosting_eq() -> Equalizer {
    let mut eq = Equalizer::DEFAULT;
    eq.bands[2].gain_db = 6.0;
    eq
}

/// Several chunks, through a compressor and an equalizer whose state must
/// carry from one chunk to the next, over a seekable file used twice, a
/// resampled one at varispeed and a pitch-corrected clip.
#[test]
fn a_render_in_chunks_equals_one_mix_of_the_whole_timeline() {
    let mut project = Project::default();
    let seekable = media(&mut project, chirp("seekable.wav", 48_000, 20), 48_000, 1);
    let resampled = media(&mut project, chirp("resampled.wav", 44_100, 20), 44_100, 1);
    let mut first = audio_track(vec![
        clip(1, seekable, 10, 300, 0),
        clip(2, seekable, 50, 120, 320),
    ]);
    first.mix.effects = vec![
        effect(AudioEffectKind::MultibandCompressor(
            MultibandCompressor::DEFAULT,
        )),
        effect(AudioEffectKind::Equalizer(boosting_eq())),
    ];
    let varispeed = Clip::new(
        ClipId(3),
        ClipSource::Media(resampled),
        0,
        40,
        200,
        Rational::one(),
        Rational::new(3, 2),
    );
    let mut stretched = clip(4, resampled, 100, 100, 260);
    stretched.set_speed(Rational::new(5, 4), Rational::one());
    stretched.pitch_correction = true;
    let tl = timeline(vec![first, audio_track(vec![varispeed, stretched])]);

    let range = samples(0..480);
    assert!(range.end - range.start > 3 * CHUNK_FRAMES);
    let whole = mixed_whole(&project, &tl, range.clone());
    let chunked = rendered(&project, &tl, range);
    assert_eq!(chunked.len(), whole.len());
    let first_diff = chunked.iter().zip(&whole).position(|(a, b)| a != b);
    assert_eq!(first_diff, None, "the chunked render differs");
    assert!(chunked.iter().any(|s| s.abs() > 0.1));
}

#[test]
fn a_range_reads_only_the_windows_near_it() {
    let mut project = Project::default();
    let file = media(&mut project, chirp("near.wav", 48_000, 20), 48_000, 1);
    let tl = timeline(vec![audio_track(vec![clip(1, file, 0, 500, 0)])]);
    let mut wanted = Wanted {
        windows: HashSet::new(),
        compounds: &HashMap::new(),
    };
    let range = samples(100..110);
    MixSnapshot::from_timeline_range(
        &project,
        &tl,
        PROJECT_SAMPLE_RATE,
        PROJECT_CHANNELS,
        &mut wanted,
        range.clone(),
    );
    assert_eq!(wanted.windows.len(), 1);
    let (_, _, frames) = wanted.windows.iter().next().unwrap();
    assert!(
        frames.start >= range.start && frames.end < range.end + PROJECT_SAMPLE_RATE as u64,
        "{frames:?}"
    );
}

/// The point of the render: what it holds does not grow with the timeline.
#[test]
fn the_render_holds_only_the_audio_near_where_it_is() {
    let mut project = Project::default();
    let file = media(&mut project, chirp("long.wav", 48_000, 60), 48_000, 1);
    let tl = timeline(vec![audio_track(vec![clip(1, file, 0, 1500, 0)])]);
    let mut audio = StreamingAudio::default();
    let end = samples(0..1500).end;
    let bound = (2 * CHUNK_FRAMES + READ_AHEAD_FRAMES) as usize * PROJECT_CHANNELS as usize;
    let mut at = 0;
    while at < end {
        let chunk = at..(at + CHUNK_FRAMES).min(end);
        audio.snapshot(&project, &tl, chunk.clone(), false).unwrap();
        // The windows share the readers' blocks.
        let held: usize = audio
            .readers
            .iter()
            .flat_map(|r| &r.kept)
            .map(|kept| kept.blocks.iter().map(|b| b.len()).sum::<usize>() + kept.filling.len())
            .sum();
        assert!(held <= bound, "{held} samples held at {chunk:?}");
        assert!(audio.readers.len() <= 1);
        at = chunk.end;
    }
}

#[test]
fn the_streams_of_a_file_read_at_the_same_place_share_one_decode() {
    let path = source(
        "two_streams.mkv",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=12",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=48000:duration=12",
            "-map",
            "0",
            "-map",
            "1",
            "-c:a",
            "pcm_s16le",
        ],
    );
    let mut project = Project::default();
    let file = media(&mut project, path, 48_000, 2);
    let mut second = clip(2, file, 0, 300, 0);
    second.audio_stream_index = 1;
    let tl = timeline(vec![
        audio_track(vec![clip(1, file, 0, 300, 0)]),
        audio_track(vec![second]),
    ]);
    let mut audio = StreamingAudio::default();
    audio
        .snapshot(&project, &tl, 0..CHUNK_FRAMES, false)
        .unwrap();
    assert_eq!(audio.readers.len(), 1);
    assert_eq!(audio.readers[0].streams, [0, 1]);

    let range = samples(0..300);
    assert_eq!(
        rendered(&project, &tl, range.clone()),
        mixed_whole(&project, &tl, range)
    );
}

#[test]
fn a_stream_the_file_does_not_have_is_silent() {
    let mut project = Project::default();
    let file = media(&mut project, chirp("one_stream.wav", 48_000, 2), 48_000, 1);
    let mut missing = clip(1, file, 0, 50, 0);
    missing.audio_stream_index = 3;
    let tl = timeline(vec![audio_track(vec![missing])]);
    let mixed = rendered(&project, &tl, samples(0..50));
    assert!(mixed.iter().all(|s| *s == 0.0));
}

#[test]
fn a_break_from_the_sink_stops_the_render() {
    let mut project = Project::default();
    let file = media(&mut project, chirp("stop.wav", 48_000, 20), 48_000, 1);
    let tl = timeline(vec![audio_track(vec![clip(1, file, 0, 500, 0)])]);
    let mut chunks = 0;
    render_offline(&project, &tl, samples(0..500), |_| {
        chunks += 1;
        ControlFlow::Break(())
    })
    .unwrap();
    assert_eq!(chunks, 1);
}

#[test]
fn a_range_starting_inside_the_timeline_is_that_part_of_the_mix() {
    let mut project = Project::default();
    let file = media(&mut project, chirp("part.wav", 48_000, 20), 48_000, 1);
    let mut track = audio_track(vec![clip(1, file, 0, 500, 0)]);
    track.mix.effects = vec![effect(AudioEffectKind::Equalizer(boosting_eq()))];
    let tl = timeline(vec![track]);
    let start = timeline_frame_to_sample(237, FPS as f64, PROJECT_SAMPLE_RATE);
    let range = start..samples(0..480).end;
    assert_eq!(
        rendered(&project, &tl, range.clone()),
        mixed_whole(&project, &tl, range)
    );
}

/// Each chunk is a new snapshot: the compound's own compressor carries on
/// across them like the outer ones.
#[test]
fn a_compound_rendered_in_chunks_equals_one_mix() {
    let mut project = Project::default();
    let file = media(&mut project, chirp("nested.wav", 48_000, 20), 48_000, 1);
    let mut nested_track = audio_track(vec![clip(1, file, 0, 400, 0)]);
    nested_track.mix.effects = vec![effect(AudioEffectKind::MultibandCompressor(
        MultibandCompressor::DEFAULT,
    ))];
    let nested = project.timelines.insert(timeline(vec![nested_track]));
    let compound = project.media_pool.insert(MediaItem {
        compound: Some(nested),
        ..project.media_pool[file].clone()
    });
    let mut varispeed = Clip::new(
        ClipId(2),
        ClipSource::Media(compound),
        0,
        200,
        150,
        Rational::one(),
        Rational::new(3, 2),
    );
    varispeed.pitch_correction = false;
    let tl = timeline(vec![
        audio_track(vec![clip(1, compound, 0, 300, 10)]),
        audio_track(vec![varispeed]),
    ]);
    let range = samples(0..400);
    let whole = mixed_whole(&project, &tl, range.clone());
    assert_eq!(rendered(&project, &tl, range), whole);
    assert!(whole.iter().any(|s| s.abs() > 0.1));
}
