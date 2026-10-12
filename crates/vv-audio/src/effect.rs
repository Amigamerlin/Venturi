//! The interface the mixer drives an effect with: the built-in ones today,
//! a plugin host later (see plans/AUDIO_LIVE_MIX.md §3.6).

use std::any::Any;

use crate::dynamics::BandActivity;

/// An effect with a state, run in order a mix block at a time.
pub trait Effect: Send {
    /// In place, interleaved. No allocations: it runs in the realtime
    /// callback. `Some` for an effect with bands to meter.
    fn process(&mut self, samples: &mut [f32]) -> Option<BandActivity>;

    /// The mix jumped: start again from silence.
    fn reset(&mut self);

    /// Carries on from where `previous` was, if it is the same kind of
    /// effect: new parameters must not restart it.
    fn take_memory_from(&mut self, previous: &dyn Effect);

    fn as_any(&self) -> &dyn Any;
}
