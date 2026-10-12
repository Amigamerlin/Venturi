//! Audio track mixing (gain, resample, time-stretch) and `cpal` output
//! (see ARCHITECTURE.md § Audio pipeline). The mixer position is the
//! playback clock.

pub mod blocks;
pub mod dynamics;
pub mod effect;
pub mod eq;
mod filter;
pub mod loudness;
pub mod mixer;
pub mod recorder;
pub mod spectrum;
pub mod stretch;

pub use blocks::{BLOCK_FRAMES, SampleBlocks};
pub use mixer::{
    AnalysisSlot, AudioSource, BandMeter, ClipAudio, CompoundBlock, CompoundMix, LevelAnalysis,
    LevelReading, MixSnapshot, Mixer, MixerState, StretchedWindow,
};
pub use stretch::stretch_samples;
