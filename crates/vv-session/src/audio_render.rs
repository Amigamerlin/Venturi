//! A timeline's mix rendered start to end in chunks, its sources decoded by
//! seeking and freed as the render moves on: memory does not grow with the
//! length of the timeline (plans/AUDIO_LIVE_MIX.md §3.2). The levels of
//! the normalizations are rendered the same way, by segment (§3.3).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use vv_audio::loudness::{LOOKAHEAD_FRAMES, LevelMeter, LevelParts};
use vv_audio::mixer::{
    AudioSource, ClipAudio, MixSnapshot, MixState, PROJECT_SAMPLE_RATE, mix_range_from,
    remix_channels_into, stretch_range,
};
use vv_audio::{LevelAnalysis, LevelReading, LevelSegment, SampleBlocks};
use vv_core::{MediaId, Project, Timeline};

use crate::export::{ExportError, PROJECT_CHANNELS};

/// A multiple of the mixer's block, so a split render equals a whole one.
const CHUNK_FRAMES: u64 = 1 << 18;
/// How far ahead of what a reader decoded a window may start and still be
/// read from it, instead of seeking with a new one.
const READ_AHEAD_FRAMES: u64 = 10 * PROJECT_SAMPLE_RATE as u64;
/// Small, unlike the preview's: a window holds the blocks it overlaps,
/// and an edited timeline reads many short windows.
const BLOCK_FRAMES: u64 = 1 << 13;
/// Decoded packets a reader may be ahead of the render.
const READER_QUEUE: usize = 64;
/// Rendered before every segment of a measure: the loudness filters settle
/// in it, the true peak interpolates from it.
const WARM_UP_FRAMES: u64 = PROJECT_SAMPLE_RATE as u64 / 4;

/// Hands `sink` the mix of the timeline audio frames `range`, in order, at
/// `PROJECT_SAMPLE_RATE`/`PROJECT_CHANNELS`; `Break` stops the render.
pub fn render_offline(
    project: &Project,
    timeline: &Timeline,
    range: Range<u64>,
    levels: &mut LevelCache,
    sink: impl FnMut(&[f32]) -> ControlFlow<()>,
) -> Result<(), ExportError> {
    render(project, Mix::Timeline(timeline), range, levels, sink)
}

/// The level `analysis` describes, from the measures of its segments:
/// the ones in `levels` are not rendered again.
pub fn measure_level(
    analysis: &LevelAnalysis,
    levels: &mut LevelCache,
) -> Result<f32, ExportError> {
    let mut parts = Vec::with_capacity(analysis.segments.len());
    for segment in &analysis.segments {
        let part = match levels.get(segment.key) {
            Some(part) => part,
            None => {
                let part = measure_segment(analysis, segment, levels)?;
                levels.put(segment.key, &part);
                part
            }
        };
        parts.push(part);
    }
    Ok(LevelParts::merge(&parts))
}

fn measure_segment(
    analysis: &LevelAnalysis,
    segment: &LevelSegment,
    levels: &mut LevelCache,
) -> Result<LevelParts, ExportError> {
    let frames = &segment.frames;
    let warm_up = segment.preroll.max(WARM_UP_FRAMES).min(frames.start);
    let range = frames.start - warm_up..frames.end + LOOKAHEAD_FRAMES;
    let mut meter = LevelMeter::new(analysis.mode, analysis.sample_rate, analysis.channels);
    let mut after = Vec::new();
    let ch = analysis.channels.max(1) as usize;
    let mut at = range.start;
    render(
        analysis.project,
        Mix::Measured(analysis),
        range,
        levels,
        |chunk| {
            let len = (chunk.len() / ch) as u64;
            let split = |frame: u64| ((frame.clamp(at, at + len) - at) as usize) * ch;
            let (start, end) = (split(frames.start), split(frames.end));
            meter.warm_up(&chunk[..start]);
            meter.push(&chunk[start..end]);
            after.extend_from_slice(&chunk[end..]);
            at += len;
            ControlFlow::Continue(())
        },
    )?;
    Ok(meter.into_parts(&after))
}

/// What a render mixes.
#[derive(Clone, Copy)]
enum Mix<'a> {
    Timeline(&'a Timeline),
    /// A normalization's input.
    Measured(&'a LevelAnalysis<'a>),
}

