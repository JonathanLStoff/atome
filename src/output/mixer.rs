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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
///
/// A command with a `voice` is held apart from everything else until the
/// moment it is committed to the device, so [`Voices::cancel`] can take back
/// whatever of that voice has not yet been handed over — a film stopped
/// mid-scene takes its soundtrack with it, within a buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct MixCommand<S: SampleType> {
    pub index: usize,
    pub samples: Vec<S>,
    pub voice: Option<u64>,
}

/// "Bring this voice to this level, over this long."
///
/// `samples` is the length of the ramp in interleaved samples (`frames *
/// channels`), as indexes are; `0` is at once. With `then_cancel`, the voice is
/// taken back when the ramp ends: a fade out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainChange {
    pub voice: u64,
    /// Linear: `1.0` is as scheduled, `0.0` silent.
    pub gain: f32,
    pub samples: usize,
    /// The stream's channel count, so every channel of a frame gets one gain.
    pub channels: usize,
    pub then_cancel: bool,
}

/// A voice's level: where it is going, and how far along it is.
#[derive(Clone, Copy, Debug)]
struct Ramp {
    from: f32,
    to: f32,
    /// Absolute interleaved index the ramp starts at, and how long it is.
    start: usize,
    len: usize,
    channels: usize,
    then_cancel: bool,
    /// The cursor when the voice last had samples waiting, to forget a voice
    /// that has long since played out.
    seen: usize,
}

impl Ramp {
    /// The gain at absolute interleaved index `at`: linear from `from` to `to`
    /// across the ramp, a frame at a time.
    fn at(&self, at: usize) -> f32 {
        if at >= self.start + self.len {
            return self.to;
        }
        if at <= self.start {
            return self.from;
        }
        let channels = self.channels.max(1);
        let frames = (self.len / channels).max(1);
        let frame = (at - self.start) / channels;
        self.from + (self.to - self.from) * (frame as f32 / frames as f32)
    }

    fn done(&self, cursor: usize) -> bool {
        cursor >= self.start + self.len
    }
}

/// Voices to take back or to change the level of, shared between whoever
/// schedules and the mixer.
#[derive(Debug, Default)]
pub struct Voices {
    cancelled: Mutex<Vec<u64>>,
    gains: Mutex<Vec<GainChange>>,
}

impl Voices {
    /// Drops everything `voice` has scheduled that is not yet committed. Picked
    /// up on the mixer's next pass.
    pub fn cancel(&self, voice: u64) {
        self.cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(voice);
    }

    fn take(&self) -> Vec<u64> {
        std::mem::take(&mut *self.cancelled.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Changes a voice's level. Picked up on the mixer's next pass, so it is
    /// heard one buffer later: what has been handed to the device is as it was.
    pub fn gain(&self, change: GainChange) {
        self.gains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(change);
    }

    fn take_gains(&self) -> Vec<GainChange> {
        std::mem::take(&mut *self.gains.lock().unwrap_or_else(PoisonError::into_inner))
    }
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
    /// Voiced samples not yet committed, by voice: each segment's absolute
    /// index and its samples. Summed into `mix` only as they are flushed.
    voices: HashMap<u64, Vec<(usize, Vec<S>)>>,
    /// Voices to take back.
    cancels: Arc<Voices>,
    /// The level of each voice that is not simply as scheduled.
    gains: HashMap<u64, Ramp>,
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
            voices: HashMap::new(),
            cancels: Arc::new(Voices::default()),
            gains: HashMap::new(),
        }
    }

    /// Where voices are cancelled, for handing to whoever schedules them.
    pub fn voices(&self) -> Arc<Voices> {
        Arc::clone(&self.cancels)
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
        self.voices.clear();
        self.gains.clear();
    }

