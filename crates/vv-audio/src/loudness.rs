//! Level measurements of a normalization: sample peak, true peak and
//! integrated loudness (ITU-R BS.1770-4, the measure of EBU R128).

use vv_core::NormalizeMode;

/// Fed interleaved samples in order, block after block.
pub struct LevelMeter {
    kind: Kind,
}

enum Kind {
    SamplePeak(f32),
    TruePeak(TruePeak),
    Loudness(Loudness),
}

impl LevelMeter {
    pub fn new(mode: NormalizeMode, sample_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1) as usize;
        let kind = match mode {
            NormalizeMode::SamplePeak => Kind::SamplePeak(0.0),
            NormalizeMode::TruePeak => Kind::TruePeak(TruePeak::new(channels)),
            NormalizeMode::Loudness => Kind::Loudness(Loudness::new(sample_rate, channels)),
        };
        Self { kind }
    }

    pub fn push(&mut self, samples: &[f32]) {
        match &mut self.kind {
            Kind::SamplePeak(peak) => *peak = samples.iter().fold(*peak, |p, s| p.max(s.abs())),
            Kind::TruePeak(meter) => meter.push(samples),
            Kind::Loudness(meter) => meter.push(samples),
        }
    }

    /// Linear, so that a target over it is the gain: a loudness of L LUFS
    /// reads `10^(L/20)`. Silence reads 0.
    pub fn finish(self) -> f32 {
        LevelParts::merge(&[self.into_parts(&[])])
    }

    /// The signal before the part measured: it sets the filters' state and
    /// what the true peak interpolates from, without being measured.
    pub fn warm_up(&mut self, samples: &[f32]) {
        match &mut self.kind {
            Kind::SamplePeak(_) => {}
            Kind::TruePeak(meter) => {
                meter.pending.extend_from_slice(samples);
                let keep = (LOOKAHEAD - 1) * meter.channels;
                let drop = meter.pending.len().saturating_sub(keep);
                meter.pending.drain(..drop);
            }
            Kind::Loudness(meter) => {
                let (energy, frames, segments) = (meter.energy, meter.frames, meter.segments.len());
                meter.push(samples);
                meter.segments.truncate(segments);
                (meter.energy, meter.frames) = (energy, frames);
            }
        }
    }

    /// What `finish` reads, kept apart so that the measures of consecutive
    /// parts of one signal merge into the measure of all of it. `after`: the
    /// signal after the part (`LOOKAHEAD_FRAMES` of it), interpolated across
    /// the edge instead of silence.
    pub fn into_parts(self, after: &[f32]) -> LevelParts {
        match self.kind {
            Kind::SamplePeak(peak) => LevelParts::Peak(peak),
            Kind::TruePeak(mut meter) if !after.is_empty() => {
                meter.push(after);
                LevelParts::Peak(meter.peak)
            }
            Kind::TruePeak(meter) => LevelParts::Peak(meter.finish()),
            Kind::Loudness(meter) => LevelParts::Loudness {
                segment_frames: meter.segment_frames,
                segments: meter.segments,
                tail: (meter.energy, meter.frames),
            },
        }
    }
}

/// A level measured over one part of a signal.
#[derive(Debug, Clone, PartialEq)]
pub enum LevelParts {
    /// Sample or true peak. A true peak measured in parts misses what the
    /// interpolation sees across their edges.
    Peak(f32),
    /// The weighted energy of each 100 ms segment, and of the frames after
    /// the last one. Merged, a part's tail counts as one more segment of the
    /// same power: the gating blocks shift by less than a segment at each
    /// edge.
    Loudness {
        segment_frames: usize,
        segments: Vec<f64>,
        tail: (f64, usize),
    },
}

impl LevelParts {
    /// The level of the parts in order, as `LevelMeter::finish` reads it.
    /// Parts of different modes are not merged: the first one's wins.
    pub fn merge(parts: &[LevelParts]) -> f32 {
        let Some(first) = parts.first() else {
            return 0.0;
        };
        match first {
            LevelParts::Peak(_) => parts
                .iter()
                .filter_map(|part| match part {
                    LevelParts::Peak(peak) => Some(*peak),
                    LevelParts::Loudness { .. } => None,
                })
                .fold(0.0, f32::max),
            &LevelParts::Loudness { segment_frames, .. } => {
                let loudness: Vec<(&[f64], (f64, usize))> = parts
                    .iter()
                    .filter_map(|part| match part {
                        LevelParts::Loudness { segments, tail, .. } => {
                            Some((segments.as_slice(), *tail))
                        }
                        LevelParts::Peak(_) => None,
                    })
                    .collect();
                let mut segments = Vec::new();
                for (i, (part, tail)) in loudness.iter().enumerate() {
                    segments.extend_from_slice(part);
                    // At its own power, as if a whole segment.
                    if i + 1 < loudness.len() && tail.1 > 0 {
                        segments.push(tail.0 * segment_frames as f64 / tail.1 as f64);
                    }
                }
                let last_tail = loudness.last().map_or((0.0, 0), |(_, tail)| *tail);
                let frames = loudness
                    .iter()
                    .map(|(part, tail)| part.len() * segment_frames + tail.1)
                    .sum();
                gated_loudness(&segments, segment_frames, last_tail.0, frames)
                    .map_or(0.0, |lufs| 10f64.powf(lufs / 20.0) as f32)
            }
        }
    }
}

/// 4x oversampling, as in BS.1770 Annex 2 (here a Hann-windowed sinc).
const OVERSAMPLING: usize = 4;
const TAPS: usize = 12;
/// Frames of the window after the one being interpolated.
const LOOKAHEAD: usize = TAPS / 2;
/// The signal around a part that its measure reads (`LevelMeter::warm_up`,
/// `LevelMeter::into_parts`).
pub const LOOKAHEAD_FRAMES: u64 = LOOKAHEAD as u64;