impl Mix<'_> {
    fn collect(
        self,
        project: &Project,
        source: &mut impl AudioSource,
        range: Range<u64>,
    ) -> MixSnapshot {
        let (rate, channels) = (PROJECT_SAMPLE_RATE, PROJECT_CHANNELS);
        match self {
            Mix::Timeline(timeline) => {
                MixSnapshot::from_timeline_range(project, timeline, rate, channels, source, range)
            }
            Mix::Measured(analysis) => analysis.snapshot(source, Some(range)),
        }
    }
}

fn render(
    project: &Project,
    mix: Mix,
    range: Range<u64>,
    levels: &mut LevelCache,
    mut sink: impl FnMut(&[f32]) -> ControlFlow<()>,
) -> Result<(), ExportError> {
    let ch = PROJECT_CHANNELS as usize;
    let mut audio = StreamingAudio::new(levels);
    let mut state: Option<MixState> = None;
    let mut out = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let chunk = at..(at + CHUNK_FRAMES).min(range.end);
        let last = chunk.end == range.end;
        let snapshot = audio.snapshot(project, mix, chunk.clone(), last)?;
        let mut next = MixState::new(&snapshot);
        if let Some(previous) = &state {
            next.continue_from(previous);
        }
        let state = state.insert(next);
        out.resize((chunk.end - chunk.start) as usize * ch, 0.0);
        mix_range_from(&snapshot, state, chunk.start, &mut out);
        if sink(&out).is_break() {
            return Ok(());
        }
        at = chunk.end;
    }
    Ok(())
}

/// Measured segments by key, kept in memory and in
/// `$XDG_CACHE_HOME/venturi/levels`.
#[derive(Default)]
pub struct LevelCache {
    parts: HashMap<u64, LevelParts>,
    dir: Option<PathBuf>,
    /// Segments rendered, not found.
    pub rendered: usize,
}

impl LevelCache {
    pub fn on_disk() -> Self {
        Self {
            dir: Some(vv_media::cache_dir("levels")),
            ..Self::default()
        }
    }

    fn path(&self, key: u64) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{key:016x}.level")))
    }

    fn get(&mut self, key: u64) -> Option<LevelParts> {
        if let Some(part) = self.parts.get(&key) {
            return Some(part.clone());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(self.path(key)?)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .ok()?;
        let part = decode_parts(&bytes)?;
        self.parts.insert(key, part.clone());
        Some(part)
    }

    fn put(&mut self, key: u64, part: &LevelParts) {
        self.rendered += 1;
        self.parts.insert(key, part.clone());
        let Some(path) = self.path(key) else {
            return;
        };
        // Best effort: without it the level is measured again next time.
        let written = std::fs::create_dir_all(path.parent().unwrap_or(&path)).and_then(|()| {
            let tmp = path.with_extension("tmp");
            std::fs::File::create(&tmp)?.write_all(&encode_parts(part))?;
            std::fs::rename(&tmp, &path)
        });
        if let Err(e) = written {
            eprintln!("[levels] {}: {e}", path.display());
        }
    }
}

const PARTS_MAGIC: &[u8; 4] = b"VVLV";
const PARTS_VERSION: u32 = 1;

fn encode_parts(part: &LevelParts) -> Vec<u8> {
    let mut bytes = PARTS_MAGIC.to_vec();
    bytes.extend_from_slice(&PARTS_VERSION.to_le_bytes());
    match part {
        LevelParts::Peak(peak) => {
            bytes.push(0);
            bytes.extend_from_slice(&peak.to_le_bytes());
        }
        LevelParts::Loudness {
            segment_frames,
            segments,
            tail,
        } => {
            bytes.push(1);
            bytes.extend_from_slice(&(*segment_frames as u64).to_le_bytes());
            bytes.extend_from_slice(&(segments.len() as u64).to_le_bytes());
            for energy in segments {
                bytes.extend_from_slice(&energy.to_le_bytes());
            }
            bytes.extend_from_slice(&tail.0.to_le_bytes());
            bytes.extend_from_slice(&(tail.1 as u64).to_le_bytes());
        }
    }
    bytes
}

