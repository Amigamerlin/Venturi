use super::*;
use vv_core::Project;

fn wait_done(cache: &mut MixBufferCache) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cache.has_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "decode never finished"
        );
        std::thread::yield_now();
    }
}

#[test]
fn compound_mixdown_waits_for_its_real_media_then_caches_it() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("compound_source.wav");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
        ],
        &path,
    );

    let mut project = Project::default();
    let real_media = project.media_pool.insert(vv_core::MediaItem {
        path: path.clone(),
        meta: vv_core::MediaMeta {
            duration_frames: 25,
            fps: vv_core::Rational::new(25, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 1,
            audio_streams: 1,
            file: Default::default(),
        },
        content_hash: 1,
        compound: None,
        folder: None,
    });
    let real_clip = vv_core::Clip::from_source_range(
        vv_core::ClipId(1),
        vv_core::ClipSource::Media(real_media),
        0,
        25,
        0,
        vv_core::Rational::one(),
    );
    let nested_id = project.timelines.insert(vv_core::Timeline {
        name: "Nested".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (1, 1),
        tracks: vec![vv_core::Track {
            kind: vv_core::TrackKind::Audio,
            clips: vec![real_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
            mix: Default::default(),
            armed: Default::default(),
        }],
        markers: Vec::new(),
        master: Default::default(),
    });
    let compound_media = project.media_pool.insert(vv_core::MediaItem {
        path: "Compound Clip 1".into(),
        meta: vv_core::MediaMeta {
            duration_frames: 25,
            fps: vv_core::Rational::new(25, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 1,
            audio_streams: 1,
            file: Default::default(),
        },
        content_hash: 2,
        compound: Some(nested_id),
        folder: None,
    });

    let mut cache = MixBufferCache::spawn(48_000, 1);
    let mixdown = |cache: &mut MixBufferCache| {
        vv_audio::mixer::compound_mixdown(&project, compound_media, 48_000, 1, cache)
    };
    assert!(
        matches!(mixdown(&mut cache), vv_audio::ClipAudio::Pending),
        "the real media is not decoded yet: the compound clip is not ready"
    );

    wait_done(&mut cache);
    let vv_audio::ClipAudio::Ready(buffer) = mixdown(&mut cache) else {
        panic!("the decode is over: the mixdown must be complete");
    };
    assert!(
        buffer
            .to_vec(0..buffer.end())
            .iter()
            .any(|&s| s.abs() > 0.01),
        "the sine wave must reach the mixdown"
    );

    // Same content_hash: the cached buffer, not a new mix.
    let vv_audio::ClipAudio::Ready(cached) = mixdown(&mut cache) else {
        panic!("a complete mixdown stays ready");
    };
    assert_eq!(identity(&buffer), identity(&cached));
}

fn identity(buffer: &SampleBlocks) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    buffer.hash_identity(&mut hasher);
    hasher.finish()
}

/// The whole stream, as far as decoded.
fn whole(cache: &mut MixBufferCache, path: &Path, stream: usize) -> Option<SampleBlocks> {
    match cache.file(path, stream) {
        ClipAudio::Ready(buffer) | ClipAudio::Partial(buffer) => Some(buffer),
        ClipAudio::Pending | ClipAudio::Missing => None,
    }
}

fn sine_wav(name: &str, secs: u32) -> PathBuf {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=48000:duration={secs}"),
        ],
        &path,
    );
    path
}

const SEC: u64 = 48_000;

fn ready(cache: &mut MixBufferCache, path: &Path, frames: Range<u64>) -> SampleBlocks {
    match cache.file_frames(path, 0, frames) {
        ClipAudio::Ready(buffer) => buffer,
        _ => panic!("expected every block decoded"),
    }
}

fn blocks_held(cache: &MixBufferCache, path: &Path) -> Vec<u64> {
    let mut held: Vec<u64> = cache
        .streams
        .get(&(path.to_path_buf(), 0))
        .map(|stream| stream.blocks.keys().copied().collect())
        .unwrap_or_default();
    held.sort_unstable();
    held
}

