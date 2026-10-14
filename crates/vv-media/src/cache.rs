//! `SharedFrameCache`: cache of the decoded frames of every media in the
//! window, on a global budget, see plans/REFACTOR_PIPELINE.md §2.

use crate::decode::FrameYuv420;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vv_core::{FrameIdx, MediaId};

/// Stretch of source frames wanted by the current window, with its
/// position on the timeline: used to measure the distance from the playhead.
#[derive(Debug, Clone, Copy)]
pub struct WantedRange {
    pub media_id: MediaId,
    pub source_start: FrameIdx,
    pub source_end: FrameIdx,
    /// Where `source_start` falls in timeline space.
    pub timeline_start: FrameIdx,
    /// The clip's `Clip::rate`: timeline frames per source frame.
    pub rate: vv_core::Rational,
}

impl WantedRange {
    fn contains(&self, idx: FrameIdx) -> bool {
        idx >= self.source_start && idx <= self.source_end
    }

    fn timeline_position_of(&self, idx: FrameIdx) -> FrameIdx {
        self.timeline_start + self.rate.scale_round(idx) - self.rate.scale_round(self.source_start)
    }

    /// A range of a compound clip's nested timeline placed on the timeline
    /// of `outer`, the compound clip's own range: positions in two
    /// different timelines cannot be compared with each other.
    pub fn seen_through(self, outer: &WantedRange) -> Self {
        Self {
            timeline_start: outer.timeline_position_of(self.timeline_start),
            rate: outer
                .rate
                .divided_by(vv_core::Rational::new(self.rate.den, self.rate.num)),
            ..self
        }
    }

    /// The part `source_start..=source_end` of this range.
    pub fn sub_range(&self, source_start: FrameIdx, source_end: FrameIdx) -> Self {
        Self {
            source_start,
            source_end,
            timeline_start: self.timeline_position_of(source_start),
            ..*self
        }
    }
}

struct SharedInner {
    entries: HashMap<(MediaId, FrameIdx), Arc<FrameYuv420>>,
    bytes_used: usize,
    /// See `set_stride`; absent means 1.
    strides: HashMap<MediaId, FrameIdx>,
}

/// Cache of the frames of every media in the window, keyed by
/// `(MediaId, source frame)` with a budget in bytes (different resolutions
/// weigh differently). What to keep is decided by `reconcile` alone.
pub struct SharedFrameCache {
    inner: Mutex<SharedInner>,
}