/// `None` for anything but a file of this version, whole.
fn decode_parts(bytes: &[u8]) -> Option<LevelParts> {
    let rest = bytes.strip_prefix(PARTS_MAGIC)?;
    let (version, rest) = rest.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != PARTS_VERSION {
        return None;
    }
    let (&kind, mut rest) = rest.split_first()?;
    let mut u64_at = || {
        let (value, tail) = rest.split_first_chunk::<8>()?;
        rest = tail;
        Some(*value)
    };
    match kind {
        0 => {
            let peak = bytes.get(9..13)?.try_into().ok()?;
            (bytes.len() == 13).then(|| LevelParts::Peak(f32::from_le_bytes(peak)))
        }
        1 => {
            let segment_frames = u64::from_le_bytes(u64_at()?) as usize;
            let count = u64::from_le_bytes(u64_at()?);
            let segments = (0..count)
                .map(|_| u64_at().map(f64::from_le_bytes))
                .collect::<Option<Vec<f64>>>()?;
            let energy = f64::from_le_bytes(u64_at()?);
            let frames = u64::from_le_bytes(u64_at()?) as usize;
            u64_at().is_none().then_some(LevelParts::Loudness {
                segment_frames,
                segments,
                tail: (energy, frames),
            })
        }
        _ => None,
    }
}

type WindowKey = (PathBuf, usize, Range<u64>);

/// The sources of the chunk being rendered, and what the next one may
/// read again.
struct StreamingAudio<'c> {
    readers: Vec<Reader>,
    /// Audio streams of each file.
    streams: HashMap<PathBuf, usize>,
    /// The windows of the current chunk; one asked again keeps its buffer,
    /// so what was built from it (a stretch) is found again.
    windows: HashMap<WindowKey, SampleBlocks>,
    /// Mixdowns of the pitch-corrected compound clips (until plan step 4),
    /// by media and content.
    compounds: HashMap<(MediaId, u64), Arc<Vec<f32>>>,
    stretched: HashMap<u64, SampleBlocks>,
    /// By `LevelAnalysis::key`: a normalization is asked again by every
    /// chunk.
    levels: HashMap<u64, f32>,
    cache: &'c mut LevelCache,
}

impl<'c> StreamingAudio<'c> {
    fn new(cache: &'c mut LevelCache) -> Self {
        Self {
            readers: Vec::new(),
            streams: HashMap::new(),
            windows: HashMap::new(),
            compounds: HashMap::new(),
            stretched: HashMap::new(),
            levels: HashMap::new(),
            cache,
        }
    }

    /// Twice through the timeline: once to learn the windows the chunk
    /// reads, once with them decoded. `last`: no chunk follows, so nothing
    /// is kept for one.
    fn snapshot(
        &mut self,
        project: &Project,
        mix: Mix,
        range: Range<u64>,
        last: bool,
    ) -> Result<MixSnapshot, ExportError> {
        let mut wanted = Wanted {
            windows: HashSet::new(),
            compounds: &self.compounds,
        };
        mix.collect(project, &mut wanted, range.clone());
        let windows = wanted.windows;
        self.load(windows, last)?;
        let mut provider = Provider {
            audio: self,
            compounds_used: HashSet::new(),
            stretched_used: HashSet::new(),
            levels_used: HashSet::new(),
        };
        let snapshot = mix.collect(project, &mut provider, range);
        let Provider {
            compounds_used,
            stretched_used,
            levels_used,
            ..
        } = provider;
        self.compounds.retain(|key, _| compounds_used.contains(key));
        self.stretched.retain(|key, _| stretched_used.contains(key));
        self.levels.retain(|key, _| levels_used.contains(key));
        Ok(snapshot)
    }