#[test]
fn buffers_are_decoded_in_background_per_path_and_stream() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("two_streams.mkv");
    // Stream 0 mono 44.1 kHz of 1s, stream 1 stereo 48 kHz of 0.5s.
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=0.5",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-ac:1",
            "2",
            "-c:a",
            "pcm_f32le",
        ],
        &path,
    );

    let mut cache = MixBufferCache::spawn(48_000, 2);
    assert!(whole(&mut cache, &path, 0).is_none());
    assert!(whole(&mut cache, &path, 1).is_none());
    wait_done(&mut cache);

    let s0 = whole(&mut cache, &path, 0).expect("stream 0 ready");
    let s1 = whole(&mut cache, &path, 1).expect("stream 1 ready");
    assert!((s0.end() as i64 - 48_000).abs() < 100, "s0={}", s0.end());
    assert!((s1.end() as i64 - 24_000).abs() < 100, "s1={}", s1.end());
    whole(&mut cache, &path, 5);
    wait_done(&mut cache);
    assert!(
        matches!(cache.file(&path, 5), ClipAudio::Missing),
        "nonexistent stream"
    );
}

#[test]
fn a_long_track_plays_its_first_blocks_before_decoding_ends() {
    let path = sine_wav("long.wav", 20);
    let mut cache = MixBufferCache::spawn(48_000, 2);
    whole(&mut cache, &path, 0);
    let mut ends = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cache.has_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "decode never finished"
        );
        if let Some(buffer) = whole(&mut cache, &path, 0)
            && ends.last() != Some(&buffer.end())
        {
            ends.push(buffer.end());
        }
        std::thread::yield_now();
    }
    let full = whole(&mut cache, &path, 0).unwrap();
    assert_eq!(full.end(), 20 * SEC);
    assert_eq!(full.to_vec(0..full.end()).len() as u64, 20 * SEC * 2);
    assert!(
        ends.first().is_some_and(|&first| first < full.end()),
        "expected blocks before the end: {ends:?}"
    );
}

#[test]
fn a_priority_request_jumps_the_queue() {
    let paths: Vec<PathBuf> = ["queue_a.wav", "queue_b.wav", "queue_c.wav"]
        .iter()
        .map(|name| sine_wav(name, 30))
        .collect();

    let mut cache = MixBufferCache::spawn(48_000, 2);
    whole(&mut cache, &paths[0], 0);
    whole(&mut cache, &paths[1], 0);
    cache.get_or_request_first(&paths[2], 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        assert!(std::time::Instant::now() < deadline, "decode never arrived");
        cache.poll();
        if whole(&mut cache, &paths[2], 0).is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        whole(&mut cache, &paths[1], 0).is_none(),
        "the priority request should have jumped ahead of the queued one"
    );
}

#[test]
fn every_stream_of_a_file_starts_playing_before_any_of_them_is_fully_decoded() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("three_streams_long.mkv");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=120",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=48000:duration=120",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=120",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:a",
            "aac",
        ],
        &path,
    );

    // The order of the decoder's messages, not a poll loop racing it: a
    // fast decode would finish before the loop looked.
    let jobs: Vec<DecodeJob> = (0..3)
        .map(|stream| DecodeJob {
            id: stream as u64,
            path: path.clone(),
            stream,
            blocks: 0..u64::MAX,
            first: false,
            cancelled: Arc::default(),
        })
        .collect();
    let (tx, rx) = mpsc::channel();
    assert!(decode_blocks(&path, &jobs, 48_000, 2, &tx));
    drop(tx);
    let messages: Vec<Decoded> = rx.iter().collect();
    let first_done = messages
        .iter()
        .position(|m| matches!(m, Decoded::Done { .. }))
        .unwrap();
    for stream in 0..3 {
        assert!(
            messages[..first_done]
                .iter()
                .any(|m| matches!(m, Decoded::Block { job, .. } if *job == stream)),
            "stream {stream} only arrived at the end"
        );
    }
    assert!(messages.iter().all(|m| match m {
        Decoded::Block { samples, .. } => samples.len() as u64 <= BLOCK_FRAMES * 2,
        Decoded::Done { end, .. } => end.is_some_and(|end| end.abs_diff(120 * SEC) < 2048),
    }));
}

