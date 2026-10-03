//! Scheduling an output's audio from any thread.
//!
//! [`OutputClass`](super::OutputClass) owns the device stream and stays where
//! it was made. A [`Scheduler`] is what other threads hold instead: it queues
//! samples at absolute positions on the stream, tags them with a voice so they
//! can be taken back, and reads the stream's [`PlayClock`] — everything a video
//! engine needs to play a film's soundtrack in step with its pictures.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cpal::{Error, ErrorKind};
use ringbuf::traits::Producer;
use ringbuf::HeapProd;

use super::{MixCommand, OutputClass, PlayClock, SampleType, Voices};

/// Voice ids are unique in the process, so two schedulers never share one.
static NEXT_VOICE: AtomicU64 = AtomicU64::new(1);

/// Queues one output's audio from any thread. Cheap to clone.
pub struct Scheduler<S: SampleType> {
    commands: Arc<Mutex<HeapProd<MixCommand<S>>>>,
    voices: Arc<Voices>,
    clock: PlayClock,
    channels: u16,
    sample_rate: u32,
}

impl<S: SampleType> Clone for Scheduler<S> {
    fn clone(&self) -> Self {
        Scheduler {
            commands: Arc::clone(&self.commands),
            voices: Arc::clone(&self.voices),
            clock: self.clock.clone(),
            channels: self.channels,
            sample_rate: self.sample_rate,
        }
    }
}

impl<S: SampleType> Scheduler<S> {
    pub(super) fn new(
        commands: Arc<Mutex<HeapProd<MixCommand<S>>>>,
        voices: Arc<Voices>,
        clock: PlayClock,
        channels: u16,
        sample_rate: u32,
    ) -> Self {
        Scheduler {
            commands,
            voices,
            clock,
            channels,
            sample_rate,
        }
    }

    /// A voice id no one else has: tag a sound's samples with it, then
    /// [`cancel`](Self::cancel) it to stop the sound.
    pub fn new_voice(&self) -> u64 {
        NEXT_VOICE.fetch_add(1, Ordering::Relaxed)
    }

    /// Queues interleaved samples, already at the output's rate and channel
    /// count, to start at interleaved index `index` on the stream — the
    /// mixer's index, `frame * channels`. Returns the index just past them.
    ///
    /// Fails, refusing the whole batch, while the mixer's command queue is
    /// full; try again once it has drained.
    pub fn schedule(&self, samples: &[S], index: usize, voice: Option<u64>) -> Result<usize, Error> {
        let command = MixCommand {
            index,
            samples: samples.to_vec(),
            voice,
        };

        self.commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_push(command)
            .map_err(|_| {
                Error::with_message(ErrorKind::ResourceExhausted, "mixer command queue is full")
            })?;

        Ok(index + samples.len())
    }

    /// Takes back everything `voice` scheduled that has not yet reached the
    /// device: the sound stops within a buffer.
    pub fn cancel(&self, voice: u64) {
        self.voices.cancel(voice);
    }

    /// The stream's play position — the timeline [`schedule`](Self::schedule)
    /// indexes into.
    pub fn clock(&self) -> PlayClock {
        self.clock.clone()
    }

    /// The output's channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The output's sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Interleaved samples at `rate` and `channels`, remapped and resampled to
    /// this output's — ready for [`schedule`](Self::schedule). Resampling is
    /// linear and per call, so blocks resampled one at a time can tick at the
    /// joins; matching rates pass through untouched.
    pub fn align(&self, samples: &[S], rate: u32, channels: u16) -> Vec<S> {
        let source = usize::from(channels.max(1));
        let target = usize::from(self.channels.max(1));
        let frames = samples.len() / source;

        let mut buffer = if source == target {
            samples[..frames * source].to_vec()
        } else {
            OutputClass::<S>::map_channels(samples, frames, source, target)
        };

        if rate != self.sample_rate && rate > 0 {
            buffer = OutputClass::<S>::resample(&buffer, frames, target, rate, self.sample_rate);
        }

        buffer
    }
}
