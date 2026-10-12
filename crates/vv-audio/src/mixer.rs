//! Continuous mixer of the timeline audio tracks: an immutable snapshot
//! of the clips (built by the UI thread) + `mix_range`, a pure function used
//! both by the cpal callback and by the export, so preview and export
//! produce the same mix.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vv_core::{
    AudioEffectKind, ChannelStrip, ClipId, ClipSource, Equalizer, FrameIdx, Keyframed, MediaId,
    MixerChannel, MultibandCompressor, NormalizeMode, Project, SetLevel, Timeline, TimelineId,
};

use crate::blocks::{Run, SampleBlocks};
use crate::dynamics::{BandActivity, MultibandProcessor};
use crate::effect::Effect;
use crate::eq::EqProcessor;
use crate::levels::{
    InputClip, LevelAnalysis, Measured, StableHasher, has_state, hash_channel, hash_inserts,
    input_clips, inputs_have_state, level_segments,
};
use crate::spectrum::SpectrumTap;

pub const PROJECT_SAMPLE_RATE: u32 = 48_000;
/// Control-rate (~60Hz) granularity of the keyframed gain, not sample-accurate.
pub const GAIN_BLOCK_FRAMES: u64 = 800;
/// The mix goes track by track through buffers of this many frames.
const MIX_BLOCK_FRAMES: usize = 512;

#[derive(Clone)]
pub struct MixClip {
    /// Matches the mix state of a compound clip across snapshots
    /// (`MixState::continue_from`), and a clip measured alone.
    pub clip: ClipId,
    /// In timeline audio frames (one sample per channel).
    pub start: u64,
    pub len: u64,
    /// Audio frame of `buffer` corresponding to `start`.
    pub source_offset: u64,
    /// Frames of `buffer` per timeline frame: the clip speed (varispeed),
    /// 1 when `buffer` is already stretched.
    pub step: f64,
    pub input: ClipInput,
    pub gain_db: Keyframed<f32>,
    /// Timeline index of the track: its entry in `MixSnapshot::tracks`.
    pub track: usize,
    pub clip_fps: f64,
    /// Audio frame of the media at `start` and media frames per timeline
    /// frame: where the gain keyframes are read, even from a stretched copy.
    pub media_offset: u64,
    pub media_step: f64,
    /// Fades, in timeline audio frames from the start/end of
    /// `len` (see `Clip::fade_multiplier_at`, same semantics on samples).
    pub fade_in: u64,
    pub fade_out: u64,
}

/// What a clip plays, its frames counted like `MixClip::source_offset`.
#[derive(Clone)]
pub enum ClipInput {
    /// Interleaved at the snapshot's `sample_rate`/`channels`: it may hold
    /// only the part read.
    Blocks(SampleBlocks),
    /// A compound clip's timeline, mixed live in its own time.
    Nested(NestedMix),
}

#[derive(Clone)]
pub struct NestedMix {
    pub mix: Arc<MixSnapshot>,
}

/// A mixer strip, ready for the mix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bus {
    pub gain: f32,
    /// Multipliers of the left and right channel.
    pub balance: [f32; 2],
}

impl Bus {
    pub const UNITY: Bus = Bus {
        gain: 1.0,
        balance: [1.0, 1.0],
    };

    pub fn from_strip(strip: &ChannelStrip) -> Self {
        Self {
            gain: db_to_linear(strip.gain_db),
            balance: balance_gains(strip.pan),
        }
    }

    /// Multiplier of output channel `c` of `channels`: the balance only
    /// touches the first two, and nothing in mono.
    fn channel_gain(&self, c: usize, channels: usize) -> f32 {
        if channels < 2 || c >= 2 {
            self.gain
        } else {
            self.gain * self.balance[c]
        }
    }
}

/// Balance, not a pan law: the centre keeps both channels at unity, moving
/// towards a side only attenuates the other one.
pub fn balance_gains(pan: f32) -> [f32; 2] {
    let pan = pan.clamp(-1.0, 1.0);
    [(1.0 - pan).min(1.0), (1.0 + pan).min(1.0)]
}

/// Peak of a left/right pair since the last `take`. Non-negative floats
/// order like their bits, so `fetch_max` works on them.
#[derive(Debug, Default)]
pub struct StereoPeak([AtomicU32; 2]);

