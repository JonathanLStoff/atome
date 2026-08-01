//! The mixer sits between `add_samples` and the audio callback.
//!
//! ```text
//! add_samples() -> HeapRb<MixCommand<S>> -> Mixer<S> -> HeapRb<S> -> CPAL callback
//! ```
//!
//! Everything here is in the output's own sample type `S`: an `OutputClass<i16>`
//! queues `i16`, mixes `i16`, and hands `i16` to the device, with no float
//! round-trip anywhere in the path.
//!
//! Producers never touch the audio ring buffer directly: they send indexed
//! [`MixCommand`]s, and the mixer sums overlapping ones into a plain `Vec<S>`
//! before handing the finished samples to the callback. The `Vec` is fine here —
//! this runs on its own thread, so allocating and resizing it can't stall the
//! audio thread, and the callback stays lock-free.
//!
//! The two buffers split the work by how settled the audio is:
//!
//! - [`Mixer::mix`] is everything still open to change — the samples that are
//!   going to be used, scheduled from the play cursor onwards, which later
//!   commands can still sum into.
//! - [`Mixer::output`] is the buffer about to be sent to CPAL, capped at one
//!   buffer size (`buffer_size * channels`). Once samples move here they are
//!   committed, so only the next callback's worth is ever locked in.

use super::types::SampleType;
use ringbuf::traits::{Consumer, Observer, Producer};
use ringbuf::{HeapCons, HeapProd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long the mixer sleeps when a pass finds nothing to do.
const IDLE_POLL: Duration = Duration::from_micros(500);

/// Furthest ahead of the play cursor a command may schedule samples, in samples.
/// Caps how large the accumulation buffer can grow from a bad index.
const MAX_SCHEDULE_AHEAD: usize = 1 << 20;

/// Shared stop signal for the two halves of the pipeline.
///
/// Scheduled audio lives in two places at once — the mixer's own buffer and the
/// ring buffer the callback reads — and neither thread can reach into the
/// other's. So a stop is a request both sides answer: `epoch` counts stops
/// asked for, `ack` is the last one the mixer has finished clearing for, and
/// the callback keeps throwing the ring away until the two agree. Until they
/// do, the mixer may still be flushing pre-stop samples, and dropping the ring
/// only once would let those be the first thing heard.
#[derive(Debug, Default)]
pub struct ClearSignal {
    epoch: AtomicUsize,
    ack: AtomicUsize,
}

impl ClearSignal {
    /// Asks both sides to drop everything scheduled. Returns the new epoch.
    pub fn request(&self) -> usize {
        self.epoch.fetch_add(1, Ordering::Release) + 1
    }

    /// The most recent stop asked for.
    pub fn epoch(&self) -> usize {
        self.epoch.load(Ordering::Acquire)
    }

    /// The most recent stop the mixer has finished clearing for.
    pub fn acked(&self) -> usize {
        self.ack.load(Ordering::Acquire)
    }

    /// Called by the mixer once it has dropped its own pending audio.
    fn ack(&self, epoch: usize) {
        self.ack.store(epoch, Ordering::Release);
    }
}

/// "Sum these samples into the output starting at this absolute sample index."
///
/// `index` counts interleaved samples from the start of the stream, so it is
/// `frame * channels`, not a frame number.
#[derive(Clone, Debug, PartialEq)]
pub struct MixCommand<S: SampleType> {
    pub index: usize,
    pub samples: Vec<S>,
}

/// Sums incoming [`MixCommand`]s at their scheduled positions and feeds the
/// result to the audio ring buffer, all in the sample type `S`.
pub struct Mixer<S: SampleType> {
    /// Incoming work from `add_samples`.
    commands: HeapCons<MixCommand<S>>,
    /// The buffer about to go to CPAL: producer half of the ring buffer the
    /// callback reads, holding **exactly one buffer size** of samples
    /// (`buffer_size * channels`). Nothing further ahead than the next callback
    /// is ever sitting in here.
    output: HeapProd<S>,
    /// The audio still being assembled — every sample scheduled from the play
    /// cursor onwards, summed but not yet handed over. `mix[i]` is the sample at
    /// absolute index `cursor + i`, and its front becomes the next `output`.
    mix: Vec<S>,
    /// Absolute index of `mix[0]` — everything before it is already committed.
    cursor: usize,
    /// Samples dropped because they arrived after their slot was committed.
    late: usize,
    /// Samples dropped for being scheduled past [`MAX_SCHEDULE_AHEAD`].
    dropped: usize,
    /// Stop requests, shared with the audio callback.
    clear: Arc<ClearSignal>,
    /// The last stop epoch this mixer has cleared for.
    cleared: usize,
}

impl<S: SampleType> Mixer<S> {
    pub fn new(commands: HeapCons<MixCommand<S>>, output: HeapProd<S>) -> Self {
        Mixer {
            commands,
            output,
            mix: Vec::new(),
            cursor: 0,
            late: 0,
            dropped: 0,
            clear: Arc::new(ClearSignal::default()),
            cleared: 0,
        }
    }

    /// The stop signal this mixer answers, for handing to the audio callback.
    pub fn clear_signal(&self) -> Arc<ClearSignal> {
        Arc::clone(&self.clear)
    }

    /// Drops every command still queued and every sample still being assembled.
    ///
    /// The cursor is left where it is: it is an absolute position in the stream,
    /// not a count of what got played, so commands scheduled for indexes behind
    /// it are still late after a stop.
    fn clear_pending(&mut self) {
        self.commands.clear();
        self.mix.clear();
    }

    /// Sum one command into the accumulation buffer.
    fn apply(&mut self, command: MixCommand<S>) {
        let MixCommand { index, samples } = command;

        // Anything at or before the cursor was already handed to the callback,
        // so only the tail of a late command can still be mixed.
        let skip = self.cursor.saturating_sub(index);
        if skip >= samples.len() {
            self.late += samples.len();
            return;
        }
        self.late += skip;

        let start = (index + skip) - self.cursor;
        let end = start + (samples.len() - skip);
        if end > MAX_SCHEDULE_AHEAD {
            self.dropped += samples.len() - skip;
            return;
        }
        if self.mix.len() < end {
            self.mix.resize(end, S::SILENCE);
        }

        for (slot, sample) in self.mix[start..end].iter_mut().zip(&samples[skip..]) {
            *slot = slot.mix(*sample);
        }
    }

    /// Move the front of [`mix`](Self::mix) into [`output`](Self::output), as
    /// far as there is room. Returns how many samples were committed.
    ///
    /// `output` only ever holds one buffer size, so that is exactly how much can
    /// be in flight, and everything beyond the next callback stays in `mix`
    /// where later commands can still be summed into it. That one buffer size is
    /// therefore the scheduling horizon: a command landing inside the buffer
    /// already handed over is late, and its overlapping part is dropped.
    fn flush(&mut self) -> usize {
        let ready = self.mix.len().min(self.output.vacant_len());
        if ready == 0 {
            return 0;
        }

        let pushed = self.output.push_slice(&self.mix[..ready]);
        self.mix.drain(..pushed);
        self.cursor += pushed;
        pushed
    }

    /// One pass: drain every pending command, then top up the ring buffer.
    /// Returns whether the pass did any work.
    pub fn tick(&mut self) -> bool {
        let mut worked = false;

        // Answer a stop before touching anything else, so nothing queued before
        // it gets mixed or flushed on the way past.
        let epoch = self.clear.epoch();
        if epoch != self.cleared {
            self.clear_pending();
            self.cleared = epoch;
            self.clear.ack(epoch);
            worked = true;
        }

        while let Some(command) = self.commands.try_pop() {
            self.apply(command);
            worked = true;
        }
        self.flush() > 0 || worked
    }

    /// Runs [`tick`](Self::tick) until `running` is cleared, sleeping only when
    /// a pass finds nothing to do.
    pub fn run(mut self, running: Arc<AtomicBool>) {
        while running.load(Ordering::Relaxed) {
            if !self.tick() {
                thread::sleep(IDLE_POLL);
            }
        }
    }

    /// Absolute index of the next sample the mixer will commit.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Samples dropped because their slot had already been committed.
    pub fn late_samples(&self) -> usize {
        self.late
    }

    /// Samples dropped for being scheduled too far ahead.
    pub fn dropped_samples(&self) -> usize {
        self.dropped
    }
}

/// Owns the mixer thread and stops it when dropped.
pub struct MixerHandle {
    running: Arc<AtomicBool>,
    clear: Arc<ClearSignal>,
    thread: Option<JoinHandle<()>>,
}

impl MixerHandle {
    /// Spawns `mixer` on its own thread.
    pub fn spawn<S: SampleType>(mixer: Mixer<S>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let clear = mixer.clear_signal();

        let thread = thread::Builder::new()
            .name("atome_mixer".to_string())
            .spawn(move || mixer.run(flag))
            .expect("failed to spawn mixer thread");

        MixerHandle {
            running,
            clear,
            thread: Some(thread),
        }
    }

    /// Drops everything scheduled but not yet played, on both sides of the
    /// pipeline: the mixer's queued commands and accumulation buffer, and the
    /// ring buffer the audio callback is reading from.
    ///
    /// Returns as soon as the request is posted — the mixer picks it up on its
    /// next pass and the callback on its next one, so audio stops within a
    /// buffer rather than the instant this returns.
    pub fn clear_samples(&self) {
        self.clear.request();
    }

    /// The stop signal, for handing to the audio callback so it can drop the
    /// samples already committed to the ring buffer.
    pub fn clear_signal(&self) -> Arc<ClearSignal> {
        Arc::clone(&self.clear)
    }
}

impl Drop for MixerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