    /// Decodes the windows not kept from the previous chunk, in file order
    /// so that the readers only move forward. `last`: each file's readers
    /// close once its windows are read.
    fn load(&mut self, wanted: HashSet<WindowKey>, last: bool) -> Result<(), ExportError> {
        let mut previous = std::mem::take(&mut self.windows);
        let mut to_read = Vec::new();
        for key in wanted {
            match previous.remove_entry(&key) {
                Some((key, samples)) => {
                    self.windows.insert(key, samples);
                }
                None => to_read.push(key),
            }
        }
        drop(previous);
        to_read.sort_by(|a, b| (&a.0, a.2.start).cmp(&(&b.0, b.2.start)));
        for reader in &mut self.readers {
            reader.used = false;
        }
        for (i, (path, stream, frames)) in to_read.iter().enumerate() {
            if last && i > 0 && to_read[i - 1].0 != *path {
                self.readers.retain(|r| r.path != to_read[i - 1].0);
            }
            // A stream the file does not have is `Missing`.
            if *stream >= self.stream_count(path)? {
                continue;
            }
            let reader = match self
                .readers
                .iter()
                .position(|r| r.serves(path, *stream, frames.start))
            {
                Some(reader) => reader,
                None => {
                    let mut streams: Vec<usize> = to_read[i..]
                        .iter()
                        .take_while(|(p, _, f)| {
                            p == path && f.start <= frames.start + READ_AHEAD_FRAMES
                        })
                        .map(|(_, s, _)| *s)
                        .filter(|s| *s < self.streams[path])
                        .collect();
                    streams.sort_unstable();
                    streams.dedup();
                    self.readers
                        .push(Reader::open(path.clone(), streams, frames.start));
                    self.readers.len() - 1
                }
            };
            let window = self.readers[reader].read(*stream, frames.clone())?;
            self.windows
                .insert((path.clone(), *stream, frames.clone()), window);
        }
        self.readers.retain(|reader| reader.used && !last);
        for reader in &mut self.readers {
            reader.trim();
        }
        Ok(())
    }

    fn stream_count(&mut self, path: &Path) -> Result<usize, ExportError> {
        if let Some(count) = self.streams.get(path) {
            return Ok(*count);
        }
        let count = vv_media::probe::audio_streams(path)
            .map_err(ExportError::failed)?
            .len();
        self.streams.insert(path.to_path_buf(), count);
        Ok(count)
    }
}

/// The windows a mix reads, compound clips included, without providing any.
struct Wanted<'a> {
    windows: HashSet<WindowKey>,
    /// A compound clip already mixed needs none of its sources.
    compounds: &'a HashMap<(MediaId, u64), Arc<Vec<f32>>>,
}

impl AudioSource for Wanted<'_> {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self.file_frames(path, stream, 0..u64::MAX, 0)
    }

    fn file_frames(&mut self, path: &Path, stream: usize, frames: Range<u64>, _: u64) -> ClipAudio {
        self.windows.insert((path.to_path_buf(), stream, frames));
        ClipAudio::Pending
    }

    fn cached_compound(&mut self, media_id: MediaId, content_hash: u64) -> Option<Arc<Vec<f32>>> {
        self.compounds.get(&(media_id, content_hash)).cloned()
    }

    /// Measured apart, not from these windows.
    fn level(&mut self, _: LevelAnalysis) -> LevelReading {
        LevelReading::Pending(None)
    }
}

struct Provider<'a, 'c> {
    audio: &'a mut StreamingAudio<'c>,
    compounds_used: HashSet<(MediaId, u64)>,
    stretched_used: HashSet<u64>,
    levels_used: HashSet<u64>,
}