    /// Starts a voice towards a new level from wherever it is now.
    fn apply_gain(&mut self, change: GainChange) {
        let from = self.gains.get(&change.voice).map_or(1.0, |ramp| ramp.at(self.cursor));
        let to = if change.gain.is_finite() { change.gain.max(0.0) } else { 1.0 };
        self.gains.insert(
            change.voice,
            Ramp {
                from,
                to,
                start: self.cursor,
                len: change.samples,
                channels: change.channels,
                then_cancel: change.then_cancel,
                seen: self.cursor,
            },
        );
    }

    /// Sum one command into the accumulation buffer.
    fn apply(&mut self, command: MixCommand<S>) {
        let MixCommand {
            index,
            samples,
            voice,
        } = command;

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

        if let Some(voice) = voice {
            // Kept apart until committed; `mix` is only stretched to cover it.
            self.voices
                .entry(voice)
                .or_default()
                .push((index + skip, samples[skip..].to_vec()));
            return;
        }

        for (slot, sample) in self.mix[start..end].iter_mut().zip(&samples[skip..]) {
            *slot = slot.mix(*sample);
        }
    }

    /// Sums the voiced samples that fall in the next `ready` samples into
    /// `mix`, and forgets the segments that are now wholly committed.
    fn commit_voices(&mut self, ready: usize) {
        let (from, to) = (self.cursor, self.cursor + ready);

        for (voice, segments) in self.voices.iter_mut() {
            let ramp = self.gains.get_mut(voice);
            // A voice at a steady level of one needs no arithmetic at all.
            let level = ramp.as_deref().copied();

            for (index, samples) in segments.iter() {
                let (start, end) = (*index, index + samples.len());
                let (low, high) = (start.max(from), end.min(to));

                match level {
                    None => {
                        for absolute in low..high {
                            let slot = &mut self.mix[absolute - from];
                            *slot = slot.mix(samples[absolute - start]);
                        }
                    }
                    Some(ramp) => {
                        for absolute in low..high {
                            let gain = ramp.at(absolute);
                            let sample = samples[absolute - start];
                            // The float round-trip only where the level is not one.
                            let sample = if gain == 1.0 { sample } else { S::from_f32(sample.to_f32() * gain) };
                            let slot = &mut self.mix[absolute - from];
                            *slot = slot.mix(sample);
                        }
                    }
                }
            }

            segments.retain(|(index, samples)| index + samples.len() > to);
            if let Some(ramp) = ramp {
                ramp.seen = to;
            }
        }

        // A fade out that has run its course takes its voice with it; a ramp
        // that ended back at one, or whose voice played out long ago, is
        // forgotten.
        let voices = &mut self.voices;
        self.gains.retain(|voice, ramp| {
            if !ramp.done(to) {
                return true;
            }
            if ramp.then_cancel {
                voices.remove(voice);
                return false;
            }
            ramp.to != 1.0 && to.saturating_sub(ramp.seen) < MAX_SCHEDULE_AHEAD
        });

        self.voices.retain(|_, segments| !segments.is_empty());
    }

