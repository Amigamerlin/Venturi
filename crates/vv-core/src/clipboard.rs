//! Clips and media copied as text, so another Venturi instance (or another
//! project) can paste them: the payload carries the pool items and the
//! nested timelines they need, under its own ids.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::pool::{AddEntities, absorb};
use crate::{
    Clip, ClipSource, FrameIdx, MediaId, Project, Rational, Timeline, TimelineId, TrackKind,
};

const HEADER: &str = "venturi-clipboard/";
const VERSION: u32 = 1;

/// A copied clip. Id and group are reassigned on paste; the position
/// is relative to the leftmost of the copied clips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEntry {
    /// Origin track as V/A + number, not an absolute index: pasting
    /// from a compound clip with video tracks only into a
    /// video+audio timeline must land on V2, not on the track of index 2 (which there
    /// is audio).
    pub track_kind: TrackKind,
    pub track_number: usize,
    pub relative_start: FrameIdx,
    /// The clip as it was at copy time; id, position and group are reassigned
    /// on paste.
    pub clip: Clip,
    /// Fps of the origin timeline, in which `clip` and
    /// `relative_start` are expressed.
    pub timeline_fps: Rational,
    /// Clips with the same tag were in the same group at copy time.
    pub link_tag: Option<u64>,
}

/// Who copied: a paste from the same instance and project reuses the
/// copied ids as they are, so a compound clip is not duplicated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardOrigin {
    pub instance: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardPayload {
    pub origin: ClipboardOrigin,
    /// Pool items and nested timelines `media` and `clips` refer to.
    project: Project,
    /// Pool items copied from the media pool.
    media: Vec<MediaId>,
    clips: Vec<ClipboardEntry>,
}

#[derive(Debug)]
pub enum PayloadError {
    UnsupportedVersion(u32),
    Malformed(ron::error::SpannedError),
}

/// The content of a payload in the ids of the project it was merged into.
#[derive(Debug, Default)]
pub struct Pasted {
    pub media: Vec<MediaId>,
    pub clips: Vec<ClipboardEntry>,
}

impl ClipboardPayload {
    pub fn new(
        origin: ClipboardOrigin,
        source: &Project,
        media: &[MediaId],
        clips: &[ClipboardEntry],
    ) -> Self {
        let mut copier = Copier {
            source,
            fragment: Project::default(),
            media: HashMap::new(),
        };
        let media = media.iter().map(|&id| copier.media(id)).collect();
        let clips = clips
            .iter()
            .map(|entry| {
                let mut entry = entry.clone();
                copier.remap(&mut entry.clip);
                entry
            })
            .collect();
        Self {
            origin,
            project: copier.fragment,
            media,
            clips,
        }
    }

    pub fn has_clips(&self) -> bool {
        !self.clips.is_empty()
    }

    pub fn has_media(&self) -> bool {
        !self.media.is_empty()
    }

    pub fn to_text(&self) -> String {
        let body = ron::to_string(self).expect("payload serializes");
        format!("{HEADER}{VERSION}\n{body}")
    }

    /// `None` if `text` is not a payload at all.
    pub fn from_text(text: &str) -> Option<Result<Self, PayloadError>> {
        let (header, body) = text.strip_prefix(HEADER)?.split_once('\n')?;
        let version: u32 = header.trim().parse().ok()?;
        Some(if version == VERSION {
            ron::from_str(body).map_err(PayloadError::Malformed)
        } else {
            Err(PayloadError::UnsupportedVersion(version))
        })
    }

