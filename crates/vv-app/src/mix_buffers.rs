//! Audio buffers already in the mixer's format, per `(path, audio_stream_index)`,
//! in blocks of `BLOCK_FRAMES` holding only what the timeline reads, within
//! a memory budget that keeps the blocks closest to the playhead. Decode +
//! resample on a dedicated thread, seeking to each job and taking the one
//! closest to the playhead first; every block plays as soon as it is decoded.

use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vv_audio::{
    AnalysisSlot, AudioSource, BLOCK_FRAMES, ClipAudio, CompoundBlock, CompoundMix, LevelAnalysis,
    LevelReading, SampleBlocks,
};
use vv_core::MediaId;

use crate::worker::Worker;

type Key = (PathBuf, usize);

/// Identity of the source blocks over the range, range, tempo bits.
type StretchKey = (u64, u64, u64, u64);

struct StretchJob {
    key: StretchKey,
    source: SampleBlocks,
    range: Range<u64>,
    tempo: f64,
    sample_rate: u32,
    channels: u16,
}

enum Stretch {
    Pending,
    Ready(SampleBlocks),
    Failed,
}

struct LevelJob {
    key: u64,
    analysis: LevelAnalysis,
}

/// `None`: dropped for a newer job of the same slot.
type LevelResult = (u64, Option<AnalysisSlot>, Option<f32>);

/// Blocks kept on each side of what the clips read: a trim into them
/// decodes nothing.
const MARGIN_BLOCKS: u64 = 1;

/// Blocks per decode job: how far a decode goes before the worker looks
/// again for the job closest to the playhead.
const JOB_BLOCKS: u64 = 8;

/// New blocks rebuild the mix at most this often: a rebuild walks every
/// block of the compound clips.
const BLOCK_REBUILD_INTERVAL: Duration = Duration::from_millis(250);

/// Over budget, a playhead move this long chooses again what to keep.
const REBALANCE_FRAMES: u64 = 2 * vv_audio::mixer::PROJECT_SAMPLE_RATE as u64;

pub const DEFAULT_AUDIO_BUDGET_BYTES: usize = 1_000_000_000;

#[derive(Default)]
struct StreamCache {
    blocks: HashMap<u64, Arc<Vec<f32>>>,
    jobs: Vec<Job>,
    /// Frames of the stream, once a decode reached its end.
    end: Option<u64>,
    /// The stream ends before this: a decode seeked here found nothing.
    ends_before: Option<u64>,
    /// No such stream, or it does not decode.
    missing: bool,
    /// Read since the pass began.
    asked: bool,
    /// Blocks read since the pass began, with when they play.
    wanted: HashMap<u64, u64>,
}

impl StreamCache {
    fn in_flight(&self, block: u64) -> bool {
        self.jobs.iter().any(|job| job.blocks.contains(&block))
    }
}

/// The blocks of a compound clip's mix (`AudioSource::mixes_compounds_by_block`).
#[derive(Default)]
struct CompoundCache {
    content_hash: u64,
    blocks: HashMap<u64, Arc<Vec<f32>>>,
    mixing: HashSet<u64>,
    asked: bool,
    wanted: HashMap<u64, u64>,
}

/// What `rebalance` weighs: the blocks of a stream, or of a compound's mix.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Owner {
    File(Key),
    Compound(MediaId),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum OwnerRef<'a> {
    File(&'a Key),
    Compound(MediaId),
}

impl OwnerRef<'_> {
    fn to_owned(self) -> Owner {
        match self {
            OwnerRef::File(key) => Owner::File(key.clone()),
            OwnerRef::Compound(media_id) => Owner::Compound(media_id),
        }
    }
}

/// A compound block mixed: media, `content_hash`, index, samples.
type Mixed = (MediaId, u64, u64, Arc<Vec<f32>>);

struct Job {
    id: u64,
    blocks: Range<u64>,
    cancelled: Arc<AtomicBool>,
}

struct DecodeJob {
    id: u64,
    path: PathBuf,
    stream: usize,
    blocks: Range<u64>,
    /// When its first block plays, for the order of the jobs.
    plays_at: u64,
    cancelled: Arc<AtomicBool>,
}