impl StereoPeak {
    fn raise(&self, peak: [f32; 2]) {
        for (slot, value) in self.0.iter().zip(peak) {
            slot.fetch_max(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn take(&self) -> (f32, f32) {
        let [l, r] = &self.0;
        (
            f32::from_bits(l.swap(0, Ordering::Relaxed)),
            f32::from_bits(r.swap(0, Ordering::Relaxed)),
        )
    }
}

/// Post-fader peaks of what the output stream played.
#[derive(Debug, Default)]
pub struct MixMeters {
    /// Indexed like `MixSnapshot::tracks`.
    tracks: Vec<StereoPeak>,
    master: StereoPeak,
    /// Per channel and insert, like the chains of `MixSnapshot::tracks`.
    track_inserts: Vec<Vec<InsertMeter>>,
    master_inserts: Vec<InsertMeter>,
}

#[derive(Debug, Default)]
struct InsertMeter {
    bands: BandMeter,
    /// Compressors and equalizers only.
    spectrum: Option<Arc<SpectrumTap>>,
}

impl MixMeters {
    fn new(tracks: &[Channel], master: &Channel) -> Self {
        let inserts = |channel: &Channel| {
            channel
                .inserts
                .iter()
                .map(|insert| InsertMeter {
                    bands: BandMeter::default(),
                    spectrum: matches!(insert, Insert::Compressor(_) | Insert::Eq(_))
                        .then(|| Arc::new(SpectrumTap::default())),
                })
                .collect()
        };
        Self {
            tracks: tracks.iter().map(|_| StereoPeak::default()).collect(),
            master: StereoPeak::default(),
            track_inserts: tracks.iter().map(inserts).collect(),
            master_inserts: inserts(master),
        }
    }

    fn insert(&self, channel: MixerChannel, index: usize) -> Option<&InsertMeter> {
        match channel {
            MixerChannel::Track(track) => self.track_inserts.get(track)?.get(index),
            MixerChannel::Master => self.master_inserts.get(index),
        }
    }

    /// Takes over the spectra of `previous` where an effect is still there:
    /// a new snapshot for every edit would otherwise empty them all the time.
    pub fn keep_spectra_of(&mut self, previous: &MixMeters) {
        let pairs = self
            .track_inserts
            .iter_mut()
            .zip(&previous.track_inserts)
            .chain([(&mut self.master_inserts, &previous.master_inserts)]);
        for (chain, old_chain) in pairs {
            for (meter, old) in chain.iter_mut().zip(old_chain) {
                if let (Some(spectrum), Some(old)) = (&mut meter.spectrum, &old.spectrum) {
                    *spectrum = old.clone();
                }
            }
        }
    }

    /// Before and after the effect at `index` in the chain of `channel`.
    pub fn spectrum(&self, channel: MixerChannel, index: usize) -> Option<&SpectrumTap> {
        self.insert(channel, index)?.spectrum.as_deref()
    }

    pub fn track(&self, index: usize) -> Option<&StereoPeak> {
        self.tracks.get(index)
    }

    pub fn master(&self) -> &StereoPeak {
        &self.master
    }

    /// Of the effect at `index` in the chain of `channel`.
    pub fn band_meter(&self, channel: MixerChannel, index: usize) -> Option<&BandMeter> {
        Some(&self.insert(channel, index)?.bands)
    }

    fn track_inserts(&self, track: usize) -> &[InsertMeter] {
        self.track_inserts.get(track).map_or(&[], Vec::as_slice)
    }
}

/// Level and gain reduction of each band of a compressor since the last
/// `take`.
#[derive(Debug, Default)]
pub struct BandMeter {
    level: [AtomicU32; 3],
    reduction_db: [AtomicU32; 3],
}

impl BandMeter {
    fn raise(&self, activity: BandActivity) {
        let pairs = [
            (&self.level, activity.level),
            (&self.reduction_db, activity.reduction_db),
        ];
        for (slots, values) in pairs {
            for (slot, value) in slots.iter().zip(values) {
                slot.fetch_max(value.max(0.0).to_bits(), Ordering::Relaxed);
            }
        }
    }

    pub fn take(&self) -> BandActivity {
        let take = |slots: &[AtomicU32; 3]| {
            slots
                .each_ref()
                .map(|slot| f32::from_bits(slot.swap(0, Ordering::Relaxed)))
        };
        BandActivity {
            level: take(&self.level),
            reduction_db: take(&self.reduction_db),
        }
    }
}

/// An effect of a chain, ready for the mix: a normalization is the gain it
/// measured.
#[derive(Debug, Clone, PartialEq)]
pub enum Insert {
    /// A disabled effect, or one with nothing to act on: it keeps the
    /// inserts aligned with the effects of the strip.
    Off,
    Gain(f32),
    /// By timeline audio frame each gain starts at, ascending: one per clip
    /// of the track, held until the next one (the first also before it).
    ClipGains(Vec<(u64, f32)>),
    Compressor(MultibandCompressor),
    Mono,
    Eq(Equalizer),
}

/// A mixer strip, ready for the mix: its inserts, then fader and balance.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub inserts: Vec<Insert>,
    pub bus: Bus,
}

impl Channel {
    pub const UNITY: Channel = Channel {
        inserts: Vec::new(),
        bus: Bus::UNITY,
    };
}

impl Default for Channel {
    fn default() -> Self {
        Self::UNITY
    }
}

pub struct MixSnapshot {
    pub sample_rate: u32,
    pub channels: u16,
    /// Sorted by track.
    pub clips: Vec<MixClip>,
    /// Track and range of `clips` of each track with clips.
    groups: Vec<(usize, std::ops::Range<usize>)>,
    /// Indexed by timeline track index; a clip whose track is missing plays
    /// at unity.
    pub tracks: Vec<Channel>,
    pub master: Channel,
    pub meters: MixMeters,
}

impl MixSnapshot {
    pub fn empty(sample_rate: u32, channels: u16) -> Self {
        Self::new(
            sample_rate,
            channels,
            Vec::new(),
            Vec::new(),
            Channel::UNITY,
        )
    }

    pub fn new(
        sample_rate: u32,
        channels: u16,
        mut clips: Vec<MixClip>,
        tracks: Vec<Channel>,
        master: Channel,
    ) -> Self {
        clips.sort_by_key(|c| c.track);
        let mut groups: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        for (i, clip) in clips.iter().enumerate() {
            match groups.last_mut() {
                Some((track, range)) if *track == clip.track => range.end = i + 1,
                _ => groups.push((clip.track, i..i + 1)),
            }
        }
        let meters = MixMeters::new(&tracks, &master);
        Self {
            sample_rate,
            channels,
            clips,
            groups,
            tracks,
            master,
            meters,
        }
    }

    /// Clips whose audio is `Pending` are silent.
    pub fn from_timeline(
        project: &Project,
        timeline: &Timeline,
        sample_rate: u32,
        channels: u16,
        source: &mut impl AudioSource,
    ) -> Self {
        collect_clips(
            project,
            timeline,
            sample_rate,
            channels,
            source,
            None,
            0,
            0,
            None,
        )
        .0
    }

    /// Only for mixing the timeline audio frames `range`: the clips outside
    /// it are left out, and the others ask `AudioSource::file_frames` for
    /// the part they read.
    pub fn from_timeline_range(
        project: &Project,
        timeline: &Timeline,
        sample_rate: u32,
        channels: u16,
        source: &mut impl AudioSource,
        range: std::ops::Range<u64>,
    ) -> Self {
        collect_clips(
            project,
            timeline,
            sample_rate,
            channels,
            source,
            Some(range),
            0,
            0,
            None,
        )
        .0
    }

    /// What `measured` measures over `range` (all of it with `None`): its
    /// strip up to the normalization, alone and before the fader.
    pub fn measured(
        project: &Project,
        timeline: &Timeline,
        measured: Measured,
        sample_rate: u32,
        channels: u16,
        source: &mut impl AudioSource,
        range: Option<std::ops::Range<u64>>,
    ) -> Self {
        collect_clips(
            project,
            timeline,
            sample_rate,
            channels,
            source,
            range,
            0,
            0,
            Some(measured),
        )
        .0
    }

    fn track(&self, index: usize) -> &Channel {
        static UNITY: Channel = Channel::UNITY;
        self.tracks.get(index).unwrap_or(&UNITY)
    }
}

impl Insert {
    /// What runs it, if it keeps a state from one block to the next.
    fn effect(&self, sample_rate: u32, channels: u16) -> Option<Box<dyn Effect>> {
        match self {
            Insert::Off | Insert::Gain(_) | Insert::ClipGains(_) | Insert::Mono => None,
            Insert::Compressor(params) => Some(Box::new(MultibandProcessor::new(
                params,
                sample_rate,
                channels,
            ))),
            Insert::Eq(params) => Some(Box::new(EqProcessor::new(params, sample_rate, channels))),
        }
    }
}

/// The memory of the mix between two calls: the state of the effects that
/// have one, and where the previous call ended. A call that does not follow
/// on starts them over.
pub struct MixState {
    /// Aligned with the inserts of `MixSnapshot::tracks`.
    tracks: Vec<Vec<Option<Box<dyn Effect>>>>,
    master: Vec<Option<Box<dyn Effect>>>,
    /// Aligned with `MixSnapshot::clips`: the compound clips' own.
    nested: Vec<Option<NestedState>>,
    scratch: Vec<f32>,
    peaks: Vec<[f32; 2]>,
    next: Option<u64>,
}

impl MixState {
    pub fn new(snapshot: &MixSnapshot) -> Self {
        let (rate, channels) = (snapshot.sample_rate, snapshot.channels);
        let processors = |channel: &Channel| -> Vec<Option<Box<dyn Effect>>> {
            channel
                .inserts
                .iter()
                .map(|insert| insert.effect(rate, channels))
                .collect()
        };
        Self {
            tracks: snapshot.tracks.iter().map(processors).collect(),
            master: processors(&snapshot.master),
            nested: snapshot
                .clips
                .iter()
                .map(|clip| match &clip.input {
                    ClipInput::Nested(nested) => Some(NestedState::new(clip, nested)),
                    ClipInput::Blocks(_) => None,
                })
                .collect(),
            scratch: vec![0.0; MIX_BLOCK_FRAMES * snapshot.channels.max(1) as usize],
            peaks: vec![[0.0; 2]; snapshot.tracks.len()],
            next: None,
        }
    }

    /// Carries on from the state of the previous snapshot: the effects
    /// found at the same place keep their memory, so an edit does not
    /// restart them.
    pub fn continue_from(&mut self, previous: &MixState) {
        let pairs = self
            .tracks
            .iter_mut()
            .zip(&previous.tracks)
            .chain([(&mut self.master, &previous.master)]);
        for (chain, old_chain) in pairs {
            for (processor, old) in chain.iter_mut().zip(old_chain) {
                if let (Some(processor), Some(old)) = (processor, old) {
                    processor.take_memory_from(old.as_ref());
                }
            }
        }
        for nested in self.nested.iter_mut().flatten() {
            let old = previous
                .nested
                .iter()
                .flatten()
                .find(|old| old.key == nested.key);
            if let Some(old) = old {
                nested.continue_from(old);
            }
        }
        self.next = previous.next;
    }

    fn reset(&mut self) {
        for processor in self
            .tracks
            .iter_mut()
            .chain([&mut self.master])
            .flatten()
            .flatten()
        {
            processor.reset();
        }
        for nested in self.nested.iter_mut().flatten() {
            nested.rendered.clear();
        }
    }
}

/// The live mix of a compound clip: its timeline's state, and the frames
/// rendered last, which a varispeed read interpolates across calls.
struct NestedState {
    /// Track and clip, to find it again in the next snapshot.
    key: (usize, ClipId),
    state: MixState,
    rendered: Vec<f32>,
    /// Nested frame of `rendered[0]`.
    from: u64,
    channels: usize,
}

impl NestedState {
    fn new(clip: &MixClip, nested: &NestedMix) -> Self {
        let ch = nested.mix.channels.max(1) as usize;
        // What one mixer block reads, so the callback never allocates.
        let frames = (MIX_BLOCK_FRAMES as f64 * clip.step).ceil() as usize + clip.step as usize + 3;
        Self {
            key: (clip.track, clip.clip),
            state: MixState::new(&nested.mix),
            rendered: Vec::with_capacity(frames * ch),
            from: 0,
            channels: ch,
        }
    }

    /// Within the capacity: no allocations, it runs in the callback.
    fn continue_from(&mut self, previous: &NestedState) {
        self.state.continue_from(&previous.state);
        if self.channels != previous.channels {
            return;
        }
        let ch = self.channels;
        let kept = previous
            .rendered
            .len()
            .min(self.rendered.capacity() / ch * ch);
        self.rendered.clear();
        self.rendered.extend_from_slice(&previous.rendered[..kept]);
        self.from = previous.from;
        self.state.next = Some(self.from + (kept / ch) as u64);
    }

    /// The nested mix over `frames`, rendered in order: what was already
    /// rendered of them is kept, so the state stays sequential.
    fn render(&mut self, mix: &MixSnapshot, frames: std::ops::Range<u64>) -> Rendered<'_> {
        let ch = self.channels;
        let end = self.from + (self.rendered.len() / ch) as u64;
        if frames.start < self.from || frames.start > end {
            self.rendered.clear();
            self.from = frames.start;
        } else {
            let drop = (frames.start - self.from) as usize * ch;
            self.rendered.copy_within(drop.., 0);
            self.rendered.truncate(self.rendered.len() - drop);
            self.from = frames.start;
        }
        let have = self.from + (self.rendered.len() / ch) as u64;
        if frames.end > have {
            let old = self.rendered.len();
            self.rendered
                .resize(old + (frames.end - have) as usize * ch, 0.0);
            mix_into(mix, &mut self.state, have, &mut self.rendered[old..], false);
        }
        Rendered {
            samples: &self.rendered[..(frames.end - self.from) as usize * ch],
            from: self.from,
            channels: ch,
        }
    }
}

/// The frames a clip reads, from a file's blocks or a nested mix.
trait Frames {
    fn start(&self) -> u64;
    fn end(&self) -> u64;
    fn run(&self, frame: u64) -> Run<'_>;
    fn frame(&self, frame: u64) -> Option<&[f32]>;
}

impl Frames for SampleBlocks {
    fn start(&self) -> u64 {
        SampleBlocks::start(self)
    }

