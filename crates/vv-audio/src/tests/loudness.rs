use super::*;

const RATE: u32 = 48_000;

/// Interleaved, the same sine on every channel.
fn sine(freq: f64, amplitude: f32, phase: f64, seconds: f64, channels: usize) -> Vec<f32> {
    let n = (RATE as f64 * seconds) as usize;
    (0..n)
        .flat_map(|i| {
            let t = i as f64 / RATE as f64;
            let s = amplitude * (2.0 * std::f64::consts::PI * freq * t + phase).sin() as f32;
            std::iter::repeat_n(s, channels)
        })
        .collect()
}

fn measure(mode: NormalizeMode, channels: u16, chunks: &[&[f32]]) -> f32 {
    let mut meter = LevelMeter::new(mode, RATE, channels);
    for chunk in chunks {
        meter.push(chunk);
    }
    meter.finish()
}

fn lufs(level: f32) -> f32 {
    20.0 * level.log10()
}

#[test]
fn a_full_scale_sine_on_one_channel_reads_minus_3_lufs() {
    // The reference of BS.1770: 997 Hz at 0 dBFS, -3.01 LKFS.
    let samples = sine(997.0, 1.0, 0.0, 3.0, 1);
    let level = measure(NormalizeMode::Loudness, 1, &[&samples]);
    assert!((lufs(level) + 3.01).abs() < 0.05, "{} LUFS", lufs(level));
}

#[test]
fn stereo_sums_the_channels() {
    let samples = sine(997.0, 0.1, 0.0, 3.0, 2);
    let level = measure(NormalizeMode::Loudness, 2, &[&samples]);
    assert!((lufs(level) + 20.0).abs() < 0.05, "{} LUFS", lufs(level));
}

#[test]
fn the_gates_leave_out_silence_and_quiet_passages() {
    let loud = sine(997.0, 0.1, 0.0, 3.0, 2);
    let quiet = sine(997.0, 0.001, 0.0, 3.0, 2);
    let silence = vec![0.0; loud.len()];
    let level = measure(NormalizeMode::Loudness, 2, &[&silence, &loud, &quiet]);
    // Ungated, a third of the time loud would read -24.8; the blocks
    // straddling the edges of the loud part still count.
    assert!((lufs(level) + 20.0).abs() < 0.5, "{} LUFS", lufs(level));
}

#[test]
fn silence_reads_zero_in_every_mode() {
    let silence = vec![0.0; 48_000];
    for mode in NormalizeMode::ALL {
        assert_eq!(measure(mode, 2, &[&silence]), 0.0, "{mode:?}");
    }
}

#[test]
fn true_peak_finds_the_peak_between_the_samples() {
    // At a quarter of the rate, 45° off: every sample at ±0.707.
    let samples = sine(12_000.0, 1.0, std::f64::consts::FRAC_PI_4, 0.1, 1);
    let sample_peak = measure(NormalizeMode::SamplePeak, 1, &[&samples]);
    assert!((sample_peak - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
    let (first, rest) = samples.split_at(1001);
    let true_peak = measure(NormalizeMode::TruePeak, 1, &[first, rest]);
    assert!((true_peak - 1.0).abs() < 0.05, "true peak {true_peak}");
}

#[test]
fn the_split_into_blocks_does_not_change_the_reading() {
    let samples = sine(440.0, 0.3, 0.3, 1.5, 2);
    for mode in NormalizeMode::ALL {
        let whole = measure(mode, 2, &[&samples]);
        let parts: Vec<&[f32]> = samples.chunks(2 * 777).collect();
        let split = measure(mode, 2, &parts);
        assert!((whole - split).abs() < 1e-6, "{mode:?}: {whole} vs {split}");
    }
}

/// What the segments of a normalization measure: parts measured on their
/// own and merged read as the whole within 0.1 dB, even 0.65 s long with
/// their edges off the 100 ms grid.
#[test]
fn parts_measured_apart_merge_into_the_whole() {
    let loud = sine(440.0, 0.3, 0.3, 2.0, 2);
    let quiet = sine(997.0, 0.02, 0.0, 1.5, 2);
    let samples = [loud.as_slice(), &quiet, &loud].concat();
    for mode in NormalizeMode::ALL {
        let whole = measure(mode, 2, &[&samples]);
        let part = 2 * 31_337;
        let context = 2 * 4800;
        let parts: Vec<LevelParts> = (0..samples.len())
            .step_by(part)
            .map(|start| {
                let end = (start + part).min(samples.len());
                let mut meter = LevelMeter::new(mode, RATE, 2);
                meter.warm_up(&samples[start.saturating_sub(context)..start]);
                meter.push(&samples[start..end]);
                let after = (end + 2 * LOOKAHEAD_FRAMES as usize).min(samples.len());
                meter.into_parts(&samples[end..after])
            })
            .collect();
        let merged = LevelParts::merge(&parts);
        let db = 20.0 * (merged / whole).log10();
        assert!(db.abs() < 0.1, "{mode:?}: {db} dB");
    }
}