enum Decoded {
    Block {
        job: u64,
        index: u64,
        samples: Arc<Vec<f32>>,
    },
    Done {
        job: u64,
        /// Frames of the stream, if the decode reached its end.
        end: Option<u64>,
        /// Where it seeked, if it reached the end with nothing decoded.
        ends_before: Option<u64>,
        missing: bool,
    },
}

pub struct MixBufferCache {
    channels: u16,
    budget_bytes: usize,
    streams: HashMap<Key, StreamCache>,
    compounds: HashMap<MediaId, CompoundCache>,
    mix_worker: Worker<(MediaId, u64, CompoundMix)>,
    mixed_rx: mpsc::Receiver<Mixed>,
    job_keys: HashMap<u64, Key>,
    next_id: u64,
    /// Where the mix plays, in samples of what is mixed (a timeline, or a
    /// media in the preview): the worker reads it to pick a job.
    playhead: Arc<AtomicU64>,
    /// Between `begin_*pass` and `sweep_unused`, which plans what was asked.
    in_pass: bool,
    /// Blocks asked for outside a pass, or a playhead move over budget:
    /// `poll` runs `rebalance`.
    unplanned: bool,
    /// The last `rebalance` left blocks out, and where the playhead was.
    over_budget: bool,
    rebalanced_at: u64,
    /// A media preview is mixed, not a timeline: its pass keeps the
    /// timeline's mixdowns, stretches and levels.
    media_pass: bool,
    worker: Worker<DecodeJob>,
    decoded_rx: mpsc::Receiver<Decoded>,
    /// Blocks arrived and not yet reported by `poll`, and when it last did.
    unreported_blocks: bool,
    last_block_report: Option<Instant>,
    /// Whole mixdowns of the compound clips that normalize, with the
    /// `content_hash` they were computed with.
    compound: HashMap<MediaId, (u64, Arc<Vec<f32>>)>,
    compound_used: HashSet<MediaId>,
    /// Pitch-corrected clip audio (see `AudioSource::stretched`), with the
    /// source blocks it was made from: they keep its key unique.
    stretched: HashMap<StretchKey, (SampleBlocks, Stretch)>,
    /// Keys asked for since the last `sweep_unused`.
    stretch_used: HashSet<StretchKey>,
    stretch_worker: Worker<StretchJob>,
    stretch_rx: mpsc::Receiver<(StretchKey, Result<Vec<f32>, String>)>,
    /// Normalization levels by `LevelAnalysis::key`; `None` while measuring.
    levels: HashMap<u64, Option<f32>>,
    last_level: HashMap<AnalysisSlot, f32>,
    level_used: HashSet<u64>,
    level_worker: Worker<LevelJob>,
    level_rx: mpsc::Receiver<LevelResult>,
}