    fn end(&self) -> u64 {
        SampleBlocks::end(self)
    }

    fn run(&self, frame: u64) -> Run<'_> {
        SampleBlocks::run(self, frame)
    }

    fn frame(&self, frame: u64) -> Option<&[f32]> {
        SampleBlocks::frame(self, frame)
    }
}

struct Rendered<'a> {
    samples: &'a [f32],
    from: u64,
    channels: usize,
}

impl Frames for Rendered<'_> {
    fn start(&self) -> u64 {
        self.from
    }

    fn end(&self) -> u64 {
        self.from + (self.samples.len() / self.channels) as u64
    }

    fn run(&self, frame: u64) -> Run<'_> {
        if frame < self.from || frame >= self.end() {
            return Run::End;
        }
        Run::Samples(&self.samples[(frame - self.from) as usize * self.channels..])
    }

    fn frame(&self, frame: u64) -> Option<&[f32]> {
        match self.run(frame) {
            Run::Samples(samples) => Some(&samples[..self.channels]),
            Run::Silence(_) | Run::End => None,
        }
    }
}

/// What a normalization measures: while a new reading is on its way, the
/// same slot keeps playing with its last one. The channel, the position of
/// the normalization in its chain and the clip of the track (0 for the
/// whole master).
pub type AnalysisSlot = (TimelineId, MixerChannel, usize, usize);

pub(crate) fn hash_insert(insert: &Insert, h: &mut impl Hasher) {
    match insert {
        Insert::Off => 2u8.hash(h),
        Insert::Mono => 3u8.hash(h),
        Insert::Gain(gain) => (0u8, gain.to_bits()).hash(h),
        Insert::ClipGains(gains) => {
            5u8.hash(h);
            for (start, gain) in gains {
                (start, gain.to_bits()).hash(h);
            }
        }
        Insert::Compressor(params) => {
            (1u8, params.crossovers_hz.map(f32::to_bits)).hash(h);
            for band in &params.bands {
                let fields = [
                    band.threshold_db,
                    band.ratio,
                    band.attack_ms,
                    band.release_ms,
                    band.makeup_db,
                ];
                fields.map(f32::to_bits).hash(h);
            }
        }
        Insert::Eq(params) => {
            4u8.hash(h);
            for band in &params.bands {
                (band.enabled, band.shape as u8).hash(h);
                [band.freq_hz, band.gain_db, band.q]
                    .map(f32::to_bits)
                    .hash(h);
            }
        }
    }
}

pub enum LevelReading {
    Ready(f32),
    /// Being measured; the last reading of the same slot, if any.
    Pending(Option<f32>),
}

/// A clip's audio, as an `AudioSource` answers it.
pub enum ClipAudio {
    /// Already at the snapshot's `sample_rate`/`channels`.
    Ready(SampleBlocks),
    /// Like `Ready`, but with blocks still decoding: it plays, yet a
    /// compound containing it is not cached.
    Partial(SampleBlocks),
    /// Still decoding, nothing yet: silent for now, and so is a compound
    /// containing it.
    Pending,
    /// No audio there (e.g. a stream index past the file's streams).
    Missing,
}

/// The decoded audio a mix is built from. Compound clips never reach `file`:
/// they are mixed live from their nested timeline, or stretched from its
/// mixdown (`compound_mixdown`) when pitch-corrected.
pub trait AudioSource {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio;

    /// `file` when only its frames `frames` are read: the buffer holds at
    /// least those, or stops at the end of the file. `plays_at`: about when
    /// the first of them plays, in samples of the timeline being mixed (of
    /// the outermost one, for a compound clip's).
    fn file_frames(
        &mut self,
        path: &Path,
        stream: usize,
        _frames: std::ops::Range<u64>,
        _plays_at: u64,
    ) -> ClipAudio {
        self.file(path, stream)
    }

    /// A mixdown kept by `store_compound`, if still valid for `content_hash`.
    fn cached_compound(&mut self, _media_id: MediaId, _content_hash: u64) -> Option<Arc<Vec<f32>>> {
        None
    }

    fn store_compound(&mut self, _media_id: MediaId, _content_hash: u64, _mixdown: Arc<Vec<f32>>) {}

    /// Frames `range` of `buffer` time-stretched by `tempo`, pitch
    /// preserved, starting at frame 0. Synchronous here; the preview answers
    /// `Pending` while it works in the background.
    fn stretched(
        &mut self,
        buffer: &SampleBlocks,
        range: std::ops::Range<u64>,
        tempo: f64,
        sample_rate: u32,
        channels: u16,
    ) -> ClipAudio {
        stretch_range(buffer, range, tempo, sample_rate, channels).map_or(ClipAudio::Missing, |b| {
            ClipAudio::Ready(SampleBlocks::contiguous(Arc::new(b), 0, channels))
        })
    }