    /// Adds to `add` the pool items `project` lacks, at the root of the
    /// pool: a file already in the pool (same path or same content) is
    /// reused. Files are referenced where they are, never copied.
    pub fn merge_into(self, project: &mut Project, add: &mut AddEntities) -> Pasted {
        let Self {
            project: mut fragment,
            media,
            clips,
            ..
        } = self;
        let mut reuse: HashMap<MediaId, MediaId> = fragment
            .media_pool
            .iter()
            .filter(|(_, item)| item.compound.is_none())
            .filter_map(|(id, item)| {
                let existing = project
                    .media_pool
                    .iter()
                    .filter(|(_, m)| m.compound.is_none())
                    .find(|(_, m)| m.path == item.path)
                    .or_else(|| {
                        project.media_pool.iter().find(|(_, m)| {
                            m.compound.is_none()
                                && item.content_hash != 0
                                && m.content_hash == item.content_hash
                        })
                    })?;
                Some((id, existing.0))
            })
            .collect();
        // Offline at copy time: a fresh id keeps them offline here instead
        // of landing on whatever media has their old id.
        let referenced = clips
            .iter()
            .map(|e| &e.clip)
            .chain(
                fragment
                    .timelines
                    .values()
                    .flat_map(|t| t.tracks.iter().flat_map(|track| &track.clips)),
            )
            .filter_map(|clip| match clip.source {
                ClipSource::Media(id) => Some(id),
                _ => None,
            });
        for id in referenced.collect::<HashSet<_>>() {
            if !fragment.media_pool.contains_key(id) {
                reuse.insert(id, project.media_pool.alloc());
            }
        }
        rename_taken_compounds(project, &mut fragment);

        let (_, media_ids) = absorb(project, add, fragment, &reuse, None);
        let map = |id: MediaId| media_ids.get(&id).copied().unwrap_or(id);
        let clips = clips
            .into_iter()
            .map(|mut entry| {
                if let ClipSource::Media(id) = &mut entry.clip.source {
                    *id = map(*id);
                }
                entry
            })
            .collect();
        Pasted {
            media: media.into_iter().map(map).collect(),
            clips,
        }
    }
}

struct Copier<'a> {
    source: &'a Project,
    fragment: Project,
    media: HashMap<MediaId, MediaId>,
}

impl Copier<'_> {
    fn media(&mut self, id: MediaId) -> MediaId {
        if let Some(&copied) = self.media.get(&id) {
            return copied;
        }
        let Some(item) = self.source.media_pool.get(id) else {
            // Offline: kept dangling, `merge_into` gives it a fresh id.
            let copied = self.fragment.media_pool.alloc();
            self.media.insert(id, copied);
            return copied;
        };
        let copied = self.fragment.media_pool.alloc();
        self.media.insert(id, copied);
        let mut item = item.clone();
        item.folder = None;
        item.compound = item
            .compound
            .and_then(|t| Some(self.timeline(self.source.timelines.get(t)?.clone())));
        self.fragment.media_pool.insert_at(copied, item);
        copied
    }

    fn timeline(&mut self, mut timeline: Timeline) -> TimelineId {
        for clip in timeline.tracks.iter_mut().flat_map(|t| &mut t.clips) {
            self.remap(clip);
        }
        self.fragment.timelines.insert(timeline)
    }

    fn remap(&mut self, clip: &mut Clip) {
        if let ClipSource::Media(id) = &mut clip.source {
            *id = self.media(*id);
        }
    }
}

/// Compound clips are named after their timeline: a pasted "Compound Clip 1"
/// next to the project's own one becomes "Compound Clip 1 2".
fn rename_taken_compounds(project: &Project, fragment: &mut Project) {
    let mut taken: HashSet<std::path::PathBuf> = project
        .media_pool
        .values()
        .filter(|m| m.compound.is_some())
        .map(|m| m.path.clone())
        .collect();
    for item in fragment.media_pool.values_mut() {
        let Some(timeline) = item.compound.and_then(|t| fragment.timelines.get_mut(t)) else {
            continue;
        };
        if taken.contains(&item.path) {
            let base = item.path.to_string_lossy().into_owned();
            let name = (2..)
                .map(|n| format!("{base} {n}"))
                .find(|name| !taken.contains(std::path::Path::new(name)))
                .expect("infinite candidates");
            item.path = name.clone().into();
            timeline.name = name;
        }
        taken.insert(item.path.clone());
    }
}

#[cfg(test)]
#[path = "tests/clipboard.rs"]
mod tests;
