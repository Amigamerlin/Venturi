//! Audio buffers already in the mixer's format, per `(path, audio_stream_index)`,
//! holding only the frames the timeline reads (`Segment`). Decode + resample
//! on a dedicated thread. A buffer is published partially while it grows: the
//! mixer plays its start before the decode finishes, past the ready part there
//! is silence.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use vv_audio::{AnalysisSlot, AudioSource, ClipAudio, LevelAnalysis, LevelReading};
use vv_core::MediaId;

use crate::worker::Worker;

type Key = (PathBuf, usize);

/// Source buffer (by address: the entry keeps it alive), frame range, tempo bits.
type StretchKey = (usize, u64, u64, u64);

struct StretchJob {
    key: StretchKey,
    source: Arc<Vec<f32>>,
    range: std::ops::Range<u64>,
    tempo: f64,
    sample_rate: u32,
    channels: u16,
}

enum Stretch {
    Pending,
    Ready(Arc<Vec<f32>>),
    Failed,
}

struct LevelJob {
    key: u64,
    analysis: LevelAnalysis,
}

/// `None`: dropped for a newer job of the same slot.
type LevelResult = (u64, Option<AnalysisSlot>, Option<f32>);

/// The decoded frames `frames` of one stream; the buffer starts at `frames.start`.
struct Segment {
    id: u64,
    frames: Range<u64>,
    /// `None` = no samples yet, or no audio there.
    buffer: Option<Arc<Vec<f32>>>,
    done: bool,
    prioritized: bool,
    cancelled: Arc<AtomicBool>,
}

struct DecodeJob {
    id: u64,
    path: PathBuf,
    stream: usize,
    frames: Range<u64>,
    first: bool,
    cancelled: Arc<AtomicBool>,
}

struct Ready {
    id: u64,
    buffer: Option<Arc<Vec<f32>>>,
    done: bool,
}

/// Seconds of audio after which the first publication goes out; the following
/// ones at every doubling, so the buffer copies cost ~2x the decode.
const FIRST_PUBLISH_SECS: usize = 1;

/// Decoded around what the clips read: a trim within it does not decode
/// again, and clips closer than twice this share a segment.
const SEGMENT_MARGIN_SECS: u64 = 5;

pub struct MixBufferCache {
    sample_rate: u32,
    channels: u16,
    segments: HashMap<Key, Vec<Segment>>,
    /// Ranges asked for and not covered, sent to the worker by `flush_plans`
    /// so that the clips of one pass are merged into few segments.
    plans: Vec<(Key, Vec<Range<u64>>, bool)>,
    /// Segments read since `begin_pass`.
    used: HashSet<u64>,
    next_id: u64,
    worker: Worker<DecodeJob>,
    ready_rx: mpsc::Receiver<Ready>,
    /// Mixdowns of compound clips, with the `content_hash` they were
    /// computed with (see `vv_audio::mixer::compound_mixdown`).
    compound: HashMap<MediaId, (u64, Arc<Vec<f32>>)>,
    compound_used: HashSet<MediaId>,
    /// Pitch-corrected clip audio (see `AudioSource::stretched`), with the
    /// source buffer it was made from.
    stretched: HashMap<StretchKey, (Arc<Vec<f32>>, Stretch)>,
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
        let (ready_tx, ready_rx) = mpsc::channel::<Ready>();
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
                // The other queued segments of the same file go in the
                // same pass: one read of the file serves them all.
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
                if !decode_progressively(&path, &batch, sample_rate, channels, &ready_tx) {
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
            sample_rate,
            channels,
            segments: HashMap::new(),
            plans: Vec::new(),
            used: HashSet::new(),
            next_id: 0,
            worker,
            ready_rx,
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
        self.used.clear();
        self.compound_used.clear();
    }

    /// Drops the buffers, mixdowns, stretches and levels no mix asked for:
    /// to be called after rebuilding the snapshot of the whole timeline.
    pub fn sweep_unused(&mut self) {
        self.flush_plans();
        let used = &self.used;
        self.segments.retain(|_, segments| {
            segments.retain(|segment| {
                let keep = used.contains(&segment.id);
                if !keep {
                    segment.cancelled.store(true, Ordering::Relaxed);
                }
                keep
            });
            !segments.is_empty()
        });
        let used = &self.compound_used;
        self.compound.retain(|media_id, _| used.contains(media_id));
        let used = std::mem::take(&mut self.stretch_used);
        self.stretched.retain(|key, _| used.contains(key));
        let used = std::mem::take(&mut self.level_used);
        self.levels.retain(|key, _| used.contains(key));
    }