    /// Synchronous here, whole and from this source; the preview and the
    /// export measure from the files, by segment.
    fn level(&mut self, analysis: LevelAnalysis<'_>) -> LevelReading
    where
        Self: Sized,
    {
        LevelReading::Ready(analysis.measure_with(self))
    }
}

pub fn stretch_range(
    buffer: &SampleBlocks,
    range: std::ops::Range<u64>,
    tempo: f64,
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<f32>, String> {
    let range = range.start.max(buffer.start())..range.end.min(buffer.end());
    crate::stretch_samples(&buffer.to_vec(range), sample_rate, channels, tempo)
}

/// `None` is `Missing`; no compound cache.
impl<F: FnMut(&Path, usize) -> Option<SampleBlocks>> AudioSource for F {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self(path, stream).map_or(ClipAudio::Missing, ClipAudio::Ready)
    }
}

/// How much of a timeline's audio was ready.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Readiness {
    Complete,
    Partial,
    Pending,
    /// A compound clip containing itself: the whole cycle stays silent
    /// instead of layering itself `MAX_COMPOUND_DEPTH` times.
    Cyclic,
}

/// The mix clips of `timeline`, and the least ready of them. `placement`:
/// where its sample 0 plays in the outermost timeline, for `plays_at`.
#[allow(clippy::too_many_arguments)]
fn collect_clips(
    project: &Project,
    timeline: &Timeline,
    sample_rate: u32,
    channels: u16,
    source: &mut impl AudioSource,
    range: Option<std::ops::Range<u64>>,
    depth: u32,
    placement: i64,
    measuring: Option<Measured>,
) -> (MixSnapshot, Readiness) {
    let fps = timeline.fps.as_f64().max(1e-9);
    let mut clips = Vec::new();
    let mut readiness = Readiness::Complete;
    for (track_index, track) in timeline.audible_tracks() {
        for clip in track
            .clips
            .iter()
            .filter(|c| !c.disabled && c.freeze.is_none())
        {
            if let Some(Measured {
                channel: MixerChannel::Track(measured_track),
                clip: measured_clip,
                ..
            }) = measuring
                && (track_index != measured_track || measured_clip.is_some_and(|id| id != clip.id))
            {
                continue;
            }
            let ClipSource::Media(media_id) = &clip.source else {
                continue;
            };
            let Some(item) = project.media_pool.get(*media_id) else {
                continue;
            };
            let pitch_corrected = clip.pitch_correction && !clip.speed().is_one();
            let span = ClipSpan::new(clip, fps, sample_rate);
            // The stretch starts from the start of the clip.
            let mixed = range.as_ref().filter(|_| !pitch_corrected);
            let Some(needed) = span.media_frames(mixed) else {
                continue;
            };
            let into_clip = needed.start.saturating_sub(span.source_offset) as f64 / span.step;
            let plays_at = (placement + span.start as i64 + into_clip as i64).max(0) as u64;
            let nested_placement = placement + span.start as i64 - span.source_offset as i64;
            let input = if item.compound.is_some() && !pitch_corrected {
                let Some((mix, nested)) = nested_mix(
                    project,
                    *media_id,
                    (sample_rate, channels),
                    source,
                    range.is_some().then(|| needed.clone()),
                    depth,
                    nested_placement,
                ) else {
                    continue;
                };
                readiness = readiness.max(nested);
                if nested == Readiness::Cyclic {
                    continue;
                }
                ClipInput::Nested(NestedMix { mix: Arc::new(mix) })
            } else {
                let audio = if item.compound.is_some() {
                    // Stretched from its whole mixdown, until the stretch
                    // reads a live render (plans/AUDIO_LIVE_MIX.md step 4).
                    compound_audio(
                        project,
                        *media_id,
                        sample_rate,
                        channels,
                        source,
                        depth,
                        nested_placement,
                    )
                } else {
                    source.file_frames(
                        &item.path,
                        clip.audio_stream_index,
                        needed.clone(),
                        plays_at,
                    )
                };
                match audio {
                    ClipAudio::Ready(buffer) => ClipInput::Blocks(buffer),
                    // Stretching half a buffer would have to be redone anyway.
                    ClipAudio::Partial(_) if pitch_corrected => {
                        readiness = readiness.max(Readiness::Pending);
                        continue;
                    }
                    ClipAudio::Partial(buffer) => {
                        readiness = readiness.max(Readiness::Partial);
                        ClipInput::Blocks(buffer)
                    }
                    ClipAudio::Pending => {
                        readiness = readiness.max(Readiness::Pending);
                        continue;
                    }
                    ClipAudio::Missing => continue,
                }
            };
            let clip_fps = item.meta.fps.as_f64().max(1e-9);
            let readable_to = match &input {
                ClipInput::Blocks(buffer) => (buffer.end() < needed.end).then_some(buffer.end()),
                ClipInput::Nested(_) => None,
            };
            let Some(mut mix_clip) = mix_clip_from(clip, &span, clip_fps, input, readable_to)
            else {
                continue;
            };
            mix_clip.track = track_index;
            if pitch_corrected && let ClipInput::Blocks(buffer) = &mix_clip.input {
                let from = mix_clip.source_offset;
                let range = from..from + (mix_clip.len as f64 * mix_clip.step).ceil() as u64;
                match source.stretched(buffer, range, mix_clip.step, sample_rate, channels) {
                    ClipAudio::Ready(stretched) => {
                        mix_clip.len = mix_clip.len.min(stretched.end());
                        mix_clip.input = ClipInput::Blocks(stretched);
                        mix_clip.source_offset = 0;
                        mix_clip.step = 1.0;
                    }
                    ClipAudio::Pending => {
                        readiness = readiness.max(Readiness::Pending);
                        continue;
                    }
                    ClipAudio::Partial(_) | ClipAudio::Missing => continue,
                }
            }
            clips.push(mix_clip);
        }
    }
    let timeline_id = project
        .timelines
        .iter()
        .find(|(_, t)| std::ptr::eq(*t, timeline))
        .map(|(id, _)| id);
    let slot = |channel, insert, clip| timeline_id.map(|id| (id, channel, insert, clip));
    let inputs = input_clips(project, timeline, sample_rate);
    let measured_channel = measuring.map(|m| m.channel);
    let format = (sample_rate, channels);
    let mut tracks: Vec<Channel> = Vec::with_capacity(timeline.tracks.len());
    for (index, track) in timeline.tracks.iter().enumerate() {
        let channel = MixerChannel::Track(index);
        if matches!(measured_channel, Some(MixerChannel::Track(t)) if t != index) {
            tracks.push(Channel::UNITY);
            continue;
        }
        let stop = measuring.filter(|m| m.channel == channel).map(|m| m.insert);
        let track_inputs: Vec<&InputClip> = inputs.iter().filter(|c| c.track == index).collect();
        let mut track_clips: Vec<(ClipId, u64)> =
            track_inputs.iter().map(|c| (c.id, c.start)).collect();
        track_clips.sort_by_key(|&(_, start)| start);
        let analyze = |inserts: &[Insert], clip: Option<(usize, ClipId)>, mode| {
            let input: Vec<&InputClip> = match clip {
                Some((_, id)) => track_inputs
                    .iter()
                    .copied()
                    .filter(|c| c.id == id)
                    .collect(),
                None => track_inputs.clone(),
            };
            let stateful = has_state(inserts) || inputs_have_state(&input);
            let segments =
                level_segments(&input, chain_key(mode, format, inserts), |_| 0, stateful);
            source.level(LevelAnalysis {
                slot: slot(channel, inserts.len(), clip.map_or(0, |(i, _)| i)),
                project,
                timeline,
                measured: Measured {
                    channel,
                    insert: inserts.len(),
                    clip: clip.map(|(_, id)| id),
                },
                mode,
                sample_rate,
                channels,
                segments,
            })
        };
        let inserts = resolve_chain(
            &track.mix,
            stop,
            !track_inputs.is_empty(),
            &track_clips,
            analyze,
            &mut readiness,
        );
        // Measured before the fader.
        let bus = match stop {
            Some(_) => Bus::UNITY,
            None => Bus::from_strip(&track.mix),
        };
        tracks.push(Channel { inserts, bus });
    }
    let master = if matches!(measured_channel, Some(MixerChannel::Track(_))) {
        Channel::UNITY
    } else {
        let stop = measuring.map(|m| m.insert);
        let all: Vec<&InputClip> = inputs.iter().collect();
        let tracks_have_state = all.iter().any(|clip| {
            tracks
                .get(clip.track)
                .is_some_and(|t| has_state(&t.inserts))
        });
        let analyze = |inserts: &[Insert], _: Option<(usize, ClipId)>, mode| {
            let stateful = has_state(inserts) || tracks_have_state || inputs_have_state(&all);
            let track_key = |track: usize| {
                let mut h = StableHasher::default();
                if let Some(channel) = tracks.get(track) {
                    hash_channel(channel, &mut h);
                }
                h.finish()
            };
            let segments =
                level_segments(&all, chain_key(mode, format, inserts), track_key, stateful);
            source.level(LevelAnalysis {
                slot: slot(MixerChannel::Master, inserts.len(), 0),
                project,
                timeline,
                measured: Measured {
                    channel: MixerChannel::Master,
                    insert: inserts.len(),
                    clip: None,
                },
                mode,
                sample_rate,
                channels,
                segments,
            })
        };
        let inserts = resolve_chain(
            &timeline.master,
            stop,
            !all.is_empty(),
            &[],
            analyze,
            &mut readiness,
        );
        let bus = match stop {
            Some(_) => Bus::UNITY,
            None => Bus::from_strip(&timeline.master),
        };
        Channel { inserts, bus }
    };
    let snapshot = MixSnapshot::new(sample_rate, channels, clips, tracks, master);
    (snapshot, readiness)
}

