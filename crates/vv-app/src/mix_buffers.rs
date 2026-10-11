//! Audio buffers already in the mixer's format, per `(path, audio_stream_index)`,
//! in blocks of `BLOCK_FRAMES` holding only what the timeline reads. Decode +
//! resample on a dedicated thread; every block plays as soon as it is decoded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hasher;
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vv_audio::{AnalysisSlot, AudioSource, ClipAudio, LevelAnalysis, LevelReading, SampleBlocks};
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

/// About 1.4 s at 48 kHz.
const BLOCK_FRAMES: u64 = 1 << 16;

/// Blocks kept on each side of what the clips read: a trim into them
/// decodes nothing.
const MARGIN_BLOCKS: u64 = 1;

/// New blocks rebuild the mix at most this often: until compound clips are
/// mixed by block, each rebuild mixes a partial compound whole.
const BLOCK_REBUILD_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct StreamCache {
    blocks: HashMap<u64, Arc<Vec<f32>>>,
    jobs: Vec<Job>,
    /// Frames of the stream, once a decode reached its end.
    end: Option<u64>,
    /// No such stream, or it does not decode.
    missing: bool,
    /// Read since `begin_pass`.
    asked: bool,
    used: HashSet<u64>,
    /// Read whole with its end still unknown: no block is dropped.
    used_all: bool,
}

impl StreamCache {
    fn in_flight(&self, block: u64) -> bool {
        self.jobs.iter().any(|job| job.blocks.contains(&block))
    }
}

struct Job {
    id: u64,
    blocks: Range<u64>,
    prioritized: bool,
    cancelled: Arc<AtomicBool>,
}

struct DecodeJob {
    id: u64,
    path: PathBuf,
    stream: usize,
    blocks: Range<u64>,
    first: bool,
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
        missing: bool,
    },
}