    /// Move the front of [`mix`](Self::mix) into [`output`](Self::output), as
    /// far as there is room. Returns how many samples were committed.
    ///
    /// `output` only ever holds one buffer size, so that is exactly how much can
    /// be in flight, and everything beyond the next callback stays in `mix`
    /// where later commands can still be summed into it. That one buffer size is
    /// therefore the scheduling horizon: a command landing inside the buffer
    /// already handed over is late, and its overlapping part is dropped.
    ///
    /// When there is less in `mix` than room in `output`, the gap goes out as
    /// silence, so the ring is always full and the cursor stays exactly one
    /// buffer ahead of the [`PlayClock`](super::PlayClock). Leaving it to idle
    /// would freeze the cursor while the clock kept counting, and a sound
    /// scheduled off the clock would land however long the output had been
    /// quiet in the future — late, or past [`MAX_SCHEDULE_AHEAD`] and dropped.
    fn flush(&mut self) -> usize {
        let ready = self.output.vacant_len();
        if ready == 0 {
            return 0;
        }
        if self.mix.len() < ready {
            self.mix.resize(ready, S::SILENCE);
        }

        self.commit_voices(ready);

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

        for voice in self.cancels.take() {
            self.voices.remove(&voice);
            self.gains.remove(&voice);
            worked = true;
        }

        for change in self.cancels.take_gains() {
            self.apply_gain(change);
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
    voices: Arc<Voices>,
    thread: Option<JoinHandle<()>>,
}

impl MixerHandle {
    /// Spawns `mixer` on its own thread.
    pub fn spawn<S: SampleType>(mixer: Mixer<S>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let clear = mixer.clear_signal();
        let voices = mixer.voices();

        let thread = thread::Builder::new()
            .name("atome_mixer".to_string())
            .spawn(move || mixer.run(flag))
            .expect("failed to spawn mixer thread");

        MixerHandle {
            running,
            clear,
            voices,
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

    /// Where voices are cancelled.
    pub fn voices(&self) -> Arc<Voices> {
        Arc::clone(&self.voices)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Split;
    use ringbuf::HeapRb;

    fn mixer(output: usize) -> (HeapProd<MixCommand<f32>>, Mixer<f32>, HeapCons<f32>) {
        let (commands, pending) = HeapRb::<MixCommand<f32>>::new(16).split();
        let (audio, device) = HeapRb::<f32>::new(output).split();
        (commands, Mixer::new(pending, audio), device)
    }

    fn command(index: usize, samples: Vec<f32>, voice: Option<u64>) -> MixCommand<f32> {
        MixCommand {
            index,
            samples,
            voice,
        }
    }

    /// A voice sums like anything else when it plays out.
    #[test]
    fn a_voice_is_mixed_as_it_is_committed() {
        let (mut commands, mut mixer, mut device) = mixer(4);
        let _ = commands.try_push(command(0, vec![0.25; 4], None));
        let _ = commands.try_push(command(2, vec![0.5; 2], Some(7)));

        mixer.tick();
        let mut heard = [0.0; 4];
        device.pop_slice(&mut heard);

        assert_eq!(heard, [0.25, 0.25, 0.75, 0.75]);
    }

    /// Cancelled before it is committed, a voice is never heard — and the rest
    /// of the mix is untouched.
    #[test]
    fn a_cancelled_voice_is_taken_back() {
        let (mut commands, mut mixer, mut device) = mixer(4);
        let _ = commands.try_push(command(0, vec![0.25; 8], None));
        let _ = commands.try_push(command(0, vec![0.5; 8], Some(9)));

        mixer.tick();
        let mut first = [0.0; 4];
        device.pop_slice(&mut first);
        assert_eq!(first, [0.75; 4], "committed before the cancel");

        mixer.voices().cancel(9);
        mixer.tick();
        let mut second = [0.0; 4];
        device.pop_slice(&mut second);
        assert_eq!(second, [0.25; 4], "the voice is gone, the rest is not");
    }

    fn gain(voice: u64, gain: f32, samples: usize, then_cancel: bool) -> GainChange {
        GainChange { voice, gain, samples, channels: 2, then_cancel }
    }

    /// A level set at once holds for everything the voice plays after it, and
    /// nothing else in the mix is touched.
    #[test]
    fn a_voice_plays_at_the_level_it_is_set_to() {
        let (mut commands, mut mixer, mut device) = mixer(4);
        let _ = commands.try_push(command(0, vec![0.25; 8], None));
        let _ = commands.try_push(command(0, vec![0.5; 8], Some(3)));
        mixer.voices().gain(gain(3, 0.5, 0, false));

        let mut heard = [0.0; 4];
        for _ in 0..2 {
            mixer.tick();
            device.pop_slice(&mut heard);
            assert_eq!(heard, [0.5; 4], "0.25 and half of 0.5");
        }
    }

    /// A ramp moves a frame at a time, both channels of a frame together, and
    /// ends on the level asked for.
    #[test]
    fn a_ramp_goes_from_where_the_voice_is_to_the_new_level() {
        let (mut commands, mut mixer, mut device) = mixer(16);
        let _ = commands.try_push(command(0, vec![1.0; 16], Some(5)));
        // Four frames of stereo.
        mixer.voices().gain(gain(5, 0.0, 8, false));

        mixer.tick();
        let mut heard = [9.0; 16];
        device.pop_slice(&mut heard);

        assert_eq!(&heard[..8], &[1.0, 1.0, 0.75, 0.75, 0.5, 0.5, 0.25, 0.25]);
        assert_eq!(&heard[8..], &[0.0; 8], "held at the end of the ramp");
    }

    /// A second change mid-ramp starts from the level the first had reached.
    #[test]
    fn a_new_level_mid_ramp_starts_from_where_the_ramp_had_got_to() {
        let (mut commands, mut mixer, mut device) = mixer(4);
        let _ = commands.try_push(command(0, vec![1.0; 12], Some(5)));
        mixer.voices().gain(gain(5, 0.0, 8, false));

        mixer.tick();
        let mut heard = [9.0; 4];
        device.pop_slice(&mut heard);
        assert_eq!(heard, [1.0, 1.0, 0.75, 0.75]);

        // Half way down: back up at once.
        mixer.voices().gain(gain(5, 1.0, 0, false));
        mixer.tick();
        device.pop_slice(&mut heard);
        assert_eq!(heard, [1.0; 4]);
    }

    /// A fade out is a ramp to nothing that takes the voice with it: what the
    /// voice had scheduled past the fade is never heard.
    #[test]
    fn a_fade_out_cancels_the_voice_when_it_ends() {
        let (mut commands, mut mixer, mut device) = mixer(4);
        let _ = commands.try_push(command(0, vec![0.25; 12], None));
        let _ = commands.try_push(command(0, vec![1.0; 12], Some(8)));
        mixer.voices().gain(gain(8, 0.0, 4, true));

        mixer.tick();
        let mut heard = [9.0; 4];
        device.pop_slice(&mut heard);
        assert_eq!(heard, [1.25, 1.25, 0.75, 0.75]);

        // Brought back up, were the voice still there, it would be heard.
        mixer.voices().gain(gain(8, 1.0, 0, false));
        for _ in 0..2 {
            mixer.tick();
            device.pop_slice(&mut heard);
            assert_eq!(heard, [0.25; 4], "the voice went with its fade");
        }
    }

    /// With nothing to play the cursor still keeps pace with the device, so a
    /// sound scheduled just ahead of it after a long quiet stretch is neither
    /// late nor dropped.
    #[test]
    fn an_idle_mixer_keeps_pace_with_the_device() {
        let (mut commands, mut mixer, mut device) = mixer(4);

        // Far longer than MAX_SCHEDULE_AHEAD of silence.
        let quiet = MAX_SCHEDULE_AHEAD * 2;
        let mut heard = [1.0; 4];
        for _ in 0..quiet / 4 {
            mixer.tick();
            assert_eq!(device.pop_slice(&mut heard), 4, "the ring never runs dry");
            assert_eq!(heard, [0.0; 4]);
        }
        mixer.tick();
        assert_eq!(mixer.cursor(), quiet + 4, "one buffer ahead of what was played");

        let _ = commands.try_push(command(quiet + 4, vec![0.5; 4], None));
        mixer.tick();
        device.pop_slice(&mut heard);
        assert_eq!(heard, [0.0; 4], "the buffer already committed");
        mixer.tick();
        device.pop_slice(&mut heard);
        assert_eq!(heard, [0.5; 4]);
        assert_eq!((mixer.late_samples(), mixer.dropped_samples()), (0, 0));
    }
}