impl AudioSource for Provider<'_, '_> {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self.file_frames(path, stream, 0..u64::MAX, 0)
    }

    fn file_frames(&mut self, path: &Path, stream: usize, frames: Range<u64>, _: u64) -> ClipAudio {
        self.audio
            .windows
            .get(&(path.to_path_buf(), stream, frames))
            .map_or(ClipAudio::Missing, |window| {
                ClipAudio::Ready(window.clone())
            })
    }

    fn cached_compound(&mut self, media_id: MediaId, content_hash: u64) -> Option<Arc<Vec<f32>>> {
        let key = (media_id, content_hash);
        let mixdown = self.audio.compounds.get(&key).cloned()?;
        self.compounds_used.insert(key);
        Some(mixdown)
    }

    fn store_compound(&mut self, media_id: MediaId, content_hash: u64, mixdown: Arc<Vec<f32>>) {
        let key = (media_id, content_hash);
        self.audio.compounds.insert(key, mixdown);
        self.compounds_used.insert(key);
    }

    fn stretched(
        &mut self,
        buffer: &SampleBlocks,
        range: Range<u64>,
        tempo: f64,
        sample_rate: u32,
        channels: u16,
    ) -> ClipAudio {
        let mut h = DefaultHasher::new();
        buffer.hash_identity(&mut h);
        (range.start, range.end, tempo.to_bits()).hash(&mut h);
        let key = h.finish();
        self.stretched_used.insert(key);
        if let Some(stretched) = self.audio.stretched.get(&key) {
            return ClipAudio::Ready(stretched.clone());
        }
        let Ok(samples) = stretch_range(buffer, range, tempo, sample_rate, channels) else {
            return ClipAudio::Missing;
        };
        let stretched = SampleBlocks::contiguous(Arc::new(samples), 0, channels);
        self.audio.stretched.insert(key, stretched.clone());
        ClipAudio::Ready(stretched)
    }

    fn level(&mut self, analysis: LevelAnalysis) -> LevelReading {
        let key = analysis.key();
        self.levels_used.insert(key);
        if let Some(level) = self.audio.levels.get(&key) {
            return LevelReading::Ready(*level);
        }
        // Unity gain rather than a failed render.
        let level = measure_level(&analysis, self.audio.cache).unwrap_or_else(|e| {
            eprintln!("[levels] {e}");
            0.0
        });
        self.audio.levels.insert(key, level);
        LevelReading::Ready(level)
    }
}

enum Decoded {
    Samples {
        slot: usize,
        at: u64,
        samples: Vec<f32>,
    },
    End(Result<(), String>),
}

/// One decode of some streams of a file, from a position on, on its own
/// thread: blocked on a bounded channel, it decodes only as far as the
/// render reads.
struct Reader {
    path: PathBuf,
    streams: Vec<usize>,
    decoded: Receiver<Decoded>,
    kept: Vec<KeptStream>,
    ended: bool,
    /// Per stream, the first frame read during the current chunk.
    read_from: Vec<Option<u64>>,
    used: bool,
}

/// Decoded frames not yet dropped, in blocks on the `BLOCK_FRAMES` grid
/// that the windows share instead of copying.
struct KeptStream {
    /// Grid index of `blocks[0]`, and of `filling` after the last one.
    first: u64,
    blocks: VecDeque<Arc<Vec<f32>>>,
    filling: Vec<f32>,
}

impl KeptStream {
    fn start(&self) -> u64 {
        self.first * BLOCK_FRAMES
    }

    fn end(&self) -> u64 {
        (self.first + self.blocks.len() as u64) * BLOCK_FRAMES
            + (self.filling.len() / PROJECT_CHANNELS as usize) as u64
    }

    /// `None`: silence.
    fn push(&mut self, mut frames: u64, mut samples: Option<&[f32]>) {
        let ch = PROJECT_CHANNELS as u64;
        while frames > 0 {
            let room = BLOCK_FRAMES - self.filling.len() as u64 / ch;
            let count = frames.min(room);
            let len = (count * ch) as usize;
            match &mut samples {
                Some(s) => {
                    self.filling.extend_from_slice(&s[..len]);
                    *s = &s[len..];
                }
                None => self.filling.resize(self.filling.len() + len, 0.0),
            }
            frames -= count;
            if count == room {
                let block = std::mem::replace(&mut self.filling, new_block());
                self.blocks.push_back(Arc::new(block));
            }
        }
    }

    /// The blocks holding `frames`, up to `end` if it comes first.
    fn view(&self, frames: Range<u64>) -> SampleBlocks {
        let end = frames.end.min(self.end());
        let first = (frames.start / BLOCK_FRAMES).max(self.first);
        let last = end.div_ceil(BLOCK_FRAMES).max(first);
        let full = self.first + self.blocks.len() as u64;
        let blocks = (first..last)
            .map(
                |index| match self.blocks.get((index - self.first) as usize) {
                    Some(block) => Some(block.clone()),
                    // Still filling: copied, at most one block.
                    None => (index == full).then(|| Arc::new(self.filling.clone())),
                },
            )
            .collect();
        let start = first * BLOCK_FRAMES;
        SampleBlocks::new(
            start,
            BLOCK_FRAMES,
            PROJECT_CHANNELS,
            blocks,
            end.max(start),
        )
    }