/// What every segment of a normalization shares: the mode, the format and
/// the effects before it.
fn chain_key(mode: NormalizeMode, (sample_rate, channels): (u32, u16), inserts: &[Insert]) -> u64 {
    let mut h = StableHasher::default();
    (mode as u8, sample_rate, channels).hash(&mut h);
    hash_inserts(inserts, &mut h);
    h.finish()
}

/// The effects of `strip`, in order, one insert each, up to `stop`. A
/// normalization measures the channel through the effects before it
/// (`analyze`), so it stays right after a compressor too. `clips`: a
/// track's, by start, measured one by one when its normalization asks so;
/// the master is measured whole.
fn resolve_chain(
    strip: &ChannelStrip,
    stop: Option<usize>,
    has_audio: bool,
    clips: &[(ClipId, u64)],
    mut analyze: impl FnMut(&[Insert], Option<(usize, ClipId)>, NormalizeMode) -> LevelReading,
    readiness: &mut Readiness,
) -> Vec<Insert> {
    let mut inserts = Vec::new();
    for effect in strip.effects.iter().take(stop.unwrap_or(usize::MAX)) {
        if !effect.enabled {
            inserts.push(Insert::Off);
            continue;
        }
        let insert = match &effect.kind {
            AudioEffectKind::Normalize { .. } if !has_audio => Insert::Off,
            &AudioEffectKind::Normalize {
                target_db,
                mode,
                set_level,
            } => {
                let mut gain = |clip: Option<(usize, ClipId)>| {
                    let reading = analyze(&inserts, clip, mode);
                    normalize_gain(target_db, reading, readiness)
                };
                // The loudest peak of the clips is the peak of the track:
                // measured in one go.
                let per_clip =
                    !clips.is_empty() && (set_level == SetLevel::Independent || mode.is_loudness());
                if !per_clip {
                    Insert::Gain(gain(None).unwrap_or(1.0))
                } else {
                    let gains: Vec<Option<f32>> = clips
                        .iter()
                        .enumerate()
                        .map(|(i, &(id, _))| gain(Some((i, id))))
                        .collect();
                    match set_level {
                        SetLevel::Relative => Insert::Gain(
                            gains.into_iter().flatten().reduce(f32::min).unwrap_or(1.0),
                        ),
                        SetLevel::Independent => Insert::ClipGains(
                            clips
                                .iter()
                                .zip(gains)
                                .map(|(&(_, start), gain)| (start, gain.unwrap_or(1.0)))
                                .collect(),
                        ),
                    }
                }
            }
            AudioEffectKind::MultibandCompressor(params) => Insert::Compressor(params.clone()),
            AudioEffectKind::Mono => Insert::Mono,
            AudioEffectKind::Equalizer(params) => Insert::Eq(params.clone()),
        };
        inserts.push(insert);
    }
    inserts
}

/// A reading still on its way leaves the mix `Partial`, so a compound
/// containing it is not cached with the wrong gain. `None` for silence,
/// which stays silence instead of getting the maximum gain.
fn normalize_gain(target_db: f32, reading: LevelReading, readiness: &mut Readiness) -> Option<f32> {
    let level = match reading {
        LevelReading::Ready(level) => Some(level),
        LevelReading::Pending(last) => {
            *readiness = (*readiness).max(Readiness::Partial);
            last
        }
    };
    level
        .filter(|level| *level > 1e-6)
        .map(|level| (db_to_linear(target_db) / level).min(db_to_linear(vv_core::GAIN_DB_MAX)))
}

/// The mixdown of the nested timeline of compound clip `media_id`, at
/// `sample_rate`/`channels`. `Pending` while a clip in it (at any depth) has
/// nothing yet, `Partial` while one is still decoding: only a complete one is
/// cached, or the missing piece would stay missing until `content_hash`
/// changes again.
pub fn compound_mixdown(
    project: &Project,
    media_id: MediaId,
    sample_rate: u32,
    channels: u16,
    source: &mut impl AudioSource,
) -> ClipAudio {
    compound_audio(project, media_id, sample_rate, channels, source, 0, 0)
}

fn compound_audio(
    project: &Project,
    media_id: MediaId,
    sample_rate: u32,
    channels: u16,
    source: &mut impl AudioSource,
    depth: u32,
    placement: i64,
) -> ClipAudio {
    let Some(item) = project.media_pool.get(media_id) else {
        return ClipAudio::Missing;
    };
    let blocks = |mixdown| SampleBlocks::contiguous(mixdown, 0, channels);
    if let Some(mixdown) = source.cached_compound(media_id, item.content_hash) {
        return ClipAudio::Ready(blocks(mixdown));
    }
    let Some((snapshot, readiness)) = nested_mix(
        project,
        media_id,
        (sample_rate, channels),
        source,
        None,
        depth,
        placement,
    ) else {
        return ClipAudio::Missing;
    };
    if readiness >= Readiness::Pending {
        return ClipAudio::Pending;
    }
    let len = snapshot
        .clips
        .iter()
        .map(|c| c.start + c.len)
        .max()
        .unwrap_or(0);
    let mut mixdown = vec![0.0f32; len as usize * channels.max(1) as usize];
    mix_range(&snapshot, 0, &mut mixdown);
    let mixdown = Arc::new(mixdown);
    if readiness == Readiness::Partial {
        return ClipAudio::Partial(blocks(mixdown));
    }
    source.store_compound(media_id, item.content_hash, mixdown.clone());
    ClipAudio::Ready(blocks(mixdown))
}

