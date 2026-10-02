//! Where an output is in its stream, as heard — readable from any thread.
//!
//! The one clock in a show that must never drift is the audio device's: it
//! consumes samples at exactly its rate, and everything else follows it. A
//! [`PlayClock`] is that clock made visible. The output callback adds every
//! buffer it hands to the device and notes the latency cpal reports between
//! the callback and the moment that buffer is actually heard; any other thread
//! can then ask what is coming out of the speakers *now*.
//!
//! It counts frames of the stream itself — silence included — so its timeline
//! is the mixer's: a sound scheduled with
//! [`add_samples`](crate::output::OutputClass::add_samples) at interleaved
//! index `i` is heard at `i / channels / sample_rate` on this clock. That is
//! what lets a video engine (vtome's `MasterClock`) start a picture on the same
//! frame as its sound.
//!
//! # Real-time safety
//!
//! The callback side is a handful of relaxed atomic stores and one read of the
//! monotonic clock. No lock, no allocation, nothing that can block.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many buffer periods may pass without a callback before the stream is
/// considered stopped. A callback is never exactly on time; several missing in
/// a row means the device has gone, or the stream was dropped.
const STALL_PERIODS: u32 = 4;

/// Never call a stream stopped sooner than this, however short its buffers:
/// at 32 frames and 48 kHz four periods is under 3 ms, which ordinary
/// scheduling jitter on a busy machine exceeds.
const STALL_FLOOR: Duration = Duration::from_millis(50);

/// The play position of one output stream.
///
/// Cheap to clone; every clone reads the same stream. Get one from
/// [`OutputClass::clock`](crate::output::OutputClass::clock).
///
/// ```no_run
/// # fn demo(output: &atome::output::OutputClass<f32>) {
/// let clock = output.clock();
///
/// std::thread::spawn(move || loop {
///     // What the listener hears right now, from the start of the stream.
///     println!("{:?}", clock.position());
///     std::thread::sleep(std::time::Duration::from_millis(500));
/// });
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct PlayClock {
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    /// What every instant below is measured from.
    epoch: Instant,
    sample_rate: AtomicU32,
    /// Frames handed to the device before the most recent callback.
    frames_before: AtomicU64,
    /// Frames the most recent callback handed over.
    period: AtomicU64,
    /// When the most recent callback ran, in nanoseconds from `epoch`, plus
    /// one — so zero can mean "never".
    called_at: AtomicU64,
    /// How long after its callback a buffer is heard, in nanoseconds.
    latency: AtomicU64,
    /// Whether the stream exists. Callbacks alone cannot say: a stream that is
    /// dropped simply stops calling.
    open: AtomicBool,
    /// The furthest position handed out, in nanoseconds, so the clock never
    /// runs backwards when a callback lands a little earlier than projected.
    furthest: AtomicU64,
}

impl PlayClock {
    pub(crate) fn new(sample_rate: u32) -> Self {
        PlayClock {
            state: Arc::new(State {
                epoch: Instant::now(),
                sample_rate: AtomicU32::new(sample_rate.max(1)),
                frames_before: AtomicU64::new(0),
                period: AtomicU64::new(0),
                called_at: AtomicU64::new(0),
                latency: AtomicU64::new(0),
                open: AtomicBool::new(false),
                furthest: AtomicU64::new(0),
            }),
        }
    }

    /// What is being heard now, measured from the first frame of the stream.
    ///
    /// Between callbacks the position moves on with the monotonic clock, so it
    /// is smooth rather than stepping a buffer at a time; it never runs more
    /// than a buffer past the last callback, so a stalled device freezes it
    /// rather than letting it run away. Zero until the stream has started, and
    /// it never goes backwards.
    pub fn position(&self) -> Duration {
        let state = &*self.state;

        let called_at = state.called_at.load(Ordering::Acquire);
        if called_at == 0 {
            return Duration::ZERO;
        }

        let rate = u64::from(state.sample_rate.load(Ordering::Relaxed));
        let frames_before = state.frames_before.load(Ordering::Relaxed);
        let period = state.period.load(Ordering::Relaxed);
        let latency = state.latency.load(Ordering::Relaxed);

        let since_callback = (state.epoch.elapsed().as_nanos() as u64).saturating_sub(called_at - 1);
        let period_nanos = frames_to_nanos(period, rate);

        // The buffer this callback filled starts being heard `latency` after
        // it; until then, the previous one is still playing out.
        let heard = frames_to_nanos(frames_before, rate) + since_callback.min(period_nanos);
        let heard = heard.saturating_sub(latency);

        let furthest = state.furthest.fetch_max(heard, Ordering::AcqRel).max(heard);

        Duration::from_nanos(furthest)
    }

    /// Frames handed to the device since the stream was built — silence
    /// included, so this is the stream's own timeline, the mixer's index
    /// divided by the channel count.
    pub fn frames(&self) -> u64 {
        let state = &*self.state;
        state.frames_before.load(Ordering::Relaxed) + state.period.load(Ordering::Relaxed)
    }

    /// How long after it is written a buffer reaches the listener, as the
    /// device last reported it.
    pub fn latency(&self) -> Duration {
        Duration::from_nanos(self.state.latency.load(Ordering::Relaxed))
    }