impl Default for SharedFrameCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedFrameCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SharedInner {
                entries: HashMap::new(),
                bytes_used: 0,
                strides: HashMap::new(),
            }),
        }
    }

    pub fn get(&self, media_id: MediaId, idx: FrameIdx) -> Option<Arc<FrameYuv420>> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&(media_id, idx))
            .cloned()
    }

    /// `idx`, or else the nearest cached frame within the stride of the media
    /// and within `lo..=hi` (the clip's source frames, widened to `idx`: a
    /// transition shows frames past the trim), earlier first: with a stride
    /// the frames in between are not decoded at all.
    pub fn get_near(
        &self,
        media_id: MediaId,
        idx: FrameIdx,
        lo: FrameIdx,
        hi: FrameIdx,
    ) -> Option<Arc<FrameYuv420>> {
        let inner = self.inner.lock().unwrap();
        let stride = inner.stride(media_id);
        let clip = lo.min(idx)..=hi.max(idx);
        (0..stride)
            .flat_map(|d| [idx - d, idx + d])
            .filter(|i| clip.contains(i))
            .find_map(|i| inner.entries.get(&(media_id, i)).cloned())
    }

    /// Only the frames at multiples of `stride` are wanted for `media_id`
    /// (fast playback shows no others): `covers` and `cached_ranges` count the
    /// ones in between as there.
    pub fn set_stride(&self, media_id: MediaId, stride: FrameIdx) {
        let mut inner = self.inner.lock().unwrap();
        if stride > 1 {
            inner.strides.insert(media_id, stride);
        } else {
            inner.strides.remove(&media_id);
        }
    }

    pub fn stride(&self, media_id: MediaId) -> FrameIdx {
        self.inner.lock().unwrap().stride(media_id)
    }

    pub fn contains(&self, media_id: MediaId, idx: FrameIdx) -> bool {
        self.inner
            .lock()
            .unwrap()
            .entries
            .contains_key(&(media_id, idx))
    }

    /// Bytes used, on the real size of each frame.
    pub fn bytes_used(&self) -> usize {
        self.inner.lock().unwrap().bytes_used
    }

    /// Does not enforce the budget: the caller inserts in priority order and
    /// stops on its own.
    pub fn insert(&self, media_id: MediaId, idx: FrameIdx, frame: Arc<FrameYuv420>) {
        let mut inner = self.inner.lock().unwrap();
        let bytes = frame.byte_len();
        if let Some(old) = inner.entries.insert((media_id, idx), frame) {
            inner.bytes_used -= old.byte_len();
        }
        inner.bytes_used += bytes;
    }

    /// Empties everything: for when the frames come from a wrong source (e.g.
    /// the proxy toggle changes), which `reconcile` would not be able to tell.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.bytes_used = 0;
    }

    /// Drops every frame of `media_id`: for when the id now names another
    /// file, which `reconcile` cannot tell.
    pub fn remove_media(&self, media_id: MediaId) {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;
        inner.strides.remove(&media_id);
        let bytes_used = &mut inner.bytes_used;
        inner.entries.retain(|&(m, _), frame| {
            let keep = m != media_id;
            if !keep {
                *bytes_used -= frame.byte_len();
            }
            keep
        });
    }

    /// The only eviction policy, run every cycle before the fill.
    /// Tier A: out goes whatever falls in no interval of `window`.
    /// Tier B: past `budget_bytes`, out go the frames farthest from the playhead
    /// in timeline frames. Never by recency: during the fill the frame at the
    /// playhead is the least recent one.
    pub fn reconcile(&self, playhead: FrameIdx, window: &[WantedRange], budget_bytes: usize) {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        // Tier A.
        let entries = &mut inner.entries;
        let bytes_used = &mut inner.bytes_used;
        entries.retain(|&(media_id, idx), frame| {
            let keep = window
                .iter()
                .any(|w| w.media_id == media_id && w.contains(idx));
            if !keep {
                *bytes_used -= frame.byte_len();
            }
            keep
        });

        // Tier B.
        if inner.bytes_used > budget_bytes {
            let mut scored: Vec<((MediaId, FrameIdx), (u8, FrameIdx))> = inner
                .entries
                .keys()
                .map(|&(media_id, idx)| {
                    ((media_id, idx), eviction_rank(media_id, idx, playhead, window))
                })
                .collect();
            // Eviction works from the tail.
            scored.sort_unstable_by_key(|&(_, rank)| std::cmp::Reverse(rank));
            for (key, _) in scored {
                if inner.bytes_used <= budget_bytes {
                    break;
                }
                if let Some(frame) = inner.entries.remove(&key) {
                    inner.bytes_used -= frame.byte_len();
                }
            }
        }
    }

    /// All the wanted frames `start..=end` of the media are cached: all of
    /// them, or with a stride its multiples (none in the range: covered).
    /// Stops at the first missing one, without building the intervals.
    pub fn covers(&self, media_id: MediaId, start: FrameIdx, end: FrameIdx) -> bool {
        let inner = self.inner.lock().unwrap();
        let stride = inner.stride(media_id);
        let first_multiple = start + (-start).rem_euclid(stride);
        (first_multiple..=end)
            .step_by(stride as usize)
            .all(|idx| inner.entries.contains_key(&(media_id, idx)))
    }

    /// `covers` for a whole segment to show: one with no multiple of the
    /// stride in it (a freeze frame, a clip of a frame or two) still wants
    /// its first frame, or it would show nothing.
    pub fn covers_segment(&self, media_id: MediaId, start: FrameIdx, end: FrameIdx) -> bool {
        let stride = self.stride(media_id);
        let has_multiple = start + (-start).rem_euclid(stride) <= end;
        start > end
            || self.covers(media_id, start, end) && (has_multiple || self.contains(media_id, start))
    }

    /// Contiguous (inclusive) intervals currently cached for a media — for
    /// the "buffered" indicator in the UI. With a stride, the gaps it leaves
    /// do not split them.
    pub fn cached_ranges(&self, media_id: MediaId) -> Vec<(FrameIdx, FrameIdx)> {
        let inner = self.inner.lock().unwrap();
        let stride = inner.stride(media_id);
        let mut indices: Vec<FrameIdx> = inner
            .entries
            .keys()
            .filter(|&&(m, _)| m == media_id)
            .map(|&(_, idx)| idx)
            .collect();
        indices.sort_unstable();

        let mut ranges: Vec<(FrameIdx, FrameIdx)> = Vec::new();
        for idx in indices {
            match ranges.last_mut() {
                Some((_, end)) if idx <= *end + stride => *end = idx,
                _ => ranges.push((idx, idx)),
            }
        }
        ranges
    }
}

impl SharedInner {
    fn stride(&self, media_id: MediaId) -> FrameIdx {
        self.strides.get(&media_id).copied().unwrap_or(1)
    }
}

/// Eviction order, greatest first: outside the window, then behind the
/// playhead, then ahead, farthest first within each (in timeline frames).
/// Behind goes before ahead: measured both ways alike, during playback the
/// frames just played would push out the ones about to be.
fn eviction_rank(
    media_id: MediaId,
    idx: FrameIdx,
    playhead: FrameIdx,
    window: &[WantedRange],
) -> (u8, FrameIdx) {
    window
        .iter()
        .filter(|w| w.media_id == media_id && w.contains(idx))
        .map(|w| {
            let offset = w.timeline_position_of(idx) - playhead;
            if offset < 0 {
                (1, -offset)
            } else {
                (0, offset)
            }
        })
        .min()
        .unwrap_or((2, FrameIdx::MAX))
}

#[cfg(test)]
#[path = "tests/cache.rs"]
mod tests;
