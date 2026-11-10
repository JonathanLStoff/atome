//! Scheduling an output's audio from any thread.
//!
//! [`OutputClass`](super::OutputClass) owns the device stream and stays where
//! it was made. A [`Scheduler`] is what other threads hold instead: it queues
//! samples at absolute positions on the stream, tags them with a voice so they
//! can be taken back, and reads the stream's [`PlayClock`] — everything a video
//! engine needs to play a film's soundtrack in step with its pictures.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cpal::{Error, ErrorKind};
use ringbuf::traits::Producer;
use ringbuf::HeapProd;

use super::{resample_into, GainChange, MixCommand, OutputClass, PlayClock, SampleType, Voices};

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

    /// Brings `voice` to `gain` (linear: `1.0` as scheduled, `0.0` silent)
    /// over `over`, from whatever level it is at; `Duration::ZERO` is at once.
    /// The level holds for everything the voice schedules afterwards.
    ///
    /// Done in the mixer, on the samples the voice has queued but the device
    /// has not been handed yet, so it is heard within a buffer however far
    /// ahead the voice is scheduled.
    pub fn set_gain(&self, voice: u64, gain: f32, over: Duration) {
        self.voices.gain(self.change(voice, gain, over, false));
    }

    /// Fades `voice` to silence over `over` and then takes it back, as
    /// [`cancel`](Self::cancel) would: a stop that does not click.
    pub fn fade_out(&self, voice: u64, over: Duration) {
        self.voices.gain(self.change(voice, 0.0, over, true));
    }

    fn change(&self, voice: u64, gain: f32, over: Duration, then_cancel: bool) -> GainChange {
        let channels = usize::from(self.channels.max(1));
        let frames = (over.as_secs_f64() * f64::from(self.sample_rate)).round() as usize;
        GainChange {
            voice,
            gain,
            samples: frames * channels,
            channels,
            then_cancel,
        }
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

    /// An [`Aligner`] for this output: [`align`](Self::align) for a feed of
    /// many blocks, with nothing allocated per block.
    pub fn aligner(&self) -> Aligner<S> {
        Aligner {
            channels: self.channels,
            sample_rate: self.sample_rate,
            mapped: Vec::new(),
            out: Vec::new(),
        }
    }
}

/// [`Scheduler::align`] with its buffers kept between calls: one per feed, used
/// for block after block. A block already at the output's rate and channel
/// count is handed straight back, not copied.
pub struct Aligner<S: SampleType> {
    channels: u16,
    sample_rate: u32,
    mapped: Vec<S>,
    out: Vec<S>,
}

impl<S: SampleType> Aligner<S> {
    /// Whether samples at `rate` and `channels` are the output's already, so
    /// [`align`](Self::align) would return them as they are.
    pub fn matches(&self, rate: u32, channels: u16) -> bool {
        channels.max(1) == self.channels.max(1) && (rate == self.sample_rate || rate == 0)
    }

    /// As [`Scheduler::align`], into this aligner's own buffers. The slice is
    /// good until the next call.
    pub fn align<'a>(&'a mut self, samples: &'a [S], rate: u32, channels: u16) -> &'a [S] {
        let source = usize::from(channels.max(1));
        let target = usize::from(self.channels.max(1));
        let frames = samples.len() / source;
        let resampled = rate != self.sample_rate && rate > 0;

        if source == target && !resampled {
            return &samples[..frames * source];
        }

        if source != target {
            self.mapped.clear();
            OutputClass::<S>::map_channels_into(samples, frames, source, target, &mut self.mapped);
        }
        if !resampled {
            return &self.mapped;
        }

        let mapped: &[S] = if source == target { &samples[..frames * source] } else { &self.mapped };
        self.out.clear();
        resample_into(mapped, frames, target, rate, self.sample_rate, &mut self.out);
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aligner(channels: u16, sample_rate: u32) -> Aligner<f32> {
        Aligner { channels, sample_rate, mapped: Vec::new(), out: Vec::new() }
    }

    #[test]
    fn a_block_that_already_fits_is_handed_back_as_it_is() {
        let mut aligner = aligner(2, 48_000);
        let block = [0.1, 0.2, 0.3, 0.4];

        assert!(aligner.matches(48_000, 2));
        let out = aligner.align(&block, 48_000, 2);
        assert_eq!(out.as_ptr(), block.as_ptr(), "not copied");
        assert_eq!(out, &block);
    }

    #[test]
    fn an_aligner_gives_what_align_gives() {
        let block: Vec<f32> = (0..64).map(|i| i as f32 / 64.0).collect();

        for (rate, channels) in [(48_000, 1), (44_100, 2), (44_100, 1), (96_000, 4)] {
            let mut aligner = aligner(2, 48_000);
            assert!(!aligner.matches(rate, channels));
            let source = usize::from(channels);
            let frames = block.len() / source;

            let mut expected = if source == 2 {
                block[..frames * source].to_vec()
            } else {
                OutputClass::<f32>::map_channels(&block, frames, source, 2)
            };
            if rate != 48_000 {
                expected = OutputClass::<f32>::resample(&expected, frames, 2, rate, 48_000);
            }

            // Twice, so what the first call left in the buffers is seen not to leak into the second.
            assert_eq!(aligner.align(&block, rate, channels), &expected[..]);
            assert_eq!(aligner.align(&block, rate, channels), &expected[..], "{rate} Hz, {channels} ch");
        }
    }
}