    fn drop_before(&mut self, frame: u64) {
        while self.first < frame / BLOCK_FRAMES && !self.blocks.is_empty() {
            self.blocks.pop_front();
            self.first += 1;
        }
    }
}

impl Reader {
    /// From the start of the block `from` is in: the blocks of every reader
    /// of the file are on the same grid.
    fn open(path: PathBuf, streams: Vec<usize>, from: u64) -> Self {
        let first = from / BLOCK_FRAMES;
        let (tx, decoded) = sync_channel(READER_QUEUE);
        let (thread_path, thread_streams) = (path.clone(), streams.clone());
        std::thread::spawn(move || {
            decode_into(&thread_path, &thread_streams, first * BLOCK_FRAMES, &tx)
        });
        let n = streams.len();
        Self {
            path,
            streams,
            decoded,
            kept: (0..n)
                .map(|_| KeptStream {
                    first,
                    blocks: VecDeque::new(),
                    filling: new_block(),
                })
                .collect(),
            ended: false,
            read_from: vec![None; n],
            used: false,
        }
    }

    fn serves(&self, path: &Path, stream: usize, start: u64) -> bool {
        self.path == path
            && self
                .streams
                .iter()
                .position(|s| *s == stream)
                .is_some_and(|slot| {
                    let kept = &self.kept[slot];
                    kept.start() <= start && start <= kept.end() + READ_AHEAD_FRAMES
                })
    }

    /// Up to the end of the file if it comes first.
    fn read(&mut self, stream: usize, frames: Range<u64>) -> Result<SampleBlocks, ExportError> {
        let slot = self.streams.iter().position(|s| *s == stream).unwrap();
        while self.kept[slot].end() < frames.end && !self.ended {
            match self.decoded.recv() {
                Ok(Decoded::Samples { slot, at, samples }) => self.append(slot, at, &samples),
                Ok(Decoded::End(Err(e))) => return Err(ExportError::Failed(e)),
                Ok(Decoded::End(Ok(()))) | Err(_) => self.ended = true,
            }
        }
        self.used = true;
        let first = self.read_from[slot].get_or_insert(frames.start);
        *first = (*first).min(frames.start);
        Ok(self.kept[slot].view(frames))
    }

    /// A gap in the stream plays as silence, as in the preview.
    fn append(&mut self, slot: usize, at: u64, samples: &[f32]) {
        let ch = PROJECT_CHANNELS as u64;
        let kept = &mut self.kept[slot];
        let end = kept.end();
        let to = at + samples.len() as u64 / ch;
        if to <= end {
            return;
        }
        if at > end {
            kept.push(at - end, None);
        }
        let skip = end.saturating_sub(at);
        kept.push(to - at - skip, Some(&samples[(skip * ch) as usize..]));
    }

    /// Drops the blocks before the first frame read in this chunk: the
    /// next one reads further on.
    fn trim(&mut self) {
        for (kept, read_from) in self.kept.iter_mut().zip(&mut self.read_from) {
            kept.drop_before(read_from.take().unwrap_or(u64::MAX));
        }
    }
}

fn new_block() -> Vec<f32> {
    Vec::with_capacity((BLOCK_FRAMES * PROJECT_CHANNELS as u64) as usize)
}

fn decode_into(path: &Path, streams: &[usize], from: u64, tx: &SyncSender<Decoded>) {
    let result = vv_media::decode_audio_streams_from(
        path,
        streams,
        PROJECT_SAMPLE_RATE,
        from,
        |slot, channels, at, chunk| {
            let mut samples = Vec::new();
            remix_channels_into(chunk, channels, PROJECT_CHANNELS, &mut samples);
            match tx.send(Decoded::Samples { slot, at, samples }) {
                Ok(()) => ControlFlow::Continue(()),
                // The render is done with this reader.
                Err(_) => ControlFlow::Break(()),
            }
        },
    );
    let _ = tx.send(Decoded::End(result.map(drop).map_err(|e| e.to_string())));
}

#[cfg(test)]
#[path = "tests/audio_render.rs"]
mod tests;
