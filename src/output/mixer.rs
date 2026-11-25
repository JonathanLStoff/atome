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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long the mixer sleeps when a pass finds nothing to do, at the least.
const IDLE_POLL: Duration = Duration::from_micros(500);
/// And at the most, however long the output's buffer.
const IDLE_MAX: Duration = Duration::from_millis(5);

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

/// "Weigh each channel of this voice, or bus, by these levels, over this long":
/// a pan, done in the mixer so it is heard within a buffer.
///
/// `levels` has one linear level per output channel; a channel past its end
/// stays at `1.0`. It multiplies the voice's [`GainChange`] level.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelsChange {
    pub voice: u64,
    pub levels: Vec<f32>,
    /// The ramp's length in interleaved samples; `0` is at once.
    pub samples: usize,
    pub channels: usize,
}

/// A voice's per-channel levels: where they are going, and how far along.
#[derive(Clone, Debug)]
struct Weights {
    from: Vec<f32>,
    to: Vec<f32>,
    start: usize,
    len: usize,
    channels: usize,
    seen: usize,
}

impl Weights {
    fn level(levels: &[f32], channel: usize) -> f32 {
        levels.get(channel).copied().unwrap_or(1.0)
    }

    /// The level at absolute interleaved index `at`, for the channel it is on.
    fn at(&self, at: usize) -> f32 {
        let channels = self.channels.max(1);
        let channel = at % channels;
        let to = Self::level(&self.to, channel);
        if at >= self.start + self.len {
            return to;
        }
        let from = Self::level(&self.from, channel);
        if at <= self.start {
            return from;
        }
        let frames = (self.len / channels).max(1);
        let frame = (at - self.start) / channels;
        from + (to - from) * (frame as f32 / frames as f32)
    }

    /// Every channel's level now, to start a new ramp from.
    fn now(&self, at: usize) -> Vec<f32> {
        let channels = self.channels.max(1);
        let frame = at - at % channels;
        (0..channels).map(|channel| self.at(frame + channel)).collect()
    }

    fn unity(&self, cursor: usize) -> bool {
        cursor >= self.start + self.len && self.to.iter().all(|level| *level == 1.0)
    }
}

/// A submix: the voices sent to it are summed apart, run through its effects,
/// brought to its own level and pan (a [`GainChange`] and [`LevelsChange`] on
/// its id, as for a voice), and then summed into the output.
#[derive(Debug)]
struct Bus<S: SampleType> {
    effects: Vec<crate::plugins::Plugin>,
    channels: u16,
    buffer: Vec<S>,
    /// A voice summed into `buffer` this pass.
    fed: bool,
}

impl<S: SampleType> Bus<S> {
    fn new(channels: u16) -> Self {
        Bus { effects: Vec::new(), channels, buffer: Vec::new(), fed: false }
    }
}

/// Voices to take back or to change the level of, shared between whoever
/// schedules and the mixer.
#[derive(Debug, Default)]
pub struct Voices {
    cancelled: Mutex<Vec<u64>>,
    gains: Mutex<Vec<GainChange>>,
    levels: Mutex<Vec<LevelsChange>>,
    /// A voice sent to a bus, or (`None`) back to the output.
    sends: Mutex<Vec<(u64, Option<u64>)>>,
    /// A bus's effects, and the channel count they run at.
    effects: Mutex<Vec<(u64, Vec<crate::plugins::Plugin>, u16)>>,
}