struct TruePeak {
    channels: usize,
    /// Per phase between two samples, `TAPS` coefficients.
    phases: [[f32; TAPS]; OVERSAMPLING - 1],
    /// The frames whose interpolation still needs the ones after them,
    /// preceded by the window before them.
    pending: Vec<f32>,
    peak: f32,
}

impl TruePeak {
    fn new(channels: usize) -> Self {
        let phases = std::array::from_fn(|p| {
            let offset = (p + 1) as f64 / OVERSAMPLING as f64;
            let mut taps: [f64; TAPS] = std::array::from_fn(|k| {
                let x = k as f64 - (LOOKAHEAD - 1) as f64 - offset;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let hann = 0.5 * (1.0 + (std::f64::consts::PI * x / LOOKAHEAD as f64).cos());
                sinc * hann
            });
            // Unity gain at DC.
            let sum: f64 = taps.iter().sum();
            taps.iter_mut().for_each(|t| *t /= sum);
            taps.map(|t| t as f32)
        });
        Self {
            channels,
            phases,
            // Silence before the start.
            pending: vec![0.0; (LOOKAHEAD - 1) * channels],
            peak: 0.0,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.peak = samples.iter().fold(self.peak, |p, s| p.max(s.abs()));
        self.pending.extend_from_slice(samples);
        let ch = self.channels;
        let frames = self.pending.len() / ch;
        if frames < TAPS {
            return;
        }
        for window in 0..=frames - TAPS {
            for phase in &self.phases {
                for c in 0..ch {
                    let value: f32 = phase
                        .iter()
                        .enumerate()
                        .map(|(k, h)| h * self.pending[(window + k) * ch + c])
                        .sum();
                    self.peak = self.peak.max(value.abs());
                }
            }
        }
        self.pending.drain(..(frames - TAPS + 1) * ch);
    }

    fn finish(mut self) -> f32 {
        // Silence after the end too.
        let tail = vec![0.0; LOOKAHEAD * self.channels];
        self.push(&tail);
        self.peak
    }
}

/// Gating blocks of 400 ms overlapping by 75%: one every 100 ms.
const SEGMENTS_PER_BLOCK: usize = 4;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = -10.0;

struct Loudness {
    channels: usize,
    weights: Vec<f64>,
    filters: Vec<[Biquad; 2]>,
    segment_frames: usize,
    /// Weighted energy of the current segment and its frames so far.
    energy: f64,
    frames: usize,
    /// Weighted energy of each 100 ms segment completed.
    segments: Vec<f64>,
}

impl Loudness {
    fn new(sample_rate: u32, channels: usize) -> Self {
        let rate = sample_rate.max(1) as f64;
        // Surround weights on 5.1 (L R C LFE Ls Rs); the device order of
        // anything else is unknown, all of it counts alike.
        let weights = (0..channels)
            .map(|c| match (channels, c) {
                (6, 3) => 0.0,
                (6, 4 | 5) => 1.41,
                _ => 1.0,
            })
            .collect();
        Self {
            channels,
            weights,
            filters: (0..channels).map(|_| k_weighting(rate)).collect(),
            segment_frames: (rate / 10.0).round().max(1.0) as usize,
            energy: 0.0,
            frames: 0,
            segments: Vec::new(),
        }
    }

    fn push(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(self.channels) {
            for ((sample, [shelf, high_pass]), weight) in
                frame.iter().zip(&mut self.filters).zip(&self.weights)
            {
                let y = high_pass.process(shelf.process(*sample as f64));
                self.energy += weight * y * y;
            }
            self.frames += 1;
            if self.frames == self.segment_frames {
                self.segments.push(self.energy);
                self.energy = 0.0;
                self.frames = 0;
            }
        }
    }
}

/// Integrated loudness of the 100 ms `segments` followed by `tail_energy`,
/// `frames` long in all; `None` for silence.
fn gated_loudness(
    segments: &[f64],
    segment_frames: usize,
    tail_energy: f64,
    frames: usize,
) -> Option<f64> {
    let block_frames = (segment_frames * SEGMENTS_PER_BLOCK) as f64;
    let mut blocks: Vec<f64> = segments
        .windows(SEGMENTS_PER_BLOCK)
        .map(|w| w.iter().sum::<f64>() / block_frames)
        .collect();
    // Shorter than a block: the whole of it is the one block.
    if blocks.is_empty() && frames > 0 {
        blocks.push((segments.iter().sum::<f64>() + tail_energy) / frames as f64);
    }
    let loudness = |mean_square: f64| -0.691 + 10.0 * mean_square.log10();
    let gated_mean = |gate: f64| {
        let above: Vec<f64> = blocks
            .iter()
            .copied()
            .filter(|&b| b > 0.0 && loudness(b) > gate)
            .collect();
        (!above.is_empty()).then(|| above.iter().sum::<f64>() / above.len() as f64)
    };
    let relative_gate = loudness(gated_mean(ABSOLUTE_GATE_LUFS)?) + RELATIVE_GATE_LU;
    gated_mean(relative_gate.max(ABSOLUTE_GATE_LUFS)).map(loudness)
}

/// Direct form I.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self {
            b,
            a,
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// The BS.1770 pre-filter (high shelf) and RLB high-pass, from their
/// analog prototypes so that they hold at any rate, not only at 48 kHz.
fn k_weighting(rate: f64) -> [Biquad; 2] {
    let (f0, gain_db, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::new(
        [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
        ],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad::new(
        [1.0, -2.0, 1.0],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    [shelf, high_pass]
}

#[cfg(test)]
#[path = "tests/loudness.rs"]
mod tests;