impl MixBufferCache {
    pub fn spawn(sample_rate: u32, channels: u16) -> Self {
        let (decoded_tx, decoded_rx) = mpsc::channel::<Decoded>();
        let playhead = Arc::new(AtomicU64::new(0));
        let worker_playhead = playhead.clone();
        let worker = Worker::spawn(move |job_rx: mpsc::Receiver<DecodeJob>| {
            let mut queue: Vec<DecodeJob> = Vec::new();
            let mut from_start: HashMap<PathBuf, bool> = HashMap::new();
            let mut reads_from_start = |path: &Path| {
                *from_start.entry(path.to_path_buf()).or_insert_with(|| {
                    vv_media::audio_streams(path).is_ok_and(|streams| {
                        streams
                            .iter()
                            .any(|stream| stream.sample_rate != sample_rate)
                    })
                })
            };
            loop {
                if queue.is_empty() {
                    match job_rx.recv() {
                        Ok(job) => queue.push(job),
                        Err(_) => break,
                    }
                }
                queue.extend(job_rx.try_iter());
                queue.retain(|job| !job.cancelled.load(Ordering::Relaxed));
                let playhead = worker_playhead.load(Ordering::Relaxed);
                let batch = take_batch(&mut queue, playhead, &mut reads_from_start);
                let Some(path) = batch.first().map(|job| job.path.clone()) else {
                    continue;
                };
                if !decode_blocks(&path, &batch, sample_rate, channels, &decoded_tx) {
                    break;
                }
            }
        });
        let (mixed_tx, mixed_rx) = mpsc::channel::<Mixed>();
        let mix_worker = Worker::spawn(move |jobs: mpsc::Receiver<(MediaId, u64, CompoundMix)>| {
            for (media_id, content_hash, mix) in jobs {
                let block = Arc::new(mix.run());
                if mixed_tx
                    .send((media_id, content_hash, mix.index, block))
                    .is_err()
                {
                    break;
                }
            }
        });
        let (stretch_tx, stretch_rx) = mpsc::channel();
        let stretch_worker = Worker::spawn(move |jobs: mpsc::Receiver<StretchJob>| {
            for job in jobs {
                let result = vv_audio::mixer::stretch_range(
                    &job.source,
                    job.range,
                    job.tempo,
                    job.sample_rate,
                    job.channels,
                );
                if stretch_tx.send((job.key, result)).is_err() {
                    break;
                }
            }
        });
        let (level_tx, level_rx) = mpsc::channel::<LevelResult>();
        let level_worker = Worker::spawn(move |jobs: mpsc::Receiver<LevelJob>| {
            while let Ok(first) = jobs.recv() {
                let mut batch = vec![first];
                // A drag queues one job per frame: only the newest of each
                // slot is worth measuring.
                while let Ok(job) = jobs.try_recv() {
                    let slot = job.analysis.slot;
                    let older =
                        slot.and_then(|s| batch.iter().position(|j| j.analysis.slot == Some(s)));
                    if let Some(older) = older {
                        let dropped = std::mem::replace(&mut batch[older], job);
                        if level_tx.send((dropped.key, slot, None)).is_err() {
                            return;
                        }
                    } else {
                        batch.push(job);
                    }
                }
                for job in batch {
                    let level = job.analysis.run();
                    if level_tx
                        .send((job.key, job.analysis.slot, Some(level)))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
        Self {
            channels,
            budget_bytes: DEFAULT_AUDIO_BUDGET_BYTES,
            streams: HashMap::new(),
            compounds: HashMap::new(),
            mix_worker,
            mixed_rx,
            job_keys: HashMap::new(),
            next_id: 0,
            playhead,
            in_pass: false,
            unplanned: false,
            over_budget: false,
            rebalanced_at: 0,
            media_pass: false,
            worker,
            decoded_rx,
            unreported_blocks: false,
            last_block_report: None,
            compound: HashMap::new(),
            compound_used: HashSet::new(),
            stretched: HashMap::new(),
            stretch_used: HashSet::new(),
            stretch_worker,
            stretch_rx,
            levels: HashMap::new(),
            last_level: HashMap::new(),
            level_used: HashSet::new(),
            level_worker,
            level_rx,
        }
    }

    pub fn set_budget_bytes(&mut self, bytes: usize) {
        if bytes != self.budget_bytes {
            self.budget_bytes = bytes;
            self.unplanned = true;
        }
    }

    /// The mix reads more than the budget holds: only the blocks closest to
    /// the playhead are kept, and what measures a whole timeline cannot.
    pub fn over_budget(&self) -> bool {
        self.over_budget
    }

    /// Where the mix plays now, in samples of what is mixed.
    pub fn set_playhead(&mut self, sample: u64) {
        self.playhead.store(sample, Ordering::Relaxed);
        if self.over_budget && sample.abs_diff(self.rebalanced_at) > REBALANCE_FRAMES {
            self.unplanned = true;
        }
    }

    /// From here to `sweep_unused`, a full mix of the timeline: what it
    /// does not read is dropped.
    pub fn begin_pass(&mut self) {
        self.begin(false);
    }

    /// Like `begin_pass`, for the preview of a media: the timeline's
    /// mixdowns, stretches and levels are kept for when it plays again.
    pub fn begin_media_pass(&mut self) {
        self.begin(true);
    }

    fn begin(&mut self, media_pass: bool) {
        self.in_pass = true;
        self.media_pass = media_pass;
        for stream in self.streams.values_mut() {
            stream.asked = false;
            stream.wanted.clear();
        }
        if !media_pass {
            self.compound_used.clear();
            for compound in self.compounds.values_mut() {
                compound.asked = false;
                compound.wanted.clear();
            }
        }
    }

    /// Drops the blocks, mixdowns, stretches and levels the pass did not
    /// ask for, cancels the decodes no longer wanted and plans the others.
    pub fn sweep_unused(&mut self) {
        self.in_pass = false;
        let job_keys = &mut self.job_keys;
        self.streams.retain(|_, stream| {
            let StreamCache {
                blocks,
                jobs,
                asked,
                wanted,
                ..
            } = stream;
            blocks.retain(|index, _| wanted.contains_key(index));
            jobs.retain(|job| {
                let keep = job.blocks.clone().any(|index| wanted.contains_key(&index));
                if !keep {
                    job.cancelled.store(true, Ordering::Relaxed);
                    job_keys.remove(&job.id);
                }
                keep
            });
            *asked
        });
        if !self.media_pass {
            self.compounds.retain(|_, compound| {
                let wanted = &compound.wanted;
                compound
                    .blocks
                    .retain(|index, _| wanted.contains_key(index));
                compound.asked
            });
        }
        self.rebalance();
        if self.media_pass {
            return;
        }
        let used = &self.compound_used;
        self.compound.retain(|media_id, _| used.contains(media_id));
        let used = std::mem::take(&mut self.stretch_used);
        self.stretched.retain(|key, _| used.contains(key));
        let used = std::mem::take(&mut self.level_used);
        self.levels.retain(|key, _| used.contains(key));
    }

    /// Frames `frames` of `stream` of `path`, as far as decoded; for the
    /// preview of a media, so they play at their own position.
    pub fn media_frames(
        &mut self,
        path: &Path,
        stream: usize,
        frames: Range<u64>,
    ) -> Option<SampleBlocks> {
        let plays_at = frames.start;
        match self.lookup(path, stream, frames, plays_at) {
            ClipAudio::Ready(buffer) | ClipAudio::Partial(buffer) => Some(buffer),
            ClipAudio::Pending | ClipAudio::Missing => None,
        }
    }

    /// Non-blocking: the blocks holding `needed`, `Partial` while some are
    /// not decoded. Asks for them, with `MARGIN_BLOCKS` around:
    /// `rebalance` decides which to decode and keep.
    fn lookup(
        &mut self,
        path: &Path,
        stream: usize,
        needed: Range<u64>,
        plays_at: u64,
    ) -> ClipAudio {
        let playhead = self.playhead.load(Ordering::Relaxed);
        let entry = self
            .streams
            .entry((path.to_path_buf(), stream))
            .or_default();
        entry.asked = true;
        if entry.missing {
            return ClipAudio::Missing;
        }
        let first_block = needed.start / BLOCK_FRAMES;
        let limit = entry.end.or(entry.ends_before).unwrap_or(u64::MAX);
        let last_block = needed
            .end
            .min(limit)
            .div_ceil(BLOCK_FRAMES)
            .max(first_block);
        let blocks_end = limit.div_ceil(BLOCK_FRAMES);
        let wanted = first_block.saturating_sub(MARGIN_BLOCKS)
            ..(last_block + MARGIN_BLOCKS).min(blocks_end).max(last_block);
        for index in wanted {
            let plays = (plays_at + index * BLOCK_FRAMES).saturating_sub(needed.start);
            let closest = entry
                .wanted
                .get(&index)
                .is_none_or(|&other| plays.abs_diff(playhead) < other.abs_diff(playhead));
            if closest {
                entry.wanted.insert(index, plays);
            }
            if !self.in_pass && !entry.blocks.contains_key(&index) && !entry.in_flight(index) {
                self.unplanned = true;
            }
        }

        let blocks: Vec<Option<Arc<Vec<f32>>>> = (first_block..last_block)
            .map(|index| entry.blocks.get(&index).cloned())
            .collect();
        let present = blocks.iter().filter(|b| b.is_some()).count();
        let view_end = entry.end.unwrap_or(u64::MAX).min(last_block * BLOCK_FRAMES);
        let complete = present == blocks.len();
        let view = SampleBlocks::new(
            first_block * BLOCK_FRAMES,
            BLOCK_FRAMES,
            self.channels,
            blocks,
            view_end,
        );
        if complete {
            ClipAudio::Ready(view)
        } else if present > 0 {
            ClipAudio::Partial(view)
        } else {
            ClipAudio::Pending
        }
    }

    /// Keeps the wanted blocks closest to the playhead that fit the
    /// budget: drops the others, plans the missing ones, cancels the jobs
    /// left without one. `true` if it dropped blocks.
    fn rebalance(&mut self) -> bool {
        self.unplanned = false;
        let playhead = self.playhead.load(Ordering::Relaxed);
        self.rebalanced_at = playhead;
        let files = self.streams.iter().flat_map(|(key, stream)| {
            stream
                .wanted
                .iter()
                .map(move |(&index, &plays)| (plays, OwnerRef::File(key), index))
        });
        let compounds = self.compounds.iter().flat_map(|(&media_id, compound)| {
            compound
                .wanted
                .iter()
                .map(move |(&index, &plays)| (plays, OwnerRef::Compound(media_id), index))
        });
        let mut wanted: Vec<(u64, OwnerRef, u64)> = files.chain(compounds).collect();
        wanted.sort_unstable_by_key(|&(plays, owner, index)| {
            (plays.abs_diff(playhead), owner, index)
        });
        let block_bytes = BLOCK_FRAMES as usize * self.channels.max(1) as usize * 4;
        let fit = (self.budget_bytes / block_bytes).min(wanted.len());
        self.over_budget = fit < wanted.len();
        let mut kept: HashMap<Owner, Vec<(u64, u64)>> = HashMap::new();
        for &(plays, owner, index) in &wanted[..fit] {
            kept.entry(owner.to_owned())
                .or_default()
                .push((index, plays));
        }

        let mut dropped = false;
        for (&media_id, compound) in &mut self.compounds {
            let keep: HashSet<u64> = kept
                .remove(&Owner::Compound(media_id))
                .unwrap_or_default()
                .into_iter()
                .map(|(index, _)| index)
                .collect();
            let before = compound.blocks.len();
            let wanted = &compound.wanted;
            compound
                .blocks
                .retain(|index, _| keep.contains(index) || !wanted.contains_key(index));
            dropped |= compound.blocks.len() < before;
        }
        let mut plans: Vec<(Key, Range<u64>, u64)> = Vec::new();
        for (key, stream) in &mut self.streams {
            let keep = kept.remove(&Owner::File(key.clone())).unwrap_or_default();
            let keep_set: HashSet<u64> = keep.iter().map(|&(index, _)| index).collect();
            let before = stream.blocks.len();
            let wanted = &stream.wanted;
            stream
                .blocks
                .retain(|index, _| keep_set.contains(index) || !wanted.contains_key(index));
            dropped |= stream.blocks.len() < before;
            let job_keys = &mut self.job_keys;
            stream.jobs.retain(|job| {
                let keep = job.blocks.clone().any(|index| keep_set.contains(&index));
                if !keep {
                    job.cancelled.store(true, Ordering::Relaxed);
                    job_keys.remove(&job.id);
                }
                keep
            });
            let mut missing: Vec<(u64, u64)> = keep
                .into_iter()
                .filter(|&(index, _)| {
                    !stream.blocks.contains_key(&index) && !stream.in_flight(index)
                })
                .collect();
            missing.sort_unstable();
            for (index, plays) in missing {
                match plans.last_mut() {
                    Some((k, run, _))
                        if k == key && run.end == index && run.end - run.start < JOB_BLOCKS =>
                    {
                        run.end += 1;
                    }
                    _ => plans.push((key.clone(), index..index + 1, plays)),
                }
            }
        }
        // The worker may start on the first before the others arrive.
        plans.sort_by_key(|&(_, _, plays_at)| plays_at.abs_diff(playhead));
        for (key, blocks, plays_at) in plans {
            let id = self.next_id;
            self.next_id += 1;
            let cancelled = Arc::new(AtomicBool::new(false));
            self.worker.send(DecodeJob {
                id,
                path: key.0.clone(),
                stream: key.1,
                blocks: blocks.clone(),
                plays_at,
                cancelled: cancelled.clone(),
            });
            self.job_keys.insert(id, key.clone());
            if let Some(stream) = self.streams.get_mut(&key) {
                stream.jobs.push(Job {
                    id,
                    blocks,
                    cancelled,
                });
            }
        }
        dropped
    }

    /// Collects the decoded blocks; `true` when the mixer snapshot must be
    /// rebuilt.
    pub fn poll(&mut self) -> bool {
        let mut changed = self.unplanned && self.rebalance();
        while let Ok((media_id, content_hash, index, block)) = self.mixed_rx.try_recv() {
            let Some(compound) = self.compounds.get_mut(&media_id) else {
                continue;
            };
            if compound.content_hash == content_hash && compound.mixing.remove(&index) {
                compound.blocks.insert(index, block);
                self.unreported_blocks = true;
            }
        }
        while let Ok(decoded) = self.decoded_rx.try_recv() {
            match decoded {
                Decoded::Block {
                    job,
                    index,
                    samples,
                } => {
                    let Some(stream) = self
                        .job_keys
                        .get(&job)
                        .and_then(|k| self.streams.get_mut(k))
                    else {
                        continue;
                    };
                    stream.blocks.entry(index).or_insert(samples);
                    self.unreported_blocks = true;
                }
                Decoded::Done {
                    job,
                    end,
                    ends_before,
                    missing,
                } => {
                    let Some(key) = self.job_keys.remove(&job) else {
                        continue;
                    };
                    if let Some(stream) = self.streams.get_mut(&key) {
                        stream.jobs.retain(|j| j.id != job);
                        stream.end = end.or(stream.end);
                        stream.ends_before = match (stream.ends_before, ends_before) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                        stream.missing |= missing;
                    }
                    changed = true;
                }
            }
        }
        let now = Instant::now();
        let report_due = self
            .last_block_report
            .is_none_or(|last| now.duration_since(last) >= BLOCK_REBUILD_INTERVAL);
        if changed || (self.unreported_blocks && report_due) {
            changed |= self.unreported_blocks;
            self.unreported_blocks = false;
            self.last_block_report = Some(now);
        }
        while let Ok((key, result)) = self.stretch_rx.try_recv() {
            if let Some((_, entry)) = self.stretched.get_mut(&key) {
                *entry = match result {
                    Ok(samples) => Stretch::Ready(SampleBlocks::contiguous(
                        Arc::new(samples),
                        0,
                        self.channels,
                    )),
                    Err(e) => {
                        eprintln!("[mix_buffers] stretch failed: {e}");
                        Stretch::Failed
                    }
                };
                changed = true;
            }
        }
        while let Ok((key, slot, level)) = self.level_rx.try_recv() {
            match level {
                Some(level) => {
                    if let Some(entry) = self.levels.get_mut(&key) {
                        *entry = Some(level);
                        changed = true;
                    }
                    if let Some(slot) = slot {
                        self.last_level.insert(slot, level);
                    }
                }
                // Asked again, it is measured again.
                None => {
                    self.levels.remove(&key);
                }
            }
        }
        changed
    }

    #[cfg(test)]
    pub fn has_pending(&mut self) -> bool {
        self.poll();
        self.streams.values().any(|stream| !stream.jobs.is_empty())
            || self.compounds.values().any(|c| !c.mixing.is_empty())
    }
}

/// Every buffer is at the `sample_rate`/`channels` given to `spawn`.
impl AudioSource for MixBufferCache {
    /// The mixer always asks for frames: a whole file is never held.
    fn file(&mut self, _path: &Path, _stream: usize) -> ClipAudio {
        ClipAudio::Missing
    }

    fn file_frames(
        &mut self,
        path: &Path,
        stream: usize,
        frames: Range<u64>,
        plays_at: u64,
    ) -> ClipAudio {
        self.lookup(path, stream, frames, plays_at)
    }

    fn cached_compound(&mut self, media_id: MediaId, content_hash: u64) -> Option<Arc<Vec<f32>>> {
        self.compound_used.insert(media_id);
        self.compound
            .get(&media_id)
            .filter(|(hash, _)| *hash == content_hash)
            .map(|(_, mixdown)| mixdown.clone())
    }

    fn store_compound(&mut self, media_id: MediaId, content_hash: u64, mixdown: Arc<Vec<f32>>) {
        self.compound_used.insert(media_id);
        self.compound.insert(media_id, (content_hash, mixdown));
    }

    fn mixes_compounds_by_block(&self) -> bool {
        true
    }

    fn compound_block(
        &mut self,
        media_id: MediaId,
        content_hash: u64,
        index: u64,
        plays_at: u64,
    ) -> CompoundBlock {
        let playhead = self.playhead.load(Ordering::Relaxed);
        let compound = self.compounds.entry(media_id).or_default();
        if compound.content_hash != content_hash {
            *compound = CompoundCache {
                content_hash,
                ..Default::default()
            };
        }
        compound.asked = true;
        let closest = compound
            .wanted
            .get(&index)
            .is_none_or(|&other| plays_at.abs_diff(playhead) < other.abs_diff(playhead));
        if closest {
            compound.wanted.insert(index, plays_at);
        }
        if let Some(block) = compound.blocks.get(&index) {
            CompoundBlock::Ready(block.clone())
        } else if compound.mixing.contains(&index) {
            CompoundBlock::Mixing
        } else {
            CompoundBlock::Absent
        }
    }

    fn mix_compound_block(&mut self, media_id: MediaId, content_hash: u64, mix: CompoundMix) {
        let Some(compound) = self.compounds.get_mut(&media_id) else {
            return;
        };
        if compound.content_hash == content_hash && compound.mixing.insert(mix.index) {
            self.mix_worker.send((media_id, content_hash, mix));
        }
    }

    fn stretched(
        &mut self,
        buffer: &SampleBlocks,
        range: Range<u64>,
        tempo: f64,
        sample_rate: u32,
        channels: u16,
    ) -> ClipAudio {
        let mut identity = std::collections::hash_map::DefaultHasher::new();
        buffer.hash_identity(&mut identity);
        let key = (identity.finish(), range.start, range.end, tempo.to_bits());
        self.stretch_used.insert(key);
        if let Some((_, entry)) = self.stretched.get(&key) {
            return match entry {
                Stretch::Pending => ClipAudio::Pending,
                Stretch::Ready(samples) => ClipAudio::Ready(samples.clone()),
                Stretch::Failed => ClipAudio::Missing,
            };
        }
        self.stretch_worker.send(StretchJob {
            key,
            source: buffer.clone(),
            range,
            tempo,
            sample_rate,
            channels,
        });
        self.stretched
            .insert(key, (buffer.clone(), Stretch::Pending));
        ClipAudio::Pending
    }

    fn last_level(&mut self, slot: AnalysisSlot) -> Option<f32> {
        self.last_level.get(&slot).copied()
    }

    fn level(&mut self, analysis: LevelAnalysis) -> LevelReading {
        let key = analysis.key();
        self.level_used.insert(key);
        let last = analysis
            .slot
            .and_then(|slot| self.last_level.get(&slot).copied());
        match self.levels.get(&key) {
            Some(Some(level)) => LevelReading::Ready(*level),
            Some(None) => LevelReading::Pending(last),
            None => {
                self.level_worker.send(LevelJob { key, analysis });
                self.levels.insert(key, None);
                LevelReading::Pending(last)
            }
        }
    }
}

/// The next decode: the job closest to the playhead, with the queued jobs one
/// read of its file serves too, i.e. the other streams over the same blocks.
/// A file the decoder cannot seek in (it is resampled, see
/// `vv_media::decode_audio_streams_from`) is read from its start whatever the
/// job: every job queued for it goes in that read, instead of one read from
/// the start per job.
fn take_batch(
    queue: &mut Vec<DecodeJob>,
    playhead: u64,
    reads_from_start: &mut impl FnMut(&Path) -> bool,
) -> Vec<DecodeJob> {
    let Some(next) = queue
        .iter()
        .enumerate()
        .min_by_key(|(_, job)| (job.plays_at.abs_diff(playhead), job.id))
        .map(|(i, _)| i)
    else {
        return Vec::new();
    };
    let job = queue.swap_remove(next);
    let whole_file = reads_from_start(&job.path);
    let (mut batch, rest): (Vec<_>, Vec<_>) = std::mem::take(queue)
        .into_iter()
        .partition(|other| other.path == job.path && (whole_file || other.blocks == job.blocks));
    *queue = rest;
    batch.insert(0, job);
    batch
}

/// Decodes the blocks of `jobs` (all of `path`) in one read,
/// seeking to them, sending each block as soon as it is complete and
/// stopping once the last job is. `false` if the receiver is gone (the
/// worker must exit).
fn decode_blocks(
    path: &Path,
    jobs: &[DecodeJob],
    sample_rate: u32,
    channels: u16,
    decoded_tx: &mpsc::Sender<Decoded>,
) -> bool {
    let mut streams: Vec<usize> = jobs.iter().map(|job| job.stream).collect();
    streams.sort_unstable();
    streams.dedup();
    let slot_of = |stream: usize| streams.iter().position(|&s| s == stream).unwrap();
    let ch = channels.max(1) as u64;
    let windows: Vec<Range<u64>> = jobs
        .iter()
        .map(|job| job.blocks.start * BLOCK_FRAMES..job.blocks.end * BLOCK_FRAMES)
        .collect();
    let from = windows.iter().map(|w| w.start).min().unwrap_or(0);
    let mut filling: Vec<Vec<f32>> = jobs.iter().map(|_| new_block(ch)).collect();
    let mut filled_to: Vec<u64> = windows.iter().map(|w| w.start).collect();
    let mut decoded = vec![0_u64; streams.len()];
    let mut remixed = Vec::new();
    let mut receiver_gone = false;
    let mut stopped_early = false;
    let result = vv_media::decode_audio_streams_from(
        path,
        &streams,
        sample_rate,
        from,
        |slot, src_channels, at, chunk| {
            remixed.clear();
            vv_audio::mixer::remix_channels_into(chunk, src_channels, channels, &mut remixed);
            let to = at + remixed.len() as u64 / ch;
            decoded[slot] = decoded[slot].max(to);
            let mut finished = true;
            for (i, job) in jobs.iter().enumerate() {
                if job.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let window = &windows[i];
                if job.stream == streams[slot] {
                    // A gap in the stream plays as silence.
                    let gap_end = at.min(window.end);
                    let until = window.end.min(to);
                    let sent = (filled_to[i] >= gap_end
                        || fill(
                            &mut filling[i],
                            &mut filled_to[i],
                            gap_end,
                            None,
                            ch,
                            job,
                            decoded_tx,
                        ))
                        && (filled_to[i] >= until || {
                            let skip = ((filled_to[i] - at) * ch) as usize;
                            let samples = &remixed[skip..((until - at) * ch) as usize];
                            fill(
                                &mut filling[i],
                                &mut filled_to[i],
                                until,
                                Some(samples),
                                ch,
                                job,
                                decoded_tx,
                            )
                        });
                    if !sent {
                        receiver_gone = true;
                        return ControlFlow::Break(());
                    }
                }
                if filled_to[i] < window.end {
                    finished = false;
                }
            }
            if finished {
                stopped_early = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    if receiver_gone {
        return false;
    }
    let formats = result.unwrap_or_else(|e| {
        eprintln!("[mix_buffers] decode failed for {}: {e}", path.display());
        vec![None; streams.len()]
    });
    for ((job, last), filled_to) in jobs.iter().zip(filling).zip(filled_to) {
        let slot = slot_of(job.stream);
        let missing = formats[slot].is_none();
        let at_end = !stopped_early && !missing;
        let end = (at_end && decoded[slot] > 0).then_some(decoded[slot]);
        let ends_before = (at_end && decoded[slot] == 0).then_some(from);
        // The block the file ends in, shorter than the others.
        if end.is_some() && !last.is_empty() {
            let block = Decoded::Block {
                job: job.id,
                index: (filled_to - 1) / BLOCK_FRAMES,
                samples: Arc::new(last),
            };
            if decoded_tx.send(block).is_err() {
                return false;
            }
        }
        let done = Decoded::Done {
            job: job.id,
            end,
            ends_before,
            missing,
        };
        if decoded_tx.send(done).is_err() {
            return false;
        }
    }
    true
}

fn new_block(ch: u64) -> Vec<f32> {
    Vec::with_capacity((BLOCK_FRAMES * ch) as usize)
}

/// Appends frames up to `until` to the block being filled (`samples`, or
/// silence), sending every block it completes. `false` if the receiver is gone.
fn fill(
    filling: &mut Vec<f32>,
    filled_to: &mut u64,
    until: u64,
    samples: Option<&[f32]>,
    ch: u64,
    job: &DecodeJob,
    decoded_tx: &mpsc::Sender<Decoded>,
) -> bool {
    let mut offset = 0;
    while *filled_to < until {
        let block_end = (*filled_to / BLOCK_FRAMES + 1) * BLOCK_FRAMES;
        let next = until.min(block_end);
        let count = ((next - *filled_to) * ch) as usize;
        match samples {
            Some(samples) => filling.extend_from_slice(&samples[offset..offset + count]),
            None => filling.resize(filling.len() + count, 0.0),
        }
        offset += count;
        *filled_to = next;
        if next == block_end {
            let block = Decoded::Block {
                job: job.id,
                index: next / BLOCK_FRAMES - 1,
                samples: Arc::new(std::mem::replace(filling, new_block(ch))),
            };
            if decoded_tx.send(block).is_err() {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
#[path = "tests/mix_buffers.rs"]
mod tests;