/// The nested timeline of compound clip `media_id`, ready to be mixed in
/// its own time; `frames`: the part of it read, `None` for all of it.
/// `None` if it has no timeline.
fn nested_mix(
    project: &Project,
    media_id: MediaId,
    (sample_rate, channels): (u32, u16),
    source: &mut impl AudioSource,
    frames: Option<std::ops::Range<u64>>,
    depth: u32,
    placement: i64,
) -> Option<(MixSnapshot, Readiness)> {
    if depth >= vv_core::MAX_COMPOUND_DEPTH {
        return Some((MixSnapshot::empty(sample_rate, channels), Readiness::Cyclic));
    }
    let item = project.media_pool.get(media_id)?;
    let nested = item.compound.and_then(|id| project.timelines.get(id))?;
    Some(collect_clips(
        project,
        nested,
        sample_rate,
        channels,
        source,
        frames,
        depth + 1,
        placement,
        None,
    ))
}

/// Where a clip plays, in audio frames, before its buffer is known.
pub(crate) struct ClipSpan {
    pub start: u64,
    pub len: u64,
    /// Frame of the media at `start`.
    pub source_offset: u64,
    pub step: f64,
    pub fade_in: u64,
    pub fade_out: u64,
}

impl ClipSpan {
    pub(crate) fn new(clip: &vv_core::Clip, timeline_fps: f64, sample_rate: u32) -> Self {
        let frames = |timeline_frames: FrameIdx| {
            seconds_to_frames(timeline_frames as f64 / timeline_fps, sample_rate)
        };
        Self {
            start: timeline_frame_to_sample(clip.timeline_start, timeline_fps, sample_rate),
            len: frames(clip.timeline_len),
            source_offset: seconds_to_frames(
                clip.media_secs_at(clip.timeline_start, timeline_fps),
                sample_rate,
            ),
            step: clip.speed().as_f64(),
            fade_in: frames(clip.fade_in),
            fade_out: frames(clip.fade_out),
        }
    }

    /// The media frames a mix of `range` (all of it without one) reads,
    /// `None` if none. Past the end of the file they are enough to place
    /// the fade-out where the whole file would.
    fn media_frames(&self, range: Option<&std::ops::Range<u64>>) -> Option<std::ops::Range<u64>> {
        let end = self.start + self.len;
        let (from, to) = match range {
            None => (self.start, end),
            Some(r) if r.end <= self.start || r.start >= end => return None,
            Some(r) => (
                r.start.max(self.start),
                r.end.saturating_add(self.fade_out).min(end),
            ),
        };
        if from >= to {
            return None;
        }
        let media = |f: u64| (f - self.start) as f64 * self.step;
        // A varispeed read looks up to `step` frames ahead.
        let lookahead = self.step.ceil() as u64 + 1;
        Some(
            self.source_offset + media(from).floor() as u64
                ..self.source_offset + media(to).ceil() as u64 + lookahead,
        )
    }
}

/// `None` if the buffer does not cover even one sample of the clip.
/// `readable_to`: where the file ends, if the buffer reaches it before the
/// frames the clip reads.
fn mix_clip_from(
    clip: &vv_core::Clip,
    span: &ClipSpan,
    clip_fps: f64,
    input: ClipInput,
    readable_to: Option<u64>,
) -> Option<MixClip> {
    if let ClipInput::Blocks(buffer) = &input
        && buffer.is_empty()
    {
        return None;
    }
    let step = span.step;
    let len = match readable_to {
        None => span.len,
        Some(file_end) => {
            let source_offset = span.source_offset.min(file_end);
            // The last frame read is interpolated with the one after it.
            let readable = ((file_end - source_offset) as f64 / step).floor() as u64;
            span.len
                .min(readable.saturating_sub(u64::from(step != 1.0)))
        }
    };
    if len == 0 {
        return None;
    }
    Some(MixClip {
        clip: clip.id,
        start: span.start,
        len,
        source_offset: span.source_offset,
        step,
        input,
        gain_db: clip.effects.gain_db.clone(),
        track: 0,
        clip_fps,
        media_offset: span.source_offset,
        media_step: step,
        fade_in: span.fade_in.min(len),
        fade_out: span.fade_out.min(len),
    })
}

fn seconds_to_frames(secs: f64, sample_rate: u32) -> u64 {
    (secs.max(0.0) * sample_rate as f64).round() as u64
}

pub fn timeline_frame_to_sample(frame: FrameIdx, fps: f64, sample_rate: u32) -> u64 {
    seconds_to_frames(frame as f64 / fps.max(1e-9), sample_rate)
}

pub fn sample_to_timeline_frame(sample: u64, fps: f64, sample_rate: u32) -> FrameIdx {
    (sample as f64 / sample_rate as f64 * fps).floor() as FrameIdx
}

/// Writes into `out` (interleaved, `snapshot.channels` channels) the mix
/// starting from timeline audio frame `start`, with the effects starting
/// from scratch.
pub fn mix_range(snapshot: &MixSnapshot, start: u64, out: &mut [f32]) {
    mix_into(snapshot, &mut MixState::new(snapshot), start, out, false);
}

/// `mix_range` carrying on the effects from the previous call through
/// `state`: a mix split into calls equals a single one.
pub fn mix_range_from(snapshot: &MixSnapshot, state: &mut MixState, start: u64, out: &mut [f32]) {
    mix_into(snapshot, state, start, out, false);
}

/// `mix_range` for the output stream: the effects carry on from the
/// previous call through `state`, and `snapshot.meters` go up. No
/// allocations: it runs in the realtime callback.
pub fn mix_range_metered(
    snapshot: &MixSnapshot,
    state: &mut MixState,
    start: u64,
    out: &mut [f32],
) {
    mix_into(snapshot, state, start, out, true);
}

fn mix_into(
    snapshot: &MixSnapshot,
    state: &mut MixState,
    start: u64,
    out: &mut [f32],
    metered: bool,
) {
    out.fill(0.0);
    let ch = snapshot.channels as usize;
    if ch == 0 {
        return;
    }
    let frames = out.len() / ch;
    if state.next != Some(start) {
        state.reset();
    }
    state.next = Some(start + frames as u64);
    state.peaks.fill([0.0; 2]);
    let mut master_peak = [0.0f32; 2];
    for block_start in (0..frames).step_by(MIX_BLOCK_FRAMES) {
        let len = MIX_BLOCK_FRAMES.min(frames - block_start);
        let block = &mut out[block_start * ch..(block_start + len) * ch];
        let block_time = start + block_start as u64;
        for (track, range) in &snapshot.groups {
            let channel = snapshot.track(*track);
            let scratch = &mut state.scratch[..len * ch];
            scratch.fill(0.0);
            for (index, clip) in range.clone().zip(&snapshot.clips[range.clone()]) {
                let nested = state.nested.get_mut(index).and_then(Option::as_mut);
                add_clip(snapshot, clip, nested, block_time, scratch);
            }
            let processors = state.tracks.get_mut(*track).map(Vec::as_mut_slice);
            let meters = metered.then(|| snapshot.meters.track_inserts(*track));
            apply_inserts(
                &channel.inserts,
                processors.unwrap_or_default(),
                scratch,
                ch,
                block_time,
                meters,
            );
            let mut peak = [0.0f32; 2];
            for (dst, src) in block.chunks_exact_mut(ch).zip(scratch.chunks_exact(ch)) {
                for (c, (d, s)) in dst.iter_mut().zip(src).enumerate() {
                    let value = s * channel.bus.channel_gain(c, ch);
                    *d += value;
                    if c < 2 {
                        peak[c] = peak[c].max(value.abs());
                    }
                }
            }
            if let Some(track_peak) = state.peaks.get_mut(*track) {
                *track_peak = [track_peak[0].max(peak[0]), track_peak[1].max(peak[1])];
            }
        }
        let meters = metered.then_some(snapshot.meters.master_inserts.as_slice());
        apply_inserts(
            &snapshot.master.inserts,
            &mut state.master,
            block,
            ch,
            block_time,
            meters,
        );
        for frame in block.chunks_exact_mut(ch) {
            for (c, s) in frame.iter_mut().enumerate() {
                *s *= snapshot.master.bus.channel_gain(c, ch);
                if c < 2 {
                    master_peak[c] = master_peak[c].max(s.abs());
                }
            }
        }
    }
    if metered {
        for (track, peak) in state.peaks.iter().enumerate() {
            if let Some(meter) = snapshot.meters.track(track) {
                meter.raise(mono_as_stereo(*peak, ch));
            }
        }
        snapshot
            .meters
            .master
            .raise(mono_as_stereo(master_peak, ch));
    }
}