#[test]
fn a_stretch_is_computed_in_background_and_dropped_once_unused() {
    let mut cache = MixBufferCache::spawn(48_000, 1);
    let sine = SampleBlocks::contiguous(
        Arc::new(
            (0..96_000)
                .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
                .collect(),
        ),
        0,
        1,
    );
    let ask = |cache: &mut MixBufferCache| cache.stretched(&sine, 0..48_000, 2.0, 48_000, 1);
    assert!(matches!(ask(&mut cache), ClipAudio::Pending));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let stretched = loop {
        cache.poll();
        match ask(&mut cache) {
            ClipAudio::Ready(samples) => break samples,
            ClipAudio::Pending => {}
            _ => panic!("the stretch failed"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stretch never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let secs = stretched.end() as f64 / 48_000.0;
    assert!((secs - 0.5).abs() < 0.05, "1 s at 2x lasts {secs} s");

    cache.sweep_unused();
    assert!(matches!(ask(&mut cache), ClipAudio::Ready(_)), "still used");
    cache.sweep_unused();
    cache.sweep_unused();
    assert!(
        matches!(ask(&mut cache), ClipAudio::Pending),
        "dropped when unused"
    );
}

#[test]
fn only_the_blocks_read_are_decoded_with_the_same_samples_as_the_whole_file() {
    let path = sine_wav("ranged.wav", 60);
    let mut cache = MixBufferCache::spawn(48_000, 1);
    let read = 20 * SEC..25 * SEC;
    assert!(matches!(
        cache.file_frames(&path, 0, read.clone()),
        ClipAudio::Pending
    ));
    wait_done(&mut cache);

    let buffer = ready(&mut cache, &path, read.clone());
    let (first, last) = (read.start / BLOCK_FRAMES, read.end.div_ceil(BLOCK_FRAMES));
    let with_margin: Vec<u64> = (first - MARGIN_BLOCKS..last + MARGIN_BLOCKS).collect();
    assert_eq!(blocks_held(&cache, &path), with_margin);
    let whole = vv_media::decode_audio_track(&path, 0).unwrap().unwrap();
    assert_eq!(
        buffer.to_vec(read.clone()),
        whole.samples[read.start as usize..read.end as usize]
    );
}

#[test]
fn a_trim_into_the_margin_is_ready_at_once() {
    let path = sine_wav("within.wav", 40);
    let mut cache = MixBufferCache::spawn(48_000, 1);
    cache.file_frames(&path, 0, 10 * SEC..12 * SEC);
    wait_done(&mut cache);

    let trimmed = 10 * SEC - BLOCK_FRAMES / 2..12 * SEC + BLOCK_FRAMES / 2;
    ready(&mut cache, &path, trimmed);
}

#[test]
fn a_longer_read_plays_the_blocks_there_while_the_others_decode() {
    let path = sine_wav("extended.wav", 40);
    let mut cache = MixBufferCache::spawn(48_000, 1);
    cache.file_frames(&path, 0, 10 * SEC..12 * SEC);
    wait_done(&mut cache);

    let held = blocks_held(&cache, &path).len();
    assert!(matches!(
        cache.file_frames(&path, 0, 10 * SEC..30 * SEC),
        ClipAudio::Partial(_)
    ));
    wait_done(&mut cache);
    ready(&mut cache, &path, 10 * SEC..30 * SEC);
    assert!(blocks_held(&cache, &path).len() > held);
}

#[test]
fn a_pass_frees_the_blocks_a_shortened_clip_no_longer_reads() {
    let path = sine_wav("shortened.wav", 40);
    let mut cache = MixBufferCache::spawn(48_000, 1);
    cache.begin_pass();
    cache.file_frames(&path, 0, 0..40 * SEC);
    cache.sweep_unused();
    wait_done(&mut cache);

    cache.begin_pass();
    ready(&mut cache, &path, 0..5 * SEC);
    cache.sweep_unused();
    let kept = (5 * SEC).div_ceil(BLOCK_FRAMES) + MARGIN_BLOCKS;
    assert_eq!(blocks_held(&cache, &path), (0..kept).collect::<Vec<_>>());
}

#[test]
fn a_read_past_the_end_of_the_file_ends_at_it() {
    let path = sine_wav("short.wav", 10);
    let mut cache = MixBufferCache::spawn(48_000, 1);
    cache.file_frames(&path, 0, 5 * SEC..20 * SEC);
    wait_done(&mut cache);

    let buffer = ready(&mut cache, &path, 5 * SEC..20 * SEC);
    assert_eq!(buffer.end(), 10 * SEC);
}

#[test]
fn what_a_pass_does_not_read_is_dropped() {
    let path = sine_wav("dropped.wav", 10);
    let mut cache = MixBufferCache::spawn(48_000, 1);
    cache.begin_pass();
    cache.file_frames(&path, 0, 0..SEC);
    cache.store_compound(
        <MediaId as vv_core::id_map::Id>::from_raw(1),
        1,
        Arc::new(vec![0.0; 10]),
    );
    cache.sweep_unused();
    wait_done(&mut cache);
    assert!(!cache.streams.is_empty() && !cache.compound.is_empty());

    cache.begin_pass();
    cache.sweep_unused();
    assert!(cache.streams.is_empty(), "the file's audio is freed");
    assert!(cache.compound.is_empty(), "the mixdown is freed");
}