    /// The whole of `stream` of `path`, if anything is decoded yet. Queues
    /// its decode ahead of the others (it does not interrupt the one in
    /// progress).
    pub fn get_or_request_first(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        let (audio, _) = self.lookup(path, stream, 0..u64::MAX, true);
        self.flush_plans();
        match audio {
            ClipAudio::Ready(buffer) | ClipAudio::Partial(buffer) => Some(buffer),
            ClipAudio::Pending | ClipAudio::Missing => None,
        }
    }

    #[cfg(test)]
    pub fn get_or_request(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        match self.lookup(path, stream, 0..u64::MAX, false).0 {
            ClipAudio::Ready(buffer) | ClipAudio::Partial(buffer) => Some(buffer),
            ClipAudio::Pending | ClipAudio::Missing => None,
        }
    }

    /// Non-blocking: the newest complete segment holding `needed`, else
    /// the start of it from a segment still decoding (or from an older one
    /// while a larger one decodes). What no segment will hold is planned.
    fn lookup(
        &mut self,
        path: &Path,
        stream: usize,
        needed: Range<u64>,
        first: bool,
    ) -> (ClipAudio, u64) {
        let key = (path.to_path_buf(), stream);
        let segments = self
            .segments
            .get_mut(&key)
            .map(Vec::as_mut_slice)
            .unwrap_or_default();
        let covers = |s: &Segment| s.frames.start <= needed.start && needed.end <= s.frames.end;
        if let Some(segment) = segments
            .iter()
            .filter(|s| s.done && covers(s))
            .max_by_key(|s| s.id)
        {
            self.used.insert(segment.id);
            let audio = segment
                .buffer
                .clone()
                .map_or(ClipAudio::Missing, ClipAudio::Ready);
            return (audio, segment.frames.start);
        }
        match segments
            .iter_mut()
            .filter(|s| covers(s))
            .max_by_key(|s| s.id)
        {
            Some(decoding) => {
                self.used.insert(decoding.id);
                if first && !decoding.prioritized {
                    decoding.prioritized = true;
                    self.worker.send(DecodeJob {
                        id: decoding.id,
                        path: key.0.clone(),
                        stream,
                        frames: decoding.frames.clone(),
                        first,
                        cancelled: decoding.cancelled.clone(),
                    });
                }
            }
            None => self.plan(key.clone(), needed.clone(), first),
        }
        let ch = self.channels.max(1) as u64;
        let partial = self
            .segments
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|s| s.frames.start <= needed.start)
            .filter_map(|s| {
                let buffer = s.buffer.as_ref()?;
                let end = s.frames.start + buffer.len() as u64 / ch;
                (end > needed.start).then_some((s, end))
            })
            .max_by_key(|(_, end)| *end);
        match partial {
            Some((segment, _)) => {
                self.used.insert(segment.id);
                let buffer = segment.buffer.clone().unwrap();
                (ClipAudio::Partial(buffer), segment.frames.start)
            }
            None => (ClipAudio::Pending, 0),
        }
    }

    fn plan(&mut self, key: Key, needed: Range<u64>, first: bool) {
        let margin = SEGMENT_MARGIN_SECS * self.sample_rate as u64;
        let range = needed.start.saturating_sub(margin)..needed.end.saturating_add(margin);
        match self.plans.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, ranges, prioritized)) => {
                ranges.push(range);
                *prioritized |= first;
            }
            None => self.plans.push((key, vec![range], first)),
        }
    }

    /// Turns the plans into segments, merged with each other and with the
    /// segments they overlap: the new one replaces those, which go at the
    /// next `sweep_unused` once nothing reads them.
    fn flush_plans(&mut self) {
        for ((path, stream), mut ranges, first) in std::mem::take(&mut self.plans) {
            let key = (path, stream);
            let existing = self.segments.entry(key.clone()).or_default();
            loop {
                let merged = merge_ranges(std::mem::take(&mut ranges));
                let absorbed: Vec<Range<u64>> = existing
                    .iter()
                    .filter(|s| !s.cancelled.load(Ordering::Relaxed))
                    .map(|s| s.frames.clone())
                    .filter(|f| {
                        merged.iter().any(|r| r.start < f.end && f.start < r.end)
                            && !merged.iter().any(|r| r.start <= f.start && f.end <= r.end)
                    })
                    .collect();
                ranges = merged;
                if absorbed.is_empty() {
                    break;
                }
                ranges.extend(absorbed);
            }
            for frames in ranges {
                let id = self.next_id;
                self.next_id += 1;
                let cancelled = Arc::new(AtomicBool::new(false));
                self.worker.send(DecodeJob {
                    id,
                    path: key.0.clone(),
                    stream: key.1,
                    frames: frames.clone(),
                    first,
                    cancelled: cancelled.clone(),
                });
                self.used.insert(id);
                existing.push(Segment {
                    id,
                    frames,
                    buffer: None,
                    done: false,
                    prioritized: first,
                    cancelled,
                });
            }
        }
    }

    /// Collects the ready buffers (partial ones too); `true` if at least one
    /// arrived (the mixer snapshot must be rebuilt).
    pub fn poll(&mut self) -> bool {
        self.flush_plans();
        let mut changed = false;
        while let Ok(ready) = self.ready_rx.try_recv() {
            let segment = self
                .segments
                .values_mut()
                .flatten()
                .find(|s| s.id == ready.id);
            let Some(segment) = segment else {
                continue;
            };
            segment.done = ready.done;
            if ready.buffer.is_some() {
                changed = true;
                segment.buffer = ready.buffer;
            } else if ready.done {
                changed = true;
            }
        }
        while let Ok((key, result)) = self.stretch_rx.try_recv() {
            if let Some((_, entry)) = self.stretched.get_mut(&key) {
                *entry = match result {
                    Ok(samples) => Stretch::Ready(Arc::new(samples)),
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
        self.segments.values().flatten().any(|s| !s.done)
    }
}

/// Every buffer is at the `sample_rate`/`channels` given to `spawn`.
impl AudioSource for MixBufferCache {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self.file_frames(path, stream, 0..u64::MAX).0
    }

    fn file_frames(&mut self, path: &Path, stream: usize, frames: Range<u64>) -> (ClipAudio, u64) {
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
        buffer: &Arc<Vec<f32>>,
        range: std::ops::Range<u64>,
        tempo: f64,
        sample_rate: u32,
        channels: u16,
    ) -> ClipAudio {
        let key = (
            Arc::as_ptr(buffer) as usize,
            range.start,
            range.end,
            tempo.to_bits(),
        );
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

/// Decodes `jobs` (all of `path`) in one read from the start of the file,
/// stopping once the last of them is complete. `false` if the receiver is
/// gone (the worker must exit).
fn decode_progressively(
    path: &Path,
    jobs: &[DecodeJob],
    sample_rate: u32,
    channels: u16,
    ready_tx: &mpsc::Sender<Ready>,
) -> bool {
    let mut streams: Vec<usize> = jobs.iter().map(|job| job.stream).collect();
    streams.sort_unstable();
    streams.dedup();
    let slot_of = |stream: usize| streams.iter().position(|&s| s == stream).unwrap();
    let ch = channels.max(1) as u64;
    let first_publish = FIRST_PUBLISH_SECS * sample_rate as usize * channels as usize;
    let mut buffers = vec![Vec::new(); jobs.len()];
    let mut next_publish = vec![first_publish; jobs.len()];
    let mut decoded = vec![0_u64; streams.len()];
    let mut remixed = Vec::new();
    let mut receiver_gone = false;
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
                let (a, b) = (job.frames.start.max(from), job.frames.end.min(to));
                if job.stream == streams[slot] && a < b {
                    let buffer = &mut buffers[i];
                    buffer.extend_from_slice(
                        &remixed[((a - from) * ch) as usize..((b - from) * ch) as usize],
                    );
                    if buffer.len() >= next_publish[i] {
                        next_publish[i] = buffer.len() * 2;
                        let ready = Ready {
                            id: job.id,
                            buffer: Some(Arc::new(buffer.clone())),
                            done: false,
                        };
                        if ready_tx.send(ready).is_err() {
                            receiver_gone = true;
                            return ControlFlow::Break(());
                        }
                    }
                }
                if decoded[slot_of(job.stream)] < job.frames.end {
                    finished = false;
                }
            }
            if finished {
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
    for (job, buffer) in jobs.iter().zip(buffers) {
        let buffer = (formats[slot_of(job.stream)].is_some() && !buffer.is_empty())
            .then(|| Arc::new(buffer));
        let ready = Ready {
            id: job.id,
            buffer,
            done: true,
        };
        if ready_tx.send(ready).is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "tests/mix_buffers.rs"]
mod tests;