/// `samples` start at timeline audio frame `time`. `meters`: where the
/// compressors report, if metered.
fn apply_inserts(
    inserts: &[Insert],
    processors: &mut [Option<Box<dyn Effect>>],
    samples: &mut [f32],
    channels: usize,
    time: u64,
    meters: Option<&[InsertMeter]>,
) {
    for (index, insert) in inserts.iter().enumerate() {
        match insert {
            Insert::Off => {}
            Insert::Gain(gain) => samples.iter_mut().for_each(|s| *s *= gain),
            Insert::ClipGains(gains) => apply_clip_gains(gains, time, samples, channels),
            Insert::Mono => {
                for frame in samples.chunks_exact_mut(channels.max(1)) {
                    let average = frame.iter().sum::<f32>() / frame.len() as f32;
                    frame.fill(average);
                }
            }
            Insert::Compressor(_) | Insert::Eq(_) => {
                let Some(Some(processor)) = processors.get_mut(index) else {
                    continue;
                };
                let meter = meters.and_then(|m| m.get(index));
                let spectrum = meter.and_then(|m| m.spectrum.as_deref());
                if let Some(spectrum) = spectrum {
                    spectrum.record_pre(samples, channels);
                }
                let activity = processor.process(samples);
                if let (Some(meter), Some(activity)) = (meter, activity) {
                    meter.bands.raise(activity);
                }
                if let Some(spectrum) = spectrum {
                    spectrum.record_post(samples, channels);
                }
            }
        }
    }
}

fn apply_clip_gains(gains: &[(u64, f32)], time: u64, samples: &mut [f32], channels: usize) {
    if gains.is_empty() {
        return;
    }
    let ch = channels.max(1);
    let frames = samples.len() / ch;
    let mut from = 0;
    while from < frames {
        let next = gains.partition_point(|(start, _)| *start <= time + from as u64);
        let gain = gains[next.saturating_sub(1)].1;
        let to = gains
            .get(next)
            .map_or(frames, |(start, _)| ((start - time) as usize).min(frames));
        samples[from * ch..to * ch]
            .iter_mut()
            .for_each(|s| *s *= gain);
        from = to;
    }
}

/// Adds the part of `clip` from timeline audio frame `start` into `out`,
/// with its gain and fades.
fn add_clip(
    snapshot: &MixSnapshot,
    clip: &MixClip,
    nested: Option<&mut NestedState>,
    start: u64,
    out: &mut [f32],
) {
    let ch = snapshot.channels as usize;
    let end = start + (out.len() / ch) as u64;
    let to = end.min(clip.start + clip.len);
    let from = start.max(clip.start);
    if from >= to {
        return;
    }
    match (&clip.input, nested) {
        (ClipInput::Blocks(buffer), _) => add_frames(snapshot, clip, buffer, start, from..to, out),
        (ClipInput::Nested(mix), Some(nested)) => {
            let rendered = nested.render(&mix.mix, nested_frames(clip, from..to));
            add_frames(snapshot, clip, &rendered, start, from..to, out);
        }
        (ClipInput::Nested(_), None) => {}
    }
}

/// The nested frames a clip reads over the timeline frames `frames`.
fn nested_frames(clip: &MixClip, frames: std::ops::Range<u64>) -> std::ops::Range<u64> {
    if clip.step == 1.0 {
        let first = clip.source_offset + (frames.start - clip.start);
        return first..first + (frames.end - frames.start);
    }
    let pos = |f: u64| clip.source_offset as f64 + (f - clip.start) as f64 * clip.step;
    // `add_resampled` reads up to `step` frames from each position.
    pos(frames.start) as u64..pos(frames.end - 1) as u64 + clip.step as u64 + 2
}

/// Adds the timeline frames `frames` of `clip`, reading `input`, to `out`
/// (which starts at `start`).
fn add_frames(
    snapshot: &MixSnapshot,
    clip: &MixClip,
    input: &impl Frames,
    start: u64,
    frames: std::ops::Range<u64>,
    out: &mut [f32],
) {
    let ch = snapshot.channels as usize;
    let to = frames.end;
    let mut f = frames.start;
    while f < to {
        let in_clip = f - clip.start;
        let block = in_clip / GAIN_BLOCK_FRAMES;
        let block_end = (clip.start + (block + 1) * GAIN_BLOCK_FRAMES).min(to);
        // At the start of the block, not of the call: how the mix is split
        // into calls must not change it.
        let gain = block_gain_linear(clip, block, snapshot.sample_rate)
            * fade_multiplier(clip, block * GAIN_BLOCK_FRAMES);
        let dst = ((f - start) as usize) * ch;
        if clip.step == 1.0 {
            let mut from = f;
            while from < block_end {
                let src = clip.source_offset + (from - clip.start);
                let dst = (from - start) as usize * ch;
                let frames = match input.run(src) {
                    Run::Samples(samples) => {
                        let count = samples.len().min((block_end - from) as usize * ch);
                        for (d, s) in out[dst..dst + count].iter_mut().zip(&samples[..count]) {
                            *d += s * gain;
                        }
                        (count / ch) as u64
                    }
                    Run::Silence(frames) => frames,
                    Run::End => break,
                };
                from += frames.max(1);
            }
        } else {
            let frames = &mut out[dst..dst + (block_end - f) as usize * ch];
            for (i, frame) in frames.chunks_exact_mut(ch).enumerate() {
                let pos = clip.source_offset as f64 + (in_clip + i as u64) as f64 * clip.step;
                add_resampled(input, pos, clip.step, gain, frame);
            }
        }
        f = block_end;
    }
}

fn mono_as_stereo(peak: [f32; 2], channels: usize) -> [f32; 2] {
    if channels == 1 { [peak[0]; 2] } else { peak }
}

/// Varispeed read of the frame at fractional position `pos`: linear
/// interpolation slowing down; speeding up, the average of the `step`
/// frames skipped over, a crude low-pass against aliasing.
fn add_resampled(buffer: &impl Frames, pos: f64, step: f64, gain: f32, out: &mut [f32]) {
    let last = buffer.end().saturating_sub(1);
    let at = |frame: u64, c: usize| {
        buffer
            .frame(frame.min(last))
            .map_or(0.0, |samples| samples[c])
    };
    let pos = pos.max(buffer.start() as f64);
    let i = pos as u64;
    if step < 1.0 {
        let t = (pos - i as f64) as f32;
        for (c, o) in out.iter_mut().enumerate() {
            *o += (at(i, c) + (at(i + 1, c) - at(i, c)) * t) * gain;
        }
    } else {
        let n = (step as usize).max(1);
        for (c, o) in out.iter_mut().enumerate() {
            let sum: f32 = (i..i + n as u64).map(|frame| at(frame, c)).sum();
            *o += sum / n as f32 * gain;
        }
    }
}