    /// The stream's sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.state.sample_rate.load(Ordering::Relaxed)
    }

    /// Whether the stream is running: built, not closed, and calling back.
    ///
    /// False before the first callback, after [`close`](crate::output::OutputClass::close),
    /// and when callbacks stop arriving — a device unplugged mid-show. Anything
    /// following this clock should hold still while it is false.
    pub fn is_running(&self) -> bool {
        let state = &*self.state;

        if !state.open.load(Ordering::Acquire) {
            return false;
        }

        let called_at = state.called_at.load(Ordering::Acquire);
        if called_at == 0 {
            return false;
        }

        let rate = u64::from(state.sample_rate.load(Ordering::Relaxed));
        let period = frames_to_nanos(state.period.load(Ordering::Relaxed), rate);
        let allowance = Duration::from_nanos(period.saturating_mul(u64::from(STALL_PERIODS)))
            .max(STALL_FLOOR);

        let since = (state.epoch.elapsed().as_nanos() as u64).saturating_sub(called_at - 1);

        Duration::from_nanos(since) <= allowance
    }

    /// The stream is about to start calling.
    pub(crate) fn opened(&self) {
        self.state.open.store(true, Ordering::Release);
    }

    /// The stream is gone; nothing more will be heard from it.
    pub(crate) fn closed(&self) {
        self.state.open.store(false, Ordering::Release);
    }

    /// Called from the output callback, once per buffer: `frames` handed over,
    /// to be heard `latency` from now.
    ///
    /// Relaxed stores for the numbers and a release store of the time last, so
    /// a reader that sees the new time sees the numbers that go with it.
    pub(crate) fn advance(&self, frames: u64, latency: Duration) {
        let state = &*self.state;

        let previous = state.period.swap(frames, Ordering::Relaxed);
        state.frames_before.fetch_add(previous, Ordering::Relaxed);
        state
            .latency
            .store(latency.as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);

        let now = state.epoch.elapsed().as_nanos() as u64;
        state.called_at.store(now.saturating_add(1), Ordering::Release);
    }
}

/// `frames` at `rate`, in nanoseconds, without overflowing for a stream that
/// has run for years.
fn frames_to_nanos(frames: u64, rate: u64) -> u64 {
    if rate == 0 {
        return 0;
    }

    (u128::from(frames) * 1_000_000_000 / u128::from(rate)).min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_that_has_never_been_called_is_at_zero_and_stopped() {
        let clock = PlayClock::new(48_000);

        assert_eq!(clock.position(), Duration::ZERO);
        assert_eq!(clock.frames(), 0);
        assert!(!clock.is_running());

        // Open, but no callback yet: still not running.
        clock.opened();
        assert!(!clock.is_running());
    }

    #[test]
    fn the_position_counts_what_was_handed_over_less_the_latency() {
        let clock = PlayClock::new(48_000);
        clock.opened();

        // Two seconds handed over in one-second buffers, 100 ms of latency.
        clock.advance(48_000, Duration::from_millis(100));
        clock.advance(48_000, Duration::from_millis(100));

        let position = clock.position();
        assert_eq!(clock.frames(), 96_000);

        // One whole buffer heard, the second just starting: a second, less
        // the latency, plus however long this test took to get here.
        assert!(position >= Duration::from_millis(900), "{position:?}");
        assert!(position < Duration::from_millis(1_000), "{position:?}");
        assert!(clock.is_running());
    }

    /// Between callbacks the position moves with the monotonic clock — but
    /// never more than a buffer past the last one, so a device that stops
    /// calling freezes the clock instead of letting it run away.
    #[test]
    fn between_callbacks_it_moves_on_but_never_past_one_buffer() {
        let clock = PlayClock::new(1_000);
        clock.opened();

        // Ten-millisecond buffers, no latency.
        clock.advance(10, Duration::ZERO);
        let first = clock.position();

        std::thread::sleep(Duration::from_millis(30));
        let later = clock.position();

        assert!(later > first, "{first:?} → {later:?}");
        assert!(later <= Duration::from_millis(10), "ran past its buffer: {later:?}");
        assert!(clock.is_running(), "50 ms floor, not four 10 ms periods");

        std::thread::sleep(Duration::from_millis(60));
        assert!(!clock.is_running(), "callbacks stopped");
    }

    #[test]
    fn it_never_runs_backwards() {
        let clock = PlayClock::new(1_000);
        clock.opened();

        clock.advance(100, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        let before = clock.position();

        // A burst of latency that would project the position back in time.
        clock.advance(1, Duration::from_millis(500));

        assert!(clock.position() >= before);
    }

    #[test]
    fn closing_stops_it_whatever_the_callbacks_did_last() {
        let clock = PlayClock::new(48_000);
        clock.opened();
        clock.advance(480, Duration::ZERO);
        assert!(clock.is_running());

        clock.closed();
        assert!(!clock.is_running());
    }

    #[test]
    fn years_of_frames_do_not_overflow() {
        // Ten years at 192 kHz.
        let frames = 192_000_u64 * 60 * 60 * 24 * 365 * 10;

        assert_eq!(
            frames_to_nanos(frames, 192_000),
            10 * 365 * 24 * 60 * 60 * 1_000_000_000
        );
        assert_eq!(frames_to_nanos(1, 0), 0);
    }
}