fn take_all<T>(list: &Mutex<Vec<T>>) -> Vec<T> {
    std::mem::take(&mut *list.lock().unwrap_or_else(PoisonError::into_inner))
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

    /// Weighs a voice's (or bus's) channels: a pan. Heard one buffer later.
    pub fn levels(&self, change: LevelsChange) {
        self.levels.lock().unwrap_or_else(PoisonError::into_inner).push(change);
    }

    /// Sends `voice` through `bus` (made on first use), or with `None` straight
    /// to the output again. Send before scheduling: samples already committed
    /// were mixed where the voice went then.
    pub fn send(&self, voice: u64, bus: Option<u64>) {
        self.sends.lock().unwrap_or_else(PoisonError::into_inner).push((voice, bus));
    }

    /// Sets `bus`'s effects, run in order on its sum, `channels` wide (the
    /// output's). An empty list takes them off. [`cancel`](Self::cancel) on a
    /// bus's id removes the bus; its voices go straight to the output.
    pub fn bus_effects(&self, bus: u64, effects: Vec<crate::plugins::Plugin>, channels: u16) {
        self.effects.lock().unwrap_or_else(PoisonError::into_inner).push((bus, effects, channels));
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
    /// Passes run so far ([`MixerHandle::passes`]).
    passes: Arc<AtomicU64>,
    /// An effect run on everything the output plays, as it is committed
    /// ([`MixerHandle::set_master`]), with the output's channel count.
    master: Arc<Mutex<Option<(crate::plugins::Plugin, u16)>>>,
    /// Per-channel levels (pans) of voices and buses, by id.
    weights: HashMap<u64, Weights>,
    /// The bus each voice that has one is sent through, and the cursor when it
    /// last had samples waiting.
    sends: HashMap<u64, (u64, usize)>,
    buses: HashMap<u64, Bus<S>>,
    /// How long a pass with nothing to do sleeps ([`idle_sleep`](Self::idle_sleep)).
    idle: Duration,
}

impl<S: SampleType> Mixer<S> {
    pub fn new(commands: HeapCons<MixCommand<S>>, output: HeapProd<S>) -> Self {
        Mixer {
            idle: IDLE_POLL,
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
            passes: Arc::new(AtomicU64::new(0)),
            master: Arc::new(Mutex::new(None)),
            weights: HashMap::new(),
            sends: HashMap::new(),
            buses: HashMap::new(),
        }
    }

    /// Sleeps a quarter of `buffer` (the output's buffer time) when a pass has
    /// nothing to do, within [`IDLE_POLL`] and [`IDLE_MAX`]: the ring holds one
    /// buffer, so it is topped up at least four times a buffer and never runs
    /// dry, while an idle output wakes the thread hundreds of times a second
    /// instead of thousands.
    pub fn idle_sleep(mut self, buffer: Duration) -> Self {
        self.idle = (buffer / 4).clamp(IDLE_POLL, IDLE_MAX);
        self
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
        self.sends.clear();
        // A bus's level and pan are its settings, not something scheduled.
        // shortcut: a bus effect's tail (a reverb) still rings out after a stop; reset effects if that is heard.
        let buses = &self.buses;
        self.gains.retain(|id, _| buses.contains_key(id));
        self.weights.retain(|id, _| buses.contains_key(id));
    }

    /// Starts a voice's channels towards new levels from wherever they are.
    fn apply_levels(&mut self, change: LevelsChange) {
        let channels = change.channels.max(1);
        let from = self.weights.get(&change.voice).map_or_else(|| vec![1.0; channels], |weights| weights.now(self.cursor));
        let to = change.levels.iter().map(|level| if level.is_finite() { level.max(0.0) } else { 1.0 }).collect();
        self.weights.insert(change.voice, Weights { from, to, start: self.cursor, len: change.samples, channels, seen: self.cursor });
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
    /// `mix` (through their bus, if they have one), and forgets the segments
    /// that are now wholly committed.
    fn commit_voices(&mut self, ready: usize) {
        let (from, to) = (self.cursor, self.cursor + ready);
        for bus in self.buses.values_mut() {
            bus.buffer.clear();
            bus.buffer.resize(ready, S::SILENCE);
            bus.fed = false;
        }

        for (voice, segments) in self.voices.iter_mut() {
            let ramp = self.gains.get_mut(voice);
            // A voice at a steady level of one needs no arithmetic at all.
            let level = ramp.as_deref().copied();
            let weights = self.weights.get_mut(voice);
            let send = self.sends.get_mut(voice);
            let into: &mut [S] = match send.as_ref().and_then(|(bus, _)| self.buses.get_mut(bus)) {
                Some(bus) => {
                    bus.fed = true;
                    &mut bus.buffer
                }
                None => &mut self.mix[..ready],
            };

            for (index, samples) in segments.iter() {
                let (start, end) = (*index, index + samples.len());
                let (low, high) = (start.max(from), end.min(to));

                if level.is_none() && weights.is_none() {
                    for absolute in low..high {
                        let slot = &mut into[absolute - from];
                        *slot = slot.mix(samples[absolute - start]);
                    }
                    continue;
                }
                for absolute in low..high {
                    let mut gain = level.map_or(1.0, |ramp| ramp.at(absolute));
                    if let Some(weights) = weights.as_deref() {
                        gain *= weights.at(absolute);
                    }
                    let sample = samples[absolute - start];
                    // The float round-trip only where the level is not one.
                    let sample = if gain == 1.0 { sample } else { S::from_f32(sample.to_f32() * gain) };
                    let slot = &mut into[absolute - from];
                    *slot = slot.mix(sample);
                }
            }

            segments.retain(|(index, samples)| index + samples.len() > to);
            if let Some(ramp) = ramp {
                ramp.seen = to;
            }
            if let Some(weights) = weights {
                weights.seen = to;
            }
            if let Some(send) = send {
                send.1 = to;
            }
        }

        // Each bus: its effects on its sum (every pass while it has any, so a
        // tail rings out), then its own level and pan, into the output.
        for (id, bus) in self.buses.iter_mut() {
            if !bus.fed && bus.effects.is_empty() {
                continue;
            }
            for effect in &mut bus.effects {
                // A failing effect lets the bus through as it is, never silence.
                let _ = effect.apply(&mut bus.buffer, bus.channels);
            }
            let ramp = self.gains.get_mut(id);
            let weights = self.weights.get_mut(id);
            for (offset, sample) in bus.buffer.iter().enumerate() {
                let absolute = from + offset;
                let mut gain = ramp.as_deref().map_or(1.0, |ramp| ramp.at(absolute));
                if let Some(weights) = weights.as_deref() {
                    gain *= weights.at(absolute);
                }
                let sample = if gain == 1.0 { *sample } else { S::from_f32(sample.to_f32() * gain) };
                let slot = &mut self.mix[offset];
                *slot = slot.mix(sample);
            }
            if let Some(ramp) = ramp {
                ramp.seen = to;
            }
            if let Some(weights) = weights {
                weights.seen = to;
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
        self.weights.retain(|_, weights| !weights.unity(to) && to.saturating_sub(weights.seen) < MAX_SCHEDULE_AHEAD);
        self.sends.retain(|_, (_, seen)| to.saturating_sub(*seen) < MAX_SCHEDULE_AHEAD);

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
        let slot = Arc::clone(&self.master);
        let mut master = slot.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ready = self.output.vacant_len();
        // An effect takes whole frames: what does not fill one waits a pass.
        let frame = master.as_ref().map(|(_, channels)| *channels).or_else(|| self.buses.values().find(|bus| !bus.effects.is_empty()).map(|bus| bus.channels));
        if let Some(channels) = frame {
            let channels = usize::from(channels.max(1));
            ready -= ready % channels;
        }
        if ready == 0 {
            return 0;
        }
        if self.mix.len() < ready {
            self.mix.resize(ready, S::SILENCE);
        }

        self.commit_voices(ready);

        if let Some((plugin, channels)) = master.as_mut() {
            // A failing output effect lets the sound through as it is, never silence.
            let _ = plugin.apply(&mut self.mix[..ready], *channels);
        }
        drop(master);

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
            self.weights.remove(&voice);
            self.sends.remove(&voice);
            self.buses.remove(&voice);
            worked = true;
        }

        for change in self.cancels.take_gains() {
            self.apply_gain(change);
            worked = true;
        }
        for change in take_all(&self.cancels.levels) {
            self.apply_levels(change);
            worked = true;
        }
        for (voice, bus) in take_all(&self.cancels.sends) {
            match bus {
                Some(bus) => {
                    self.buses.entry(bus).or_insert_with(|| Bus::new(1));
                    self.sends.insert(voice, (bus, self.cursor));
                }
                None => {
                    self.sends.remove(&voice);
                }
            }
            worked = true;
        }
        for (bus, effects, channels) in take_all(&self.cancels.effects) {
            let entry = self.buses.entry(bus).or_insert_with(|| Bus::new(channels));
            entry.effects = effects;
            entry.channels = channels;
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
                thread::sleep(self.idle);
            }
            self.passes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Where the output effect is kept ([`MixerHandle::set_master`]).
    pub fn master(&self) -> Arc<Mutex<Option<(crate::plugins::Plugin, u16)>>> {
        Arc::clone(&self.master)
    }

    /// How many passes this mixer has run, counted as it runs: one that stops
    /// rising has stopped mixing, though the stream keeps padding with silence.
    pub fn passes(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.passes)
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
    passes: Arc<AtomicU64>,
    master: Arc<Mutex<Option<(crate::plugins::Plugin, u16)>>>,
    thread: Option<JoinHandle<()>>,
}

impl MixerHandle {
    /// Spawns `mixer` on its own thread.
    pub fn spawn<S: SampleType>(mixer: Mixer<S>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let clear = mixer.clear_signal();
        let voices = mixer.voices();
        let passes = mixer.passes();
        let master = mixer.master();

        let thread = thread::Builder::new()
            .name("atome_mixer".to_string())
            .spawn(move || mixer.run(flag))
            .expect("failed to spawn mixer thread");

        MixerHandle {
            running,
            clear,
            voices,
            passes,
            master,
            thread: Some(thread),
        }
    }

    /// Runs `effect` on everything this output plays, as the mixer commits it,
    /// `channels` wide (the output's): a limiter to protect the speakers, say.
    /// `None` takes it off. Heard from the next buffer.
    pub fn set_master(&self, effect: Option<crate::plugins::Plugin>, channels: u16) {
        *self.master.lock().unwrap_or_else(PoisonError::into_inner) = effect.map(|effect| (effect, channels));
    }

    /// Whether the mixer thread is still running. A thread that panicked or
    /// returned is not, and nothing more will be heard from this output: the
    /// stream keeps playing silence, so its clock alone cannot tell.
    pub fn is_alive(&self) -> bool {
        self.running.load(Ordering::Relaxed) && self.thread.as_ref().is_some_and(|thread| !thread.is_finished())
    }

    /// Passes the mixer has run. A watchdog that sees it stand still while
    /// [`is_alive`](Self::is_alive) holds has a mixer that is stuck, not dead.
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::Relaxed)
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