/// Linear fade ramp at `in_clip` samples from the start of the
/// clip, same semantics as `Clip::fade_multiplier_at` but in audio samples.
fn fade_multiplier(clip: &MixClip, in_clip: u64) -> f32 {
    let in_ramp = if clip.fade_in > 0 {
        (in_clip as f32 / clip.fade_in as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let out_ramp = if clip.fade_out > 0 {
        let from_end = clip.len.saturating_sub(in_clip);
        (from_end as f32 / clip.fade_out as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    in_ramp * out_ramp
}

fn block_gain_linear(clip: &MixClip, block: u64, sample_rate: u32) -> f32 {
    if clip.gain_db.is_constant() {
        return db_to_linear(clip.gain_db.default);
    }
    // The gain keyframes live in *source* frames: the frame covering
    // this instant of the media.
    let media_frame =
        clip.media_offset as f64 + (block * GAIN_BLOCK_FRAMES) as f64 * clip.media_step;
    let media_secs = media_frame / sample_rate as f64;
    let source_frame = (media_secs * clip.clip_fps).floor() as FrameIdx;
    db_to_linear(clip.gain_db.value_at(source_frame))
}

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// From `from` to `to` channels, appending to `out`. Symmetric, not a broadcast
/// downmix: going down, channel `i` is averaged onto `i % to` (on 5.1
/// ffmpeg groups L,C,Ls and R,LFE,Rs), going up, channel `i` copies
/// `i % from`.
pub fn remix_channels_into(samples: &[f32], from: u16, to: u16, out: &mut Vec<f32>) {
    if from == 0 || to == 0 {
        return;
    }
    if from == to {
        out.extend_from_slice(samples);
        return;
    }
    let (from, to) = (from as usize, to as usize);
    out.reserve(samples.len() / from * to);
    for frame in samples.chunks_exact(from) {
        if from > to {
            for c in 0..to {
                let (sum, n) = frame
                    .iter()
                    .skip(c)
                    .step_by(to)
                    .fold((0.0, 0u32), |(sum, n), &v| (sum + v, n + 1));
                out.push(sum / n as f32);
            }
        } else {
            out.extend((0..to).map(|c| frame[c % from]));
        }
    }
}

/// Audio already stretched to `tempo` (fast forward): frame `i` of the
/// concatenation of `chunks` corresponds to timeline audio frame
/// `origin + i * tempo`.
#[derive(Clone)]
pub struct StretchedWindow {
    pub tempo: u64,
    pub origin: u64,
    pub chunks: Vec<Arc<Vec<f32>>>,
}

impl StretchedWindow {
    pub fn frames(&self, channels: u16) -> u64 {
        let ch = channels.max(1) as usize;
        self.chunks.iter().map(|c| (c.len() / ch) as u64).sum()
    }

    /// First uncovered timeline audio frame.
    pub fn covered_until(&self, channels: u16) -> u64 {
        self.origin + self.frames(channels) * self.tempo
    }
}

/// What the callback plays: the mix, or at speeds > 1x the stretched
/// window.
pub struct MixerState {
    pub mix: Arc<MixSnapshot>,
    pub stretched: Option<StretchedWindow>,
    /// Locked only by the callback; allocated here, on the UI thread.
    effects: Mutex<MixState>,
}

impl MixerState {
    pub fn new(mix: Arc<MixSnapshot>, stretched: Option<StretchedWindow>) -> Self {
        let effects = Mutex::new(MixState::new(&mix));
        Self {
            mix,
            stretched,
            effects,
        }
    }
}

/// Writes into `out` the stretched audio corresponding to timeline audio
/// frame `position`; silence outside the window. No allocations.
pub fn render_stretched(window: &StretchedWindow, channels: u16, position: u64, out: &mut [f32]) {
    out.fill(0.0);
    let ch = channels as usize;
    if ch == 0 || window.tempo == 0 || position < window.origin {
        return;
    }
    let mut skip = ((position - window.origin) / window.tempo) as usize * ch;
    let mut written = 0;
    for chunk in &window.chunks {
        if skip >= chunk.len() {
            skip -= chunk.len();
            continue;
        }
        let take = (chunk.len() - skip).min(out.len() - written);
        out[written..written + take].copy_from_slice(&chunk[skip..skip + take]);
        written += take;
        skip = 0;
        if written == out.len() {
            break;
        }
    }
}

/// Single cpal stream playing the current state. The position (timeline
/// audio frame) is the playback clock.
pub struct Mixer {
    _stream: cpal::Stream,
    playing: Arc<AtomicBool>,
    position: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<Arc<MixerState>>>>,
    /// Published states still referenced by the callback: keeping them here
    /// guarantees that the last drop (with deallocation) happens on the UI
    /// thread, never on the audio one.
    retained: Vec<Arc<MixerState>>,
    peak_left_bits: Arc<AtomicU32>,
    peak_right_bits: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
}

impl Mixer {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no audio output device")?;
        // Native channels of the device: asking for others makes PipeWire insert a
        // remix that adds output latency, visible as audio lagging
        // behind playhead and waveform.
        let channels = device
            .default_output_config()
            .map(|c| c.channels())
            .unwrap_or(2);
        let sample_rate = PROJECT_SAMPLE_RATE;
        let config = cpal::StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let playing = Arc::new(AtomicBool::new(false));
        let position = Arc::new(AtomicU64::new(0));
        let initial = Arc::new(MixerState::new(
            Arc::new(MixSnapshot::empty(sample_rate, channels)),
            None,
        ));
        let pending: Arc<Mutex<Option<Arc<MixerState>>>> = Arc::new(Mutex::new(None));
        let peak_left_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let peak_right_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));

        let cb_playing = playing.clone();
        let cb_position = position.clone();
        let cb_pending = pending.clone();
        let cb_peak_left = peak_left_bits.clone();
        let cb_peak_right = peak_right_bits.clone();
        let mut current = initial.clone();
        let ch = channels.max(1) as usize;

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if let Ok(mut slot) = cb_pending.try_lock()
                        && let Some(next) = slot.take()
                    {
                        if let (Ok(mut effects), Ok(previous)) =
                            (next.effects.try_lock(), current.effects.try_lock())
                        {
                            effects.continue_from(&previous);
                        }
                        current = next;
                    }
                    if cb_playing.load(Ordering::Relaxed) && current.mix.channels as usize == ch {
                        let pos = cb_position.load(Ordering::Relaxed);
                        let frames = (data.len() / ch) as u64;
                        let advance = match &current.stretched {
                            Some(window) => {
                                render_stretched(window, channels, pos, data);
                                frames * window.tempo
                            }
                            None => {
                                match current.effects.try_lock() {
                                    Ok(mut effects) => {
                                        mix_range_metered(&current.mix, &mut effects, pos, data)
                                    }
                                    Err(_) => data.fill(0.0),
                                }
                                frames
                            }
                        };
                        // A seek that arrived during the mix wins over the advance.
                        let _ = cb_position.compare_exchange(
                            pos,
                            pos + advance,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    } else {
                        data.fill(0.0);
                    }
                    let (l, r) = stereo_peak(data, ch);
                    cb_peak_left.store(l.to_bits(), Ordering::Relaxed);
                    cb_peak_right.store(r.to_bits(), Ordering::Relaxed);
                },
                move |err| eprintln!("vv-audio: mixer stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        Ok(Self {
            _stream: stream,
            playing,
            position,
            pending,
            retained: vec![initial],
            peak_left_bits,
            peak_right_bits,
            sample_rate,
            channels,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn set_state(&mut self, state: Arc<MixerState>) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());
        self.retained.retain(|s| Arc::strong_count(s) > 1);
        self.retained.push(state);
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn seek(&self, timeline_sample: u64) {
        self.position.store(timeline_sample, Ordering::Relaxed);
    }

    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    pub fn peak_linear_stereo(&self) -> (f32, f32) {
        (
            f32::from_bits(self.peak_left_bits.load(Ordering::Relaxed)),
            f32::from_bits(self.peak_right_bits.load(Ordering::Relaxed)),
        )
    }
}

fn stereo_peak(data: &[f32], channels: usize) -> (f32, f32) {
    let mut left = 0.0f32;
    let mut right = 0.0f32;
    for frame in data.chunks(channels.max(1)) {
        if let Some(&s) = frame.first() {
            left = left.max(s.abs());
        }
        match frame.get(1) {
            Some(&s) => right = right.max(s.abs()),
            None => right = left,
        }
    }
    (left, right)
}

#[cfg(test)]
#[path = "tests/mixer.rs"]
mod tests;