pub struct MixBufferCache {
    channels: u16,
    streams: HashMap<Key, StreamCache>,
    /// Block ranges asked for and not decoded nor in flight, sent to the
    /// worker by `flush_plans` so the clips of one pass make few jobs.
    plans: Vec<(Key, Vec<Range<u64>>, bool)>,
    job_keys: HashMap<u64, Key>,
    next_id: u64,
    worker: Worker<DecodeJob>,
    decoded_rx: mpsc::Receiver<Decoded>,
    /// Blocks arrived and not yet reported by `poll`, and when it last did.
    unreported_blocks: bool,
    last_block_report: Option<Instant>,
    /// Mixdowns of compound clips, with the `content_hash` they were
    /// computed with (see `vv_audio::mixer::compound_mixdown`).
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
        let worker = Worker::spawn(move |job_rx: mpsc::Receiver<DecodeJob>| {
            let mut queue = VecDeque::new();
            let mut started = HashSet::new();
            loop {
                if queue.is_empty() {
                    match job_rx.recv() {
                        Ok(job) => enqueue(&mut queue, job),
                        Err(_) => break,
                    }
                }
                while let Ok(job) = job_rx.try_recv() {
                    enqueue(&mut queue, job);
                }
                let Some(job) = queue.pop_front() else {
                    continue;
                };
                // The other queued jobs of the same file go in the same
                // pass: one read of the file serves them all.
                let path = job.path.clone();
                let mut batch = vec![job];
                let mut rest = VecDeque::new();
                for job in queue.drain(..) {
                    if job.path == path {
                        batch.push(job);
                    } else {
                        rest.push_back(job);
                    }
                }
                queue = rest;
                // A priority request duplicates one already queued.
                batch
                    .retain(|job| !job.cancelled.load(Ordering::Relaxed) && started.insert(job.id));
                if batch.is_empty() {
                    continue;
                }
                if !decode_blocks(&path, &batch, sample_rate, channels, &decoded_tx) {
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
            streams: HashMap::new(),
            plans: Vec::new(),
            job_keys: HashMap::new(),
            next_id: 0,
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

    /// From here to `sweep_unused`, a full mix of the timeline: what it
    /// does not read is dropped.
    pub fn begin_pass(&mut self) {
        for stream in self.streams.values_mut() {
            stream.asked = false;
            stream.used.clear();
            stream.used_all = false;
        }
        self.compound_used.clear();
    }

    /// Drops the blocks, mixdowns, stretches and levels no mix asked for, and
    /// cancels the decodes no longer wanted: to be called after rebuilding
    /// the snapshot of the whole timeline.
    pub fn sweep_unused(&mut self) {
        self.flush_plans();
        let job_keys = &mut self.job_keys;
        self.streams.retain(|_, stream| {
            let StreamCache {
                blocks,
                jobs,
                asked,
                used,
                used_all,
                ..
            } = stream;
            if !*used_all {
                blocks.retain(|index, _| *asked && used.contains(index));
                jobs.retain(|job| {
                    let wanted = *asked && job.blocks.clone().any(|index| used.contains(&index));
                    if !wanted {
                        job.cancelled.store(true, Ordering::Relaxed);
                        job_keys.remove(&job.id);
                    }
                    wanted
                });
            }
            *asked
        });
        let used = &self.compound_used;
        self.compound.retain(|media_id, _| used.contains(media_id));
        let used = std::mem::take(&mut self.stretch_used);
        self.stretched.retain(|key, _| used.contains(key));
        let used = std::mem::take(&mut self.level_used);
        self.levels.retain(|key, _| used.contains(key));
    }

    /// The whole of `stream` of `path`, as far as decoded. Queues its decode
    /// ahead of the others (it does not interrupt the one in progress).
    pub fn get_or_request_first(&mut self, path: &Path, stream: usize) -> Option<SampleBlocks> {
        let audio = self.lookup(path, stream, 0..u64::MAX, true);
        self.flush_plans();
        match audio {
            ClipAudio::Ready(buffer) | ClipAudio::Partial(buffer) => Some(buffer),
            ClipAudio::Pending | ClipAudio::Missing => None,
        }
    }

    /// Non-blocking: the blocks holding `needed`, `Partial` while some are
    /// still decoding. Those neither decoded nor in flight are planned,
    /// with `MARGIN_BLOCKS` around them.
    fn lookup(&mut self, path: &Path, stream: usize, needed: Range<u64>, first: bool) -> ClipAudio {
        let key = (path.to_path_buf(), stream);
        let entry = self.streams.entry(key.clone()).or_default();
        entry.asked = true;
        if entry.missing {
            return ClipAudio::Missing;
        }
        let first_block = needed.start / BLOCK_FRAMES;
        let end = entry.end.map_or(needed.end, |end| end.min(needed.end));
        let unbounded = end == u64::MAX;
        let last_block = if unbounded {
            entry.blocks.keys().max().map_or(0, |max| max + 1)
        } else {
            end.div_ceil(BLOCK_FRAMES)
        }
        .max(first_block);

        let mut wanted = Vec::new();
        if unbounded {
            entry.used_all = true;
            if !entry.jobs.iter().any(|job| job.blocks.end == u64::MAX) {
                wanted.push(first_block..u64::MAX);
            }
        } else {
            let margin_end = match entry.end {
                Some(end) => (last_block + MARGIN_BLOCKS).min(end.div_ceil(BLOCK_FRAMES)),
                None => last_block + MARGIN_BLOCKS,
            };
            for index in first_block.saturating_sub(MARGIN_BLOCKS)..margin_end.max(last_block) {
                entry.used.insert(index);
                if entry.blocks.contains_key(&index) {
                    continue;
                }
                if entry.in_flight(index) {
                    continue;
                }
                match wanted.last_mut() {
                    Some(run) if run.end == index => run.end = index + 1,
                    _ => wanted.push(index..index + 1),
                }
            }
        }
        if first {
            for job in entry.jobs.iter_mut().filter(|job| !job.prioritized) {
                job.prioritized = true;
                self.worker.send(DecodeJob {
                    id: job.id,
                    path: key.0.clone(),
                    stream,
                    blocks: job.blocks.clone(),
                    first,
                    cancelled: job.cancelled.clone(),
                });
            }
        }

        let blocks: Vec<Option<Arc<Vec<f32>>>> = (first_block..last_block)
            .map(|index| entry.blocks.get(&index).cloned())
            .collect();
        let present = blocks.iter().filter(|b| b.is_some()).count();
        let complete = !unbounded && present == blocks.len();
        let view_end = match entry.end {
            Some(end) => end.min(last_block * BLOCK_FRAMES),
            None => last_block * BLOCK_FRAMES,
        };
        let view = SampleBlocks::new(
            first_block * BLOCK_FRAMES,
            BLOCK_FRAMES,
            self.channels,
            blocks,
            view_end,
        );
        if !wanted.is_empty() {
            self.plan(key, wanted, first);
        }
        if complete {
            ClipAudio::Ready(view)
        } else if present > 0 {
            ClipAudio::Partial(view)
        } else {
            ClipAudio::Pending
        }
    }

    fn plan(&mut self, key: Key, blocks: Vec<Range<u64>>, first: bool) {
        match self.plans.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, planned, prioritized)) => {
                planned.extend(blocks);
                *prioritized |= first;
            }
            None => self.plans.push((key, blocks, first)),
        }
    }

    fn flush_plans(&mut self) {
        for (key, planned, first) in std::mem::take(&mut self.plans) {
            let stream = self.streams.entry(key.clone()).or_default();
            for blocks in merge_ranges(planned) {
                let id = self.next_id;
                self.next_id += 1;
                let cancelled = Arc::new(AtomicBool::new(false));
                self.worker.send(DecodeJob {
                    id,
                    path: key.0.clone(),
                    stream: key.1,
                    blocks: blocks.clone(),
                    first,
                    cancelled: cancelled.clone(),
                });
                self.job_keys.insert(id, key.clone());
                stream.jobs.push(Job {
                    id,
                    blocks,
                    prioritized: first,
                    cancelled,
                });
            }
        }
    }

    /// Collects the decoded blocks; `true` when the mixer snapshot must be
    /// rebuilt.
    pub fn poll(&mut self) -> bool {
        self.flush_plans();
        let mut changed = false;
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
                Decoded::Done { job, end, missing } => {
                    let Some(key) = self.job_keys.remove(&job) else {
                        continue;
                    };
                    if let Some(stream) = self.streams.get_mut(&key) {
                        stream.jobs.retain(|j| j.id != job);
                        stream.end = end.or(stream.end);
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
    }
}

/// Every buffer is at the `sample_rate`/`channels` given to `spawn`.
impl AudioSource for MixBufferCache {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self.lookup(path, stream, 0..u64::MAX, false)
    }

    fn file_frames(&mut self, path: &Path, stream: usize, frames: Range<u64>) -> ClipAudio {
        self.lookup(path, stream, frames, false)
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

fn enqueue(queue: &mut VecDeque<DecodeJob>, job: DecodeJob) {
    if job.first {
        queue.push_front(job);
    } else {
        queue.push_back(job);
    }
}

/// Sorted, with the overlapping or touching ranges joined.
fn merge_ranges(mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Decodes the blocks of `jobs` (all of `path`) in one read from the start
/// of the file, sending each as soon as it is complete and stopping once
/// the last job is. `false` if the receiver is gone (the worker must exit).
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
        .map(|job| {
            job.blocks.start.saturating_mul(BLOCK_FRAMES)
                ..job.blocks.end.saturating_mul(BLOCK_FRAMES)
        })
        .collect();
    let new_block = || Vec::with_capacity((BLOCK_FRAMES * ch) as usize);
    let mut filling: Vec<Vec<f32>> = jobs.iter().map(|_| new_block()).collect();
    let mut decoded = vec![0_u64; streams.len()];
    let mut remixed = Vec::new();
    let mut receiver_gone = false;
    let mut stopped_early = false;
    let result = vv_media::decode_audio_streams_streaming(
        path,
        &streams,
        Some(sample_rate),
        |slot, src_channels, chunk| {
            remixed.clear();
            vv_audio::mixer::remix_channels_into(chunk, src_channels, channels, &mut remixed);
            let from = decoded[slot];
            let to = from + remixed.len() as u64 / ch;
            decoded[slot] = to;
            let mut finished = true;
            for (i, job) in jobs.iter().enumerate() {
                if job.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let window = &windows[i];
                if job.stream == streams[slot] {
                    let mut frame = window.start.max(from);
                    let until = window.end.min(to);
                    while frame < until {
                        let block_end = (frame / BLOCK_FRAMES + 1) * BLOCK_FRAMES;
                        let next = until.min(block_end);
                        filling[i].extend_from_slice(
                            &remixed[((frame - from) * ch) as usize..((next - from) * ch) as usize],
                        );
                        frame = next;
                        if frame == block_end {
                            let block = Decoded::Block {
                                job: job.id,
                                index: frame / BLOCK_FRAMES - 1,
                                samples: Arc::new(std::mem::replace(&mut filling[i], new_block())),
                            };
                            if decoded_tx.send(block).is_err() {
                                receiver_gone = true;
                                return ControlFlow::Break(());
                            }
                        }
                    }
                }
                if decoded[slot_of(job.stream)] < window.end {
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
    for (job, last) in jobs.iter().zip(filling) {
        let slot = slot_of(job.stream);
        let missing = formats[slot].is_none();
        let end = (!stopped_early && !missing).then_some(decoded[slot]);
        // The block the file ends in, shorter than the others.
        if end.is_some() && !last.is_empty() {
            let block = Decoded::Block {
                job: job.id,
                index: decoded[slot] / BLOCK_FRAMES,
                samples: Arc::new(last),
            };
            if decoded_tx.send(block).is_err() {
                return false;
            }
        }
        if decoded_tx
            .send(Decoded::Done {
                job: job.id,
                end,
                missing,
            })
            .is_err()
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "tests/mix_buffers.rs"]
mod tests;
