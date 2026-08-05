//! atome's own effects: DSP written here, compiled in, and parameterised the
//! same way every other plugin format is.
//!
//! Nothing to install, no Cargo feature, no scanning — these are Rust, and they
//! are the shortest path to a chain that does something.
//!
//! # A note on the name
//!
//! This module is called `atome` inside a crate called `atome`, so importing it
//! shadows the crate name and `atome::` stops meaning the crate root. Reach the
//! rest of the crate through a leading `::` in the same import, as below, or
//! alias the module (`use atome::plugins::atome as effects;`). The examples
//! here take the first route.
//!
//! ```
//! use ::atome::plugins::{atome, ParamError};
//!
//! let mut compressor = atome::compressor(48_000);
//! compressor.set_params_str(r#"{ "threshold_db": -20, "ratio": 6, "attack_ms": 5 }"#)?;
//! # Ok::<(), ParamError>(())
//! ```
//!
//! Or by name, which is what a chain described by a configuration file needs:
//!
//! ```
//! use ::atome::plugins::{atome, ParamError, Params};
//!
//! let params = Params::parse("time_ms: 375, feedback: 0.4, mix: 0.3")?;
//! let delay = atome::create("delay", 48_000, &params)?;
//! # Ok::<(), ParamError>(())
//! ```
//!
//! # What is here
//!
//! | Name | What it does |
//! |------|--------------|
//! | [`gain`] | level, in dB |
//! | [`pan`] | position across the stereo field |
//! | [`width`] | mid/side stereo width |
//! | [`filter`] | resonant low-pass or high-pass |
//! | [`eq`] | three-band shelf/peak/shelf |
//! | [`compressor`] | downward compression with a soft knee |
//! | [`limiter`] | a compressor with the ratio pinned |
//! | [`gate`] | downward expansion below a threshold |
//! | [`saturation`] | `tanh` drive |
//! | [`tremolo`] | amplitude modulation |
//! | [`delay`] | delay line with feedback |
//! | [`chorus`] | modulated short delay, mixed in |
//! | [`flanger`] | as chorus, shorter and with feedback |
//! | [`reverb`] | Schroeder comb/all-pass reverb |
//!
//! [`kinds`] lists them at runtime and [`schema`] describes any one's
//! parameters, so a host can build a menu without knowing this table.
//!
//! # Sample rate
//!
//! Every constructor takes one. Almost everything here derives a coefficient
//! from it — a filter cutoff, an attack time, a delay in milliseconds — and
//! there is nothing to read it from: [`Effect::process`] gets a buffer and a
//! channel count, which is the same signature an internal plugin has always
//! had. Build the effect for the rate of the stream it will sit in.
//!
//! # State, and what cloning does
//!
//! These are stateful: a filter has history, a delay has a line, a compressor
//! has an envelope. So an effect is *owned* by the [`Plugin`] holding it rather
//! than shared, and cloning gives a fresh instance carrying the same parameters
//! with its state reset.
//!
//! That is the behaviour you want. Two devices with "the same" compressor on
//! them are two compressors: sharing one envelope follower between them would
//! make each duck when the other got loud.
//!
//! Channel state is grown on the first block, not at construction, because the
//! channel count is not known until then.

use std::f32::consts::{PI, TAU};

use super::internal::InternalPlugin;
use super::params::{ParamError, ParamSpec, Params, Value};
use super::Plugin;

// -----------------------------------------------------------------------------
//  The trait
// -----------------------------------------------------------------------------

/// A built-in effect: DSP plus the parameters that shape it.
///
/// Implement this to add an effect that behaves like the ones here — named
/// parameters, a schema, and state that resets on clone. A one-off with no
/// parameters does not need it; [`Plugin::internal`] takes a closure.
///
/// `Sync` as well as `Send` because a registered plugin lives in a `static`
/// (see [`internal::register`](super::internal::register)). It costs nothing:
/// every method here takes `&self` or `&mut self`, so an effect needs no
/// interior mutability and is `Sync` by being plain data.
pub trait Effect: Send + Sync + 'static {
    /// What parameters this effect has.
    fn schema(&self) -> Vec<ParamSpec>;

    /// Sets one parameter.
    ///
    /// # Errors
    ///
    /// [`ParamError::Unknown`] for a name this effect does not have,
    /// [`ParamError::Type`] or [`ParamError::Range`] for a value it will not
    /// take.
    fn set(&mut self, key: &str, value: &Value) -> Result<(), ParamError>;

    /// Reads one parameter back. `None` if there is no such parameter.
    fn get(&self, key: &str) -> Option<Value>;

    /// Processes one interleaved block in place.
    fn process(&mut self, buffer: &mut [f32], channels: u16);

    /// The delay this effect introduces, in frames. Zero for all of these:
    /// nothing here looks ahead.
    fn latency(&self) -> usize {
        0
    }

    /// Drops the state without touching the parameters.
    fn reset(&mut self);

    /// A fresh instance with the same parameters and no state.
    fn duplicate(&self) -> Box<dyn Effect>;
}

/// Every parameter of an effect, as a [`Params`].
pub(crate) fn params_of(effect: &dyn Effect) -> Params {
    effect
        .schema()
        .into_iter()
        .filter_map(|spec| effect.get(&spec.name).map(|value| (spec.name, value)))
        .collect()
}

/// Applies a whole set, or none of it.
///
/// Two passes: every name and value is checked against the schema before any
/// of them is written. A half-applied set is worse than a rejected one — it
/// leaves the effect in a state nobody asked for, and the caller who sees the
/// error has no way to know how far it got.
pub(crate) fn apply(effect: &mut dyn Effect, params: &Params) -> Result<(), ParamError> {
    let schema = effect.schema();

    for (key, value) in params.iter() {
        let Some(spec) = schema.iter().find(|spec| spec.name == key) else {
            return Err(ParamError::Unknown {
                key: key.to_string(),
                known: schema.into_iter().map(|spec| spec.name).collect(),
            });
        };
        spec.check(value)?;
    }

    for (key, value) in params.iter() {
        effect.set(key, value)?;
    }

    Ok(())
}

// -----------------------------------------------------------------------------
//  Parameter plumbing
// -----------------------------------------------------------------------------

/// Declares an effect's parameters once, and generates everything that follows
/// from them: the fields, the defaults, the schema, and `set`/`get`.
///
/// Every effect states its parameters in exactly one place. Writing the schema
/// and the `set` match arms by hand would be three lists to keep in step, and
/// the failure mode of them drifting apart is a parameter that silently cannot
/// be set.
macro_rules! parameters {
    (
        $(#[$meta:meta])*
        $name:ident {
            $(
                $field:ident : $default:expr, $min:expr, $max:expr, $unit:literal, $about:literal;
            )*
            $(
                @toggle $flag:ident : $flag_default:expr, $flag_about:literal;
            )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name {
            $(pub $field: f32,)*
            $(pub $flag: bool,)*
        }

        impl Default for $name {
            fn default() -> Self {
                Self {
                    $($field: $default,)*
                    $($flag: $flag_default,)*
                }
            }
        }

        impl $name {
            /// This effect's parameters, in declaration order.
            pub fn schema() -> Vec<ParamSpec> {
                vec![
                    $(ParamSpec::number(
                        stringify!($field), $unit, $min as f64, $max as f64,
                        $default as f64, $about,
                    ),)*
                    $(ParamSpec::toggle(stringify!($flag), $flag_default, $flag_about),)*
                ]
            }

            fn spec_for(key: &str) -> Option<ParamSpec> {
                Self::schema().into_iter().find(|spec| spec.name == key)
            }

            fn names() -> Vec<String> {
                Self::schema().into_iter().map(|spec| spec.name).collect()
            }

            /// Sets one parameter, checked against its spec.
            pub fn set(&mut self, key: &str, value: &Value) -> Result<(), ParamError> {
                let Some(spec) = Self::spec_for(key) else {
                    return Err(ParamError::Unknown {
                        key: key.to_string(),
                        known: Self::names(),
                    });
                };

                let number = spec.check(value)?;

                match key {
                    $(stringify!($field) => self.$field = number as f32,)*
                    $(stringify!($flag) => self.$flag = number != 0.0,)*
                    // `spec_for` already matched, so the two lists agree.
                    _ => unreachable!("'{key}' has a spec but no field"),
                }

                Ok(())
            }

            /// Reads one parameter back.
            pub fn get(&self, key: &str) -> Option<Value> {
                match key {
                    // `from_f32`, not `as f64`: these are f32 fields, and a
                    // listing should show the 1.4 that was set rather than the
                    // 1.399999976158142 that widening produces.
                    $(stringify!($field) => Some(Value::from_f32(self.$field)),)*
                    $(stringify!($flag) => Some(Value::Bool(self.$flag)),)*
                    _ => None,
                }
            }
        }
    };
}

/// Writes the parts of [`Effect`] that follow from a `parameters!` block.
///
/// Only `process` and `reset` differ between effects; everything else is the
/// same delegation, and this is it.
macro_rules! effect_common {
    ($params:ty) => {
        fn schema(&self) -> Vec<ParamSpec> {
            <$params>::schema()
        }

        fn set(&mut self, key: &str, value: &Value) -> Result<(), ParamError> {
            self.params.set(key, value)
        }

        fn get(&self, key: &str) -> Option<Value> {
            self.params.get(key)
        }

        fn duplicate(&self) -> Box<dyn Effect> {
            // Parameters carried over, state left behind — see this module's
            // documentation for why that is the useful direction.
            Box::new(Self::new(self.sample_rate).configured(self.params))
        }
    };
}

// -----------------------------------------------------------------------------
//  Building blocks
// -----------------------------------------------------------------------------

/// Per-channel state, grown to fit on the first block.
///
/// The channel count is not known when an effect is built — it comes from the
/// stream the plugin is attached to, which may not exist yet — so every effect
/// with per-channel state carries one of these instead of a bare `Vec`.
#[derive(Clone, Debug, Default)]
struct PerChannel<T> {
    slots: Vec<T>,
}

impl<T: Clone + Default> PerChannel<T> {
    fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// The slots, grown to `channels` if they were shorter.
    ///
    /// Never shrinks: a chain whose channel count drops and comes back should
    /// not reallocate each time it changes.
    fn get(&mut self, channels: usize) -> &mut [T] {
        if self.slots.len() < channels {
            self.slots.resize(channels, T::default());
        }
        &mut self.slots[..channels]
    }

    fn clear(&mut self) {
        self.slots.clear();
    }
}

/// A transposed direct-form-II biquad.
///
/// Transposed form II because it needs two state variables rather than four
/// and is the better-behaved of the two in single precision.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// Sets the coefficients from an unnormalised biquad, dividing through by
    /// `a0` as the difference equation assumes.
    fn set(&mut self, b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) {
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }

    // The Audio EQ Cookbook forms. `omega` is the normalised angular frequency
    // and `alpha` sets the bandwidth; both are shared by every shape below.

    fn low_pass(&mut self, cutoff_hz: f32, q: f32, sample_rate: f32) {
        let (omega, sin, cos) = angles(cutoff_hz, sample_rate);
        let alpha = sin / (2.0 * q.max(0.01));
        let _ = omega;
        self.set(
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        );
    }

    fn high_pass(&mut self, cutoff_hz: f32, q: f32, sample_rate: f32) {
        let (_, sin, cos) = angles(cutoff_hz, sample_rate);
        let alpha = sin / (2.0 * q.max(0.01));
        self.set(
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        );
    }

    fn peaking(&mut self, centre_hz: f32, q: f32, gain_db: f32, sample_rate: f32) {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let (_, sin, cos) = angles(centre_hz, sample_rate);
        let alpha = sin / (2.0 * q.max(0.01));
        self.set(
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        );
    }

    fn low_shelf(&mut self, corner_hz: f32, gain_db: f32, sample_rate: f32) {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let (_, sin, cos) = angles(corner_hz, sample_rate);
        // A shelf slope of 1, which is the gentlest that does not overshoot.
        let beta = 2.0 * a.sqrt() * (sin / 2.0) * (2.0_f32).sqrt();
        self.set(
            a * ((a + 1.0) - (a - 1.0) * cos + beta),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - beta),
            (a + 1.0) + (a - 1.0) * cos + beta,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - beta,
        );
    }

    fn high_shelf(&mut self, corner_hz: f32, gain_db: f32, sample_rate: f32) {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let (_, sin, cos) = angles(corner_hz, sample_rate);
        let beta = 2.0 * a.sqrt() * (sin / 2.0) * (2.0_f32).sqrt();
        self.set(
            a * ((a + 1.0) + (a - 1.0) * cos + beta),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - beta),
            (a + 1.0) - (a - 1.0) * cos + beta,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - beta,
        );
    }
}

/// `omega`, `sin omega`, `cos omega` for a frequency, clamped below Nyquist.
///
/// Above Nyquist every one of the forms above degenerates — the cookbook
/// assumes `0 < omega < pi` — so the clamp is what keeps a cutoff typed as
/// `40000` from producing coefficients that blow up rather than a filter that
/// passes everything.
fn angles(frequency_hz: f32, sample_rate: f32) -> (f32, f32, f32) {
    let nyquist = sample_rate / 2.0;
    let frequency = frequency_hz.clamp(1.0, nyquist * 0.99);
    let omega = TAU * frequency / sample_rate;
    (omega, omega.sin(), omega.cos())
}

/// A fractional-delay line with linear interpolation.
///
/// Interpolated because the modulated effects — chorus, flanger — sweep the
/// delay continuously, and stepping it a whole sample at a time is audible as
/// a click on every step.
#[derive(Clone, Debug, Default)]
struct DelayLine {
    buffer: Vec<f32>,
    write: usize,
}

impl DelayLine {
    /// A line long enough for `frames` of delay.
    fn with_capacity(frames: usize) -> Self {
        Self {
            // One spare, so a delay of exactly `frames` reads the oldest sample
            // rather than the one about to be overwritten.
            buffer: vec![0.0; frames.max(1) + 1],
            write: 0,
        }
    }

    fn push(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write = (self.write + 1) % self.buffer.len();
    }

    /// The sample `delay` frames back, interpolated between its neighbours.
    fn read(&self, delay: f32) -> f32 {
        let length = self.buffer.len();
        let delay = delay.clamp(0.0, (length - 1) as f32);

        let whole = delay.floor() as usize;
        let fraction = delay - whole as f32;

        // `+ length` before the subtraction: these are unsigned, and the read
        // point is usually behind the write point in modular terms.
        let first = (self.write + length - whole - 1) % length;
        let second = (first + length - 1) % length;

        self.buffer[first] * (1.0 - fraction) + self.buffer[second] * fraction
    }

}

/// Converts decibels to a linear multiplier.
fn from_db(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Converts a linear multiplier to decibels, with a floor so silence is a
/// number rather than `-inf`.
fn to_db(linear: f32) -> f32 {
    20.0 * linear.abs().max(1.0e-9).log10()
}

/// The per-sample coefficient for a one-pole smoother reaching 1/e in `ms`.
///
/// Zero and negative times give 1.0 — an instant response — rather than a
/// division by zero.
fn time_coefficient(ms: f32, sample_rate: f32) -> f32 {
    if ms <= 0.0 {
        return 1.0;
    }
    1.0 - (-1.0 / (ms * 0.001 * sample_rate)).exp()
}

// -----------------------------------------------------------------------------
//  Gain
// -----------------------------------------------------------------------------

parameters! {
    /// [`gain`]'s parameters.
    GainParams {
        gain_db: 0.0, -60.0, 24.0, "dB", "level change applied to every channel";
        @toggle mute: false, "silence the signal entirely";
    }
}

/// Level, in dB.
pub struct GainEffect {
    params: GainParams,
    sample_rate: u32,
}

impl GainEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: GainParams::default(),
            sample_rate,
        }
    }

    fn configured(mut self, params: GainParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for GainEffect {
    effect_common!(GainParams);

    fn process(&mut self, buffer: &mut [f32], _channels: u16) {
        let scale = if self.params.mute {
            0.0
        } else {
            from_db(self.params.gain_db)
        };

        for sample in buffer {
            *sample *= scale;
        }
    }

    fn reset(&mut self) {}
}

// -----------------------------------------------------------------------------
//  Pan
// -----------------------------------------------------------------------------

parameters! {
    /// [`pan`]'s parameters.
    PanParams {
        pan: 0.0, -1.0, 1.0, "", "-1 is hard left, 0 centre, 1 hard right";
    }
}

/// Position across the stereo field, with a constant-power law.
///
/// Constant power rather than linear: a linear pan is 6 dB down in the middle,
/// which is audible as a dip as a source sweeps across.
///
/// Only meaningful for two channels. With any other count this passes the
/// audio through — there is no sensible reading of "pan" for five channels, and
/// guessing one would be worse than doing nothing.
pub struct PanEffect {
    params: PanParams,
    sample_rate: u32,
}

impl PanEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: PanParams::default(),
            sample_rate,
        }
    }

    fn configured(mut self, params: PanParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for PanEffect {
    effect_common!(PanParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        if channels != 2 {
            return;
        }

        // -1..=1 onto 0..=pi/2, so the two gains are cos and sin of one angle
        // and their squares sum to 1.
        let angle = (self.params.pan.clamp(-1.0, 1.0) + 1.0) * 0.25 * PI;
        let (left, right) = (angle.cos(), angle.sin());

        for frame in buffer.chunks_mut(2) {
            frame[0] *= left;
            frame[1] *= right;
        }
    }

    fn reset(&mut self) {}
}

// -----------------------------------------------------------------------------
//  Stereo width
// -----------------------------------------------------------------------------

parameters! {
    /// [`width`]'s parameters.
    WidthParams {
        width: 1.0, 0.0, 2.0, "", "0 is mono, 1 unchanged, 2 twice as wide";
    }
}

/// Mid/side width. Two channels only, on the same reasoning as [`PanEffect`].
pub struct WidthEffect {
    params: WidthParams,
    sample_rate: u32,
}

impl WidthEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: WidthParams::default(),
            sample_rate,
        }
    }

    fn configured(mut self, params: WidthParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for WidthEffect {
    effect_common!(WidthParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        if channels != 2 {
            return;
        }

        let width = self.params.width;

        for frame in buffer.chunks_mut(2) {
            let mid = (frame[0] + frame[1]) * 0.5;
            let side = (frame[0] - frame[1]) * 0.5 * width;
            frame[0] = mid + side;
            frame[1] = mid - side;
        }
    }

    fn reset(&mut self) {}
}

// -----------------------------------------------------------------------------
//  Filter
// -----------------------------------------------------------------------------

parameters! {
    /// [`filter`]'s parameters.
    FilterParams {
        cutoff_hz: 1000.0, 20.0, 20000.0, "Hz", "corner frequency";
        resonance: 0.707, 0.1, 20.0, "Q", "peak at the corner; 0.707 is flat";
        @toggle high_pass: false, "high-pass instead of low-pass";
    }
}

/// A resonant low-pass or high-pass.
pub struct FilterEffect {
    params: FilterParams,
    applied: Option<FilterParams>,
    filters: PerChannel<Biquad>,
    sample_rate: u32,
}

impl FilterEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: FilterParams::default(),
            applied: None,
            filters: PerChannel::new(),
            sample_rate,
        }
    }

    fn configured(mut self, params: FilterParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for FilterEffect {
    effect_common!(FilterParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let params = self.params;
        let filters = self.filters.get(channels);

        // Coefficients cost a handful of transcendentals, so they are computed
        // when a parameter moves rather than every block. The state is not
        // touched: a coefficient change mid-stream should bend the filter, not
        // restart it.
        if self.applied != Some(params) {
            for filter in filters.iter_mut() {
                if params.high_pass {
                    filter.high_pass(params.cutoff_hz, params.resonance, rate);
                } else {
                    filter.low_pass(params.cutoff_hz, params.resonance, rate);
                }
            }
            self.applied = Some(params);
        }

        for frame in buffer.chunks_mut(channels) {
            for (sample, filter) in frame.iter_mut().zip(filters.iter_mut()) {
                *sample = filter.process(*sample);
            }
        }
    }

    fn reset(&mut self) {
        self.filters.clear();
        // The new channels will need coefficients, and `applied` is what says
        // whether they have them.
        self.applied = None;
    }
}

// -----------------------------------------------------------------------------
//  EQ
// -----------------------------------------------------------------------------

parameters! {
    /// [`eq`]'s parameters.
    EqParams {
        low_gain_db: 0.0, -24.0, 24.0, "dB", "low shelf gain";
        low_freq_hz: 200.0, 20.0, 2000.0, "Hz", "low shelf corner";
        mid_gain_db: 0.0, -24.0, 24.0, "dB", "mid peak gain";
        mid_freq_hz: 1000.0, 100.0, 10000.0, "Hz", "mid peak centre";
        mid_q: 0.7, 0.1, 10.0, "Q", "mid peak width; higher is narrower";
        high_gain_db: 0.0, -24.0, 24.0, "dB", "high shelf gain";
        high_freq_hz: 4000.0, 1000.0, 20000.0, "Hz", "high shelf corner";
    }
}

/// Three bands: low shelf, peaking mid, high shelf.
pub struct EqEffect {
    params: EqParams,
    applied: Option<EqParams>,
    bands: PerChannel<[Biquad; 3]>,
    sample_rate: u32,
}

impl EqEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: EqParams::default(),
            applied: None,
            bands: PerChannel::new(),
            sample_rate,
        }
    }

    fn configured(mut self, params: EqParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for EqEffect {
    effect_common!(EqParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let params = self.params;
        let bands = self.bands.get(channels);

        if self.applied != Some(params) {
            for band in bands.iter_mut() {
                band[0].low_shelf(params.low_freq_hz, params.low_gain_db, rate);
                band[1].peaking(params.mid_freq_hz, params.mid_q, params.mid_gain_db, rate);
                band[2].high_shelf(params.high_freq_hz, params.high_gain_db, rate);
            }
            self.applied = Some(params);
        }

        for frame in buffer.chunks_mut(channels) {
            for (sample, band) in frame.iter_mut().zip(bands.iter_mut()) {
                let mut value = *sample;
                for filter in band.iter_mut() {
                    value = filter.process(value);
                }
                *sample = value;
            }
        }
    }

    fn reset(&mut self) {
        self.bands.clear();
        self.applied = None;
    }
}

// -----------------------------------------------------------------------------
//  Compressor
// -----------------------------------------------------------------------------

parameters! {
    /// [`compressor`]'s parameters.
    CompressorParams {
        threshold_db: -18.0, -60.0, 0.0, "dB", "level above which gain reduction starts";
        ratio: 4.0, 1.0, 20.0, "", "input-to-output ratio above the threshold";
        attack_ms: 10.0, 0.1, 200.0, "ms", "how fast it reacts to a rise";
        release_ms: 120.0, 5.0, 2000.0, "ms", "how fast it lets go";
        knee_db: 6.0, 0.0, 24.0, "dB", "width of the soft knee around the threshold";
        makeup_db: 0.0, -12.0, 24.0, "dB", "level added after the reduction";
    }
}

/// Downward compression with a soft knee.
///
/// The detector is one envelope for every channel rather than one each, so a
/// stereo pair ducks together. Compressing the two independently makes the
/// image wander whenever one side is louder — a hard-panned snare pulls the
/// whole mix sideways.
pub struct CompressorEffect {
    params: CompressorParams,
    /// Envelope of the detector, in dB, shared across channels.
    envelope_db: f32,
    /// Gain reduction currently applied, in dB. Negative or zero.
    reduction_db: f32,
    sample_rate: u32,
}

impl CompressorEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: CompressorParams::default(),
            envelope_db: -120.0,
            reduction_db: 0.0,
            sample_rate,
        }
    }

    fn configured(mut self, params: CompressorParams) -> Self {
        self.params = params;
        self
    }

    /// How much reduction the curve asks for at an input level, in dB.
    ///
    /// Three regions: below the knee nothing happens, above it the excess is
    /// divided by the ratio, and across the knee the two are joined by the
    /// quadratic that matches both value and slope at each end.
    fn curve(&self, level_db: f32) -> f32 {
        let CompressorParams {
            threshold_db,
            ratio,
            knee_db,
            ..
        } = self.params;

        let over = level_db - threshold_db;

        if knee_db > 0.0 && over > -knee_db / 2.0 && over < knee_db / 2.0 {
            let x = over + knee_db / 2.0;
            return -(1.0 - 1.0 / ratio) * x * x / (2.0 * knee_db);
        }

        if over <= 0.0 {
            return 0.0;
        }

        -over * (1.0 - 1.0 / ratio)
    }
}

impl Effect for CompressorEffect {
    effect_common!(CompressorParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let attack = time_coefficient(self.params.attack_ms, rate);
        let release = time_coefficient(self.params.release_ms, rate);
        let makeup = from_db(self.params.makeup_db);

        for frame in buffer.chunks_mut(channels) {
            // The loudest channel drives the detector, so a peak anywhere in
            // the frame is caught rather than averaged away.
            let peak = frame.iter().fold(0.0_f32, |loudest, sample| {
                loudest.max(sample.abs())
            });
            self.envelope_db = to_db(peak);

            let wanted = self.curve(self.envelope_db);

            // Attack when the reduction is deepening, release when it is
            // letting go — the asymmetry is the whole character of a
            // compressor.
            let coefficient = if wanted < self.reduction_db {
                attack
            } else {
                release
            };
            self.reduction_db += coefficient * (wanted - self.reduction_db);

            let scale = from_db(self.reduction_db) * makeup;
            for sample in frame.iter_mut() {
                *sample *= scale;
            }
        }
    }

    fn reset(&mut self) {
        self.envelope_db = -120.0;
        self.reduction_db = 0.0;
    }
}

// -----------------------------------------------------------------------------
//  Limiter
// -----------------------------------------------------------------------------

parameters! {
    /// [`limiter`]'s parameters.
    LimiterParams {
        ceiling_db: -1.0, -24.0, 0.0, "dB", "level the output is held under";
        release_ms: 50.0, 1.0, 1000.0, "ms", "how fast it lets go";
    }
}

/// A compressor with the ratio pinned and the attack immediate.
///
/// Not a look-ahead limiter: with no delay to look down, a sample that jumps
/// straight past the ceiling is over it for the one sample before the detector
/// catches up. That is what the [`saturation`] shape is for, and why a real
/// mastering limiter reports latency and this one reports none.
pub struct LimiterEffect {
    params: LimiterParams,
    reduction_db: f32,
    sample_rate: u32,
}

impl LimiterEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: LimiterParams::default(),
            reduction_db: 0.0,
            sample_rate,
        }
    }

    fn configured(mut self, params: LimiterParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for LimiterEffect {
    effect_common!(LimiterParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let release = time_coefficient(self.params.release_ms, self.sample_rate as f32);
        let ceiling = self.params.ceiling_db;

        for frame in buffer.chunks_mut(channels) {
            let peak = frame
                .iter()
                .fold(0.0_f32, |loudest, sample| loudest.max(sample.abs()));
            let over = to_db(peak) - ceiling;
            let wanted = if over > 0.0 { -over } else { 0.0 };

            // Instant attack, smoothed release.
            self.reduction_db = if wanted < self.reduction_db {
                wanted
            } else {
                self.reduction_db + release * (wanted - self.reduction_db)
            };

            let scale = from_db(self.reduction_db);
            for sample in frame.iter_mut() {
                *sample *= scale;
            }
        }
    }

    fn reset(&mut self) {
        self.reduction_db = 0.0;
    }
}

// -----------------------------------------------------------------------------
//  Gate
// -----------------------------------------------------------------------------

parameters! {
    /// [`gate`]'s parameters.
    GateParams {
        threshold_db: -40.0, -90.0, 0.0, "dB", "level below which the signal is cut";
        attack_ms: 1.0, 0.1, 100.0, "ms", "how fast it opens";
        hold_ms: 50.0, 0.0, 1000.0, "ms", "how long it stays open after falling below";
        release_ms: 100.0, 1.0, 2000.0, "ms", "how fast it closes";
        floor_db: -80.0, -120.0, 0.0, "dB", "level the closed gate attenuates to";
    }
}

/// Cuts what is below a threshold, with a hold so it does not chatter.
///
/// The hold is the part that matters: a gate without one opens and closes on
/// every cycle of a signal sitting near the threshold, which is far more
/// audible than the noise it was meant to remove.
pub struct GateEffect {
    params: GateParams,
    /// 0 closed, 1 open.
    gain: f32,
    /// Frames left to hold open.
    holding: usize,
    sample_rate: u32,
}

impl GateEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: GateParams::default(),
            gain: 0.0,
            holding: 0,
            sample_rate,
        }
    }

    fn configured(mut self, params: GateParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for GateEffect {
    effect_common!(GateParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let attack = time_coefficient(self.params.attack_ms, rate);
        let release = time_coefficient(self.params.release_ms, rate);
        let hold_frames = (self.params.hold_ms * 0.001 * rate) as usize;
        let floor = from_db(self.params.floor_db);

        for frame in buffer.chunks_mut(channels) {
            let peak = frame
                .iter()
                .fold(0.0_f32, |loudest, sample| loudest.max(sample.abs()));

            if to_db(peak) >= self.params.threshold_db {
                self.holding = hold_frames;
            } else {
                self.holding = self.holding.saturating_sub(1);
            }

            let open = self.holding > 0;
            let coefficient = if open { attack } else { release };
            self.gain += coefficient * (if open { 1.0 } else { 0.0 } - self.gain);

            // The floor rather than zero, so a closed gate ducks rather than
            // mutes — a hard mute is more noticeable than the noise.
            let scale = floor + (1.0 - floor) * self.gain;
            for sample in frame.iter_mut() {
                *sample *= scale;
            }
        }
    }

    fn reset(&mut self) {
        self.gain = 0.0;
        self.holding = 0;
    }
}

// -----------------------------------------------------------------------------
//  Saturation
// -----------------------------------------------------------------------------

parameters! {
    /// [`saturation`]'s parameters.
    SaturationParams {
        drive: 2.0, 1.0, 50.0, "", "how hard the signal is pushed into the curve";
        mix: 1.0, 0.0, 1.0, "", "0 is dry, 1 is fully saturated";
        output_db: 0.0, -24.0, 12.0, "dB", "level after the curve";
    }
}

/// `tanh` saturation.
///
/// Bounded by ±1 however hard it is driven, so this cannot produce a sample
/// that clips the device. Divided through by `tanh(drive)` so turning the
/// drive up adds harmonics rather than level.
pub struct SaturationEffect {
    params: SaturationParams,
    sample_rate: u32,
}

impl SaturationEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: SaturationParams::default(),
            sample_rate,
        }
    }

    fn configured(mut self, params: SaturationParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for SaturationEffect {
    effect_common!(SaturationParams);

    fn process(&mut self, buffer: &mut [f32], _channels: u16) {
        let drive = self.params.drive;
        let normalise = 1.0 / drive.tanh();
        let mix = self.params.mix;
        let output = from_db(self.params.output_db);

        for sample in buffer {
            let wet = (*sample * drive).tanh() * normalise;
            *sample = (*sample * (1.0 - mix) + wet * mix) * output;
        }
    }

    fn reset(&mut self) {}
}

// -----------------------------------------------------------------------------
//  Tremolo
// -----------------------------------------------------------------------------

parameters! {
    /// [`tremolo`]'s parameters.
    TremoloParams {
        rate_hz: 4.0, 0.05, 20.0, "Hz", "cycles per second";
        depth: 0.5, 0.0, 1.0, "", "0 is no modulation, 1 is down to silence";
    }
}

/// Amplitude modulation.
///
/// The phase advances once per frame rather than once per sample, so every
/// channel is modulated together — advancing per sample would run the LFO
/// `channels` times too fast and put the channels out of phase with each other.
pub struct TremoloEffect {
    params: TremoloParams,
    phase: f32,
    sample_rate: u32,
}

impl TremoloEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: TremoloParams::default(),
            phase: 0.0,
            sample_rate,
        }
    }

    fn configured(mut self, params: TremoloParams) -> Self {
        self.params = params;
        self
    }
}

impl Effect for TremoloEffect {
    effect_common!(TremoloParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let step = self.params.rate_hz / self.sample_rate as f32;
        let depth = self.params.depth;

        for frame in buffer.chunks_mut(channels) {
            // 1.0 at the top of the cycle, `1 - depth` at the bottom.
            let lfo = 1.0 - depth * 0.5 * (1.0 - (self.phase * TAU).cos());
            for sample in frame.iter_mut() {
                *sample *= lfo;
            }
            self.phase = (self.phase + step).fract();
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

// -----------------------------------------------------------------------------
//  Delay
// -----------------------------------------------------------------------------

parameters! {
    /// [`delay`]'s parameters.
    DelayParams {
        time_ms: 250.0, 1.0, 2000.0, "ms", "how far back the echo comes from";
        feedback: 0.35, 0.0, 0.95, "", "how much of the echo is fed back in";
        mix: 0.3, 0.0, 1.0, "", "0 is dry, 1 is echo only";
        damping_hz: 8000.0, 200.0, 20000.0, "Hz", "each repeat is filtered above this";
    }
}

/// A delay line with feedback and a damped repeat.
///
/// `feedback` stops at 0.95 rather than 1.0 on purpose: at 1.0 the loop has no
/// loss and never decays, and a fraction over it grows without bound.
pub struct DelayEffect {
    params: DelayParams,
    lines: PerChannel<DelayLine>,
    dampers: PerChannel<Biquad>,
    applied_damping: Option<f32>,
    /// The longest delay a line was built for, in frames.
    capacity: usize,
    sample_rate: u32,
}

impl DelayEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: DelayParams::default(),
            lines: PerChannel::new(),
            dampers: PerChannel::new(),
            applied_damping: None,
            capacity: 0,
            sample_rate,
        }
    }

    fn configured(mut self, params: DelayParams) -> Self {
        self.params = params;
        self
    }

    /// The line length to build for, in frames.
    ///
    /// The parameter's own maximum rather than its current value, so moving
    /// `time_ms` never has to reallocate — which would otherwise mean an
    /// allocation on the audio thread every time the delay was turned up.
    fn line_frames(&self) -> usize {
        let max_ms = DelayParams::schema()
            .into_iter()
            .find(|spec| spec.name == "time_ms")
            .map(|spec| spec.max as f32)
            .unwrap_or(2000.0);

        (max_ms * 0.001 * self.sample_rate as f32).ceil() as usize
    }
}

impl Effect for DelayEffect {
    effect_common!(DelayParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let wanted = self.line_frames();
        if self.capacity != wanted {
            self.lines.clear();
            self.capacity = wanted;
        }

        let params = self.params;
        let delay_frames = params.time_ms * 0.001 * rate;

        // `PerChannel::get` defaults new slots, and a defaulted `DelayLine` has
        // no room in it — so newly grown ones are sized here.
        let lines = self.lines.get(channels);
        for line in lines.iter_mut() {
            if line.buffer.len() <= wanted {
                *line = DelayLine::with_capacity(wanted);
            }
        }

        let dampers = self.dampers.get(channels);
        if self.applied_damping != Some(params.damping_hz) {
            for damper in dampers.iter_mut() {
                damper.low_pass(params.damping_hz, 0.707, rate);
            }
            self.applied_damping = Some(params.damping_hz);
        }

        let lines = self.lines.get(channels);

        for frame in buffer.chunks_mut(channels) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let echo = lines[channel].read(delay_frames);
                let damped = self.dampers.slots[channel].process(echo);

                lines[channel].push(*sample + damped * params.feedback);
                *sample = *sample * (1.0 - params.mix) + echo * params.mix;
            }
        }
    }

    fn reset(&mut self) {
        self.lines.clear();
        self.dampers.clear();
        self.applied_damping = None;
    }
}

// -----------------------------------------------------------------------------
//  Chorus
// -----------------------------------------------------------------------------

parameters! {
    /// [`chorus`]'s parameters.
    ChorusParams {
        rate_hz: 0.8, 0.01, 10.0, "Hz", "how fast the delay is swept";
        depth_ms: 3.0, 0.1, 20.0, "ms", "how far the delay is swept";
        delay_ms: 12.0, 1.0, 50.0, "ms", "the delay the sweep is centred on";
        mix: 0.5, 0.0, 1.0, "", "0 is dry, 1 is the modulated copy only";
        spread: 1.0, 0.0, 1.0, "", "how far apart the channels' LFOs are";
    }
}

/// A short delay swept by an LFO and mixed back in.
///
/// `spread` offsets each channel's LFO around the cycle, which is what makes it
/// sound wide rather than like one voice wobbling in the middle.
pub struct ChorusEffect {
    params: ChorusParams,
    lines: PerChannel<DelayLine>,
    phase: f32,
    capacity: usize,
    sample_rate: u32,
}

impl ChorusEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: ChorusParams::default(),
            lines: PerChannel::new(),
            phase: 0.0,
            capacity: 0,
            sample_rate,
        }
    }

    fn configured(mut self, params: ChorusParams) -> Self {
        self.params = params;
        self
    }

    /// Room for the longest centre delay plus the deepest sweep either side.
    fn line_frames(&self) -> usize {
        let longest_ms = 50.0 + 20.0;
        (longest_ms * 0.001 * self.sample_rate as f32).ceil() as usize + 2
    }
}

impl Effect for ChorusEffect {
    effect_common!(ChorusParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let wanted = self.line_frames();
        if self.capacity != wanted {
            self.lines.clear();
            self.capacity = wanted;
        }

        let params = self.params;
        let centre = params.delay_ms * 0.001 * rate;
        let sweep = params.depth_ms * 0.001 * rate;
        let step = params.rate_hz / rate;

        let lines = self.lines.get(channels);
        for line in lines.iter_mut() {
            if line.buffer.len() <= wanted {
                *line = DelayLine::with_capacity(wanted);
            }
        }

        for frame in buffer.chunks_mut(channels) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                // Each channel sits a fraction of a cycle further round.
                let offset = if channels > 1 {
                    params.spread * channel as f32 / channels as f32
                } else {
                    0.0
                };
                let lfo = ((self.phase + offset).fract() * TAU).sin();
                let delay = (centre + lfo * sweep).max(1.0);

                let wet = lines[channel].read(delay);
                lines[channel].push(*sample);
                *sample = *sample * (1.0 - params.mix) + wet * params.mix;
            }

            self.phase = (self.phase + step).fract();
        }
    }

    fn reset(&mut self) {
        self.lines.clear();
        self.phase = 0.0;
    }
}

// -----------------------------------------------------------------------------
//  Flanger
// -----------------------------------------------------------------------------

parameters! {
    /// [`flanger`]'s parameters.
    FlangerParams {
        rate_hz: 0.3, 0.01, 10.0, "Hz", "how fast the delay is swept";
        depth_ms: 2.0, 0.1, 10.0, "ms", "how far the delay is swept";
        delay_ms: 3.0, 0.1, 20.0, "ms", "the delay the sweep is centred on";
        feedback: 0.5, -0.95, 0.95, "", "resonance; negative inverts the comb";
        mix: 0.5, 0.0, 1.0, "", "0 is dry, 1 is the swept copy only";
    }
}

/// A chorus with a shorter delay and feedback.
///
/// The short delay is what makes it a flanger: at a few milliseconds the dry
/// and delayed copies comb-filter each other audibly, and sweeping that comb is
/// the sound. Feedback sharpens the notches, and a negative value flips them to
/// peaks.
pub struct FlangerEffect {
    params: FlangerParams,
    lines: PerChannel<DelayLine>,
    phase: f32,
    capacity: usize,
    sample_rate: u32,
}

impl FlangerEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: FlangerParams::default(),
            lines: PerChannel::new(),
            phase: 0.0,
            capacity: 0,
            sample_rate,
        }
    }

    fn configured(mut self, params: FlangerParams) -> Self {
        self.params = params;
        self
    }

    fn line_frames(&self) -> usize {
        ((20.0 + 10.0) * 0.001 * self.sample_rate as f32).ceil() as usize + 2
    }
}

impl Effect for FlangerEffect {
    effect_common!(FlangerParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let wanted = self.line_frames();
        if self.capacity != wanted {
            self.lines.clear();
            self.capacity = wanted;
        }

        let params = self.params;
        let centre = params.delay_ms * 0.001 * rate;
        let sweep = params.depth_ms * 0.001 * rate;
        let step = params.rate_hz / rate;

        let lines = self.lines.get(channels);
        for line in lines.iter_mut() {
            if line.buffer.len() <= wanted {
                *line = DelayLine::with_capacity(wanted);
            }
        }

        for frame in buffer.chunks_mut(channels) {
            let lfo = (self.phase * TAU).sin();
            let delay = (centre + lfo * sweep).max(1.0);

            for (channel, sample) in frame.iter_mut().enumerate() {
                let wet = lines[channel].read(delay);
                lines[channel].push(*sample + wet * params.feedback);
                *sample = *sample * (1.0 - params.mix) + wet * params.mix;
            }

            self.phase = (self.phase + step).fract();
        }
    }

    fn reset(&mut self) {
        self.lines.clear();
        self.phase = 0.0;
    }
}

// -----------------------------------------------------------------------------
//  Reverb
// -----------------------------------------------------------------------------

parameters! {
    /// [`reverb`]'s parameters.
    ReverbParams {
        room_size: 0.5, 0.0, 1.0, "", "how long the tail rings for";
        damping: 0.5, 0.0, 1.0, "", "how fast the high end dies away";
        mix: 0.3, 0.0, 1.0, "", "0 is dry, 1 is tail only";
        width: 1.0, 0.0, 1.0, "", "how far apart the channels' tails are";
        pre_delay_ms: 0.0, 0.0, 200.0, "ms", "gap before the tail starts";
    }
}

/// Eight comb filters into four all-passes, per channel.
///
/// The Schroeder arrangement, in the proportions Freeverb uses: the combs build
/// density and the all-passes smear what is left of the pattern. The comb
/// lengths are mutually prime so their echoes do not line up into a pitch, and
/// each channel's are offset so the two tails are not the same tail twice.
pub struct ReverbEffect {
    params: ReverbParams,
    channels: PerChannel<ReverbChannel>,
    pre_delay: PerChannel<DelayLine>,
    sample_rate: u32,
}

/// One channel's combs and all-passes.
#[derive(Clone, Debug, Default)]
struct ReverbChannel {
    combs: Vec<Comb>,
    allpasses: Vec<AllPass>,
}

#[derive(Clone, Debug, Default)]
struct Comb {
    line: DelayLine,
    length: usize,
    /// One-pole low-pass inside the loop — this is what `damping` controls.
    filtered: f32,
}

#[derive(Clone, Debug, Default)]
struct AllPass {
    line: DelayLine,
    length: usize,
}

/// Comb lengths in frames at 44.1 kHz, from Freeverb. Scaled to the actual
/// rate so the room is the same size whatever the stream runs at.
const COMB_LENGTHS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_LENGTHS: [usize; 4] = [556, 441, 341, 225];
/// How far each successive channel's lines are offset, in frames at 44.1 kHz.
const STEREO_SPREAD: usize = 23;

impl ReverbEffect {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            params: ReverbParams::default(),
            channels: PerChannel::new(),
            pre_delay: PerChannel::new(),
            sample_rate,
        }
    }

    fn configured(mut self, params: ReverbParams) -> Self {
        self.params = params;
        self
    }

    /// Builds one channel's filters, offset by its index.
    ///
    /// Free of `self` so it can be called while the channel slots are borrowed
    /// mutably, which is where it is needed.
    fn build_channel(sample_rate: u32, channel: usize) -> ReverbChannel {
        let scale = sample_rate as f32 / 44_100.0;
        let offset = channel * STEREO_SPREAD;

        let sized = |base: usize| -> usize {
            (((base + offset) as f32) * scale).round().max(1.0) as usize
        };

        ReverbChannel {
            combs: COMB_LENGTHS
                .iter()
                .map(|base| {
                    let length = sized(*base);
                    Comb {
                        line: DelayLine::with_capacity(length),
                        length,
                        filtered: 0.0,
                    }
                })
                .collect(),
            allpasses: ALLPASS_LENGTHS
                .iter()
                .map(|base| {
                    let length = sized(*base);
                    AllPass {
                        line: DelayLine::with_capacity(length),
                        length,
                    }
                })
                .collect(),
        }
    }
}

impl Effect for ReverbEffect {
    effect_common!(ReverbParams);

    fn process(&mut self, buffer: &mut [f32], channels: u16) {
        let channel_count = channels as usize;
        if channel_count == 0 {
            return;
        }

        let rate = self.sample_rate as f32;
        let params = self.params;

        // Freeverb's mapping: a room of 1.0 leaves a feedback just under unity,
        // which rings for a long time without running away.
        let feedback = 0.7 + params.room_size * 0.28;
        let damping = params.damping * 0.4;
        let pre_delay_frames = params.pre_delay_ms * 0.001 * rate;

        // Built here rather than in `new` because the channel count is not
        // known until now.
        {
            let sample_rate = self.sample_rate;
            let slots = self.channels.get(channel_count);
            for (channel, slot) in slots.iter_mut().enumerate() {
                if slot.combs.is_empty() {
                    *slot = Self::build_channel(sample_rate, channel);
                }
            }
        }

        let pre_frames = ((200.0 * 0.001 * rate) as usize).max(1);
        let pre = self.pre_delay.get(channel_count);
        for line in pre.iter_mut() {
            if line.buffer.len() <= pre_frames {
                *line = DelayLine::with_capacity(pre_frames);
            }
        }

        for frame in buffer.chunks_mut(channel_count) {
            for (index, sample) in frame.iter_mut().enumerate() {
                let dry = *sample;

                let input = if pre_delay_frames > 0.0 {
                    self.pre_delay.slots[index].push(dry);
                    self.pre_delay.slots[index].read(pre_delay_frames)
                } else {
                    dry
                };

                let state = &mut self.channels.slots[index];

                // Combs in parallel, summed.
                let mut tail = 0.0;
                for comb in state.combs.iter_mut() {
                    let delayed = comb.line.read(comb.length as f32);
                    // One-pole low-pass in the feedback path: each time round
                    // the loop the high end loses a little more.
                    comb.filtered = delayed * (1.0 - damping) + comb.filtered * damping;
                    comb.line.push(input * 0.015 + comb.filtered * feedback);
                    tail += delayed;
                }

                // All-passes in series.
                for allpass in state.allpasses.iter_mut() {
                    let delayed = allpass.line.read(allpass.length as f32);
                    allpass.line.push(tail + delayed * 0.5);
                    tail = delayed - tail;
                }

                *sample = dry * (1.0 - params.mix) + tail * params.mix;
            }

            // Width, once the tails exist to be spread.
            if channel_count == 2 && params.width < 1.0 {
                let mid = (frame[0] + frame[1]) * 0.5;
                let side = (frame[0] - frame[1]) * 0.5 * params.width;
                frame[0] = mid + side;
                frame[1] = mid - side;
            }
        }
    }

    fn reset(&mut self) {
        self.channels.clear();
        self.pre_delay.clear();
    }
}

// -----------------------------------------------------------------------------
//  Constructors
// -----------------------------------------------------------------------------

/// Wraps an effect up as a [`Plugin`], ready to attach.
fn plugin(name: &str, effect: impl Effect) -> Plugin {
    Plugin::from_internal(InternalPlugin::from_effect(name, effect))
}

macro_rules! constructors {
    ($( $(#[$meta:meta])* $function:ident => $kind:literal, $effect:ty; )*) => {
        $(
            $(#[$meta])*
            ///
            /// Built with its default parameters; set them with
            /// [`Plugin::set_params`] or [`Plugin::set_params_str`].
            pub fn $function(sample_rate: u32) -> Plugin {
                plugin($kind, <$effect>::new(sample_rate))
            }
        )*

        /// Every built-in's name, in the order this module documents them.
        pub fn kinds() -> &'static [&'static str] {
            &[$($kind,)*]
        }

        /// Builds one by name, with parameters.
        ///
        /// The route for a chain described by configuration rather than by
        /// code. Names are the ones [`kinds`] lists.
        ///
        /// # Errors
        ///
        /// [`ParamError::Unknown`] naming the kind if there is no such effect,
        /// or whatever [`Plugin::set_params`] rejects in `params`.
        pub fn create(kind: &str, sample_rate: u32, params: &Params)
            -> Result<Plugin, ParamError>
        {
            let mut plugin = match kind {
                $($kind => $function(sample_rate),)*
                _ => return Err(ParamError::Unknown {
                    key: kind.to_string(),
                    known: kinds().iter().map(|kind| kind.to_string()).collect(),
                }),
            };

            plugin.set_params(params)?;
            Ok(plugin)
        }

        /// What parameters a kind takes, without building one.
        pub fn schema(kind: &str) -> Option<Vec<ParamSpec>> {
            match kind {
                $($kind => Some(<$effect>::new(48_000).schema()),)*
                _ => None,
            }
        }
    };
}

constructors! {
    /// Level, in dB.
    gain => "gain", GainEffect;
    /// Position across the stereo field. Two channels only.
    pan => "pan", PanEffect;
    /// Mid/side stereo width. Two channels only.
    width => "width", WidthEffect;
    /// A resonant low-pass or high-pass.
    filter => "filter", FilterEffect;
    /// Three bands: low shelf, peaking mid, high shelf.
    eq => "eq", EqEffect;
    /// Downward compression with a soft knee.
    compressor => "compressor", CompressorEffect;
    /// A compressor with the ratio pinned and the attack immediate.
    limiter => "limiter", LimiterEffect;
    /// Cuts what is below a threshold, with a hold.
    gate => "gate", GateEffect;
    /// `tanh` saturation.
    saturation => "saturation", SaturationEffect;
    /// Amplitude modulation.
    tremolo => "tremolo", TremoloEffect;
    /// A delay line with feedback and a damped repeat.
    delay => "delay", DelayEffect;
    /// A short delay swept by an LFO and mixed back in.
    chorus => "chorus", ChorusEffect;
    /// A chorus with a shorter delay and feedback.
    flanger => "flanger", FlangerEffect;
    /// Eight comb filters into four all-passes, per channel.
    reverb => "reverb", ReverbEffect;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a sine, interleaved to `channels`.
    fn tone(hz: f32, sample_rate: u32, frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|frame| {
                let value = (TAU * hz * frame as f32 / sample_rate as f32).sin();
                (0..channels).map(move |_| value)
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0_f32, |max, s| max.max(s.abs()))
    }

    // -------------------------------------------------------------------------
    //  Every effect, as a group
    // -------------------------------------------------------------------------

    #[test]
    fn every_kind_builds_and_has_a_schema() {
        for kind in kinds() {
            let specs = schema(kind).unwrap_or_else(|| panic!("{kind} has no schema"));
            assert!(!specs.is_empty(), "{kind} declares no parameters");

            let plugin = create(kind, 48_000, &Params::new())
                .unwrap_or_else(|e| panic!("{kind} would not build: {e}"));
            assert!(plugin.is_loaded(), "{kind} came back unloaded");
        }
    }

    #[test]
    fn every_default_is_inside_its_own_range() {
        for kind in kinds() {
            for spec in schema(kind).expect("a schema") {
                assert!(
                    spec.default >= spec.min && spec.default <= spec.max,
                    "{kind}.{} defaults to {}, outside {}..={}",
                    spec.name,
                    spec.default,
                    spec.min,
                    spec.max
                );
            }
        }
    }

    #[test]
    fn every_parameter_reads_back_what_was_written() {
        for kind in kinds() {
            for spec in schema(kind).expect("a schema") {
                // Something inside the range that is not the default, so
                // reading the default back would not pass by accident. A
                // toggle has only the two values, so it gets the other one.
                let wanted = match spec.kind {
                    crate::plugins::ParamKind::Toggle => {
                        if spec.default == 0.0 { 1.0 } else { 0.0 }
                    }
                    crate::plugins::ParamKind::Number => (spec.min + spec.max) / 2.0,
                };

                let params = Params::new().with(spec.name.clone(), wanted);
                let plugin = create(kind, 48_000, &params)
                    .unwrap_or_else(|e| panic!("{kind}.{}: {e}", spec.name));

                let got = plugin
                    .params()
                    .number(&spec.name)
                    .unwrap_or_else(|| panic!("{kind}.{} did not read back", spec.name));

                // Stored as f32, read back as f64.
                assert!(
                    (got - wanted).abs() < 1e-4,
                    "{kind}.{}: wrote {wanted}, read {got}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn every_effect_refuses_a_name_it_does_not_have() {
        for kind in kinds() {
            let params = Params::new().with("definitely_not_a_parameter", 1.0);
            let error = create(kind, 48_000, &params).expect_err("{kind} accepted rubbish");
            assert!(matches!(error, ParamError::Unknown { .. }), "{kind}: {error}");
        }
    }

    #[test]
    fn every_effect_refuses_a_value_out_of_range() {
        for kind in kinds() {
            for spec in schema(kind).expect("a schema") {
                if spec.kind != crate::plugins::ParamKind::Number {
                    continue;
                }

                let params = Params::new().with(spec.name.clone(), spec.max + 1.0);
                let error = create(kind, 48_000, &params)
                    .expect_err("should have refused an out-of-range value");
                assert!(
                    matches!(error, ParamError::Range { .. }),
                    "{kind}.{}: {error}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn every_effect_leaves_silence_silent() {
        // Nothing here generates: with no input there should be no output.
        // Catches a delay line reading uninitialised memory, or a filter with
        // a denormal wandering in.
        for kind in kinds() {
            let mut plugin = create(kind, 48_000, &Params::new()).expect("builds");

            let mut buffer = vec![0.0_f32; 2048];
            for block in buffer.chunks_mut(256) {
                plugin.apply(block, 2).expect("processes");
            }

            assert!(
                peak(&buffer) < 1e-6,
                "{kind} produced {} from silence",
                peak(&buffer)
            );
        }
    }

    #[test]
    fn every_effect_stays_finite_and_bounded() {
        // Every parameter at its maximum, driven hard. Feedback paths are the
        // risk: a delay or reverb whose loop gain reaches 1 grows without
        // bound, and the first sign of it is a NaN reaching the device.
        for kind in kinds() {
            let mut params = Params::new();
            for spec in schema(kind).expect("a schema") {
                params.set(spec.name.clone(), spec.max);
            }

            let mut plugin = create(kind, 48_000, &params)
                .unwrap_or_else(|e| panic!("{kind} at maximum: {e}"));

            // Ten seconds, so a slow-growing loop has time to show itself.
            let mut buffer = tone(220.0, 48_000, 480_000, 2);
            for block in buffer.chunks_mut(512) {
                plugin.apply(block, 2).expect("processes");
            }

            assert!(
                buffer.iter().all(|s| s.is_finite()),
                "{kind} produced a non-finite sample"
            );
            assert!(
                peak(&buffer) < 100.0,
                "{kind} reached {}, which is running away",
                peak(&buffer)
            );
        }
    }

    #[test]
    fn every_effect_handles_odd_channel_counts() {
        for kind in kinds() {
            for channels in [1_u16, 2, 3, 6] {
                let mut plugin = create(kind, 48_000, &Params::new()).expect("builds");
                let mut buffer = tone(440.0, 48_000, 1024, channels as usize);

                plugin
                    .apply(&mut buffer, channels)
                    .unwrap_or_else(|e| panic!("{kind} at {channels} channels: {e}"));

                assert!(
                    buffer.iter().all(|s| s.is_finite()),
                    "{kind} at {channels} channels produced a non-finite sample"
                );
            }
        }
    }

    #[test]
    fn a_clone_keeps_the_parameters_and_drops_the_state() {
        let mut original = delay(48_000);
        original
            .set_params_str("time_ms: 100, feedback: 0.9, mix: 1")
            .expect("valid");

        // Fill the delay line.
        let mut buffer = tone(440.0, 48_000, 48_000, 2);
        original.apply(&mut buffer, 2).expect("processes");

        let mut copy = original.clone();

        assert_eq!(copy.params().number("time_ms"), Some(100.0));
        let feedback = copy.params().number("feedback").expect("set above");
        assert!((feedback - 0.9).abs() < 1e-6, "got {feedback}");

        // A fresh line has nothing in it, so silence in is silence out — which
        // the original, full of echoes, would not give.
        let mut silence = vec![0.0_f32; 4096];
        copy.apply(&mut silence, 2).expect("processes");
        assert!(peak(&silence) < 1e-6, "the clone carried state over");
    }

    // -------------------------------------------------------------------------
    //  Individual effects
    // -------------------------------------------------------------------------

    #[test]
    fn gain_scales_by_the_decibels_asked_for() {
        let mut plugin = gain(48_000);
        plugin.set_params_str("gain_db: -6").expect("valid");

        let mut buffer = vec![1.0_f32; 64];
        plugin.apply(&mut buffer, 2).expect("processes");

        // -6 dB is a factor of about 0.501.
        assert!((buffer[0] - 0.5012).abs() < 1e-3, "got {}", buffer[0]);
    }

    #[test]
    fn gain_mutes() {
        let mut plugin = gain(48_000);
        plugin.set_params_str("mute: true").expect("valid");

        let mut buffer = vec![1.0_f32; 64];
        plugin.apply(&mut buffer, 2).expect("processes");

        assert_eq!(peak(&buffer), 0.0);
    }

    #[test]
    fn pan_holds_power_constant_across_the_sweep() {
        for (position, expected_left) in [(-1.0, 1.0), (0.0, 0.707), (1.0, 0.0)] {
            let mut plugin = pan(48_000);
            plugin
                .set_params(&Params::new().with("pan", position))
                .expect("valid");

            let mut buffer = vec![1.0_f32; 2];
            plugin.apply(&mut buffer, 2).expect("processes");

            assert!(
                (buffer[0] - expected_left).abs() < 1e-2,
                "at {position} the left channel was {}",
                buffer[0]
            );

            // The point of the constant-power law.
            let power = buffer[0] * buffer[0] + buffer[1] * buffer[1];
            assert!((power - 1.0).abs() < 1e-3, "power was {power} at {position}");
        }
    }

    #[test]
    fn width_collapses_to_mono_at_zero() {
        let mut plugin = width(48_000);
        plugin.set_params_str("width: 0").expect("valid");

        let mut buffer = vec![1.0_f32, -1.0, 0.5, 0.1];
        plugin.apply(&mut buffer, 2).expect("processes");

        assert!((buffer[0] - buffer[1]).abs() < 1e-6, "channels differ");
        assert!((buffer[2] - buffer[3]).abs() < 1e-6, "channels differ");
    }

    #[test]
    fn a_low_pass_keeps_the_low_and_loses_the_high() {
        let quiet = |hz: f32| {
            let mut plugin = filter(48_000);
            plugin
                .set_params_str("cutoff_hz: 1000, resonance: 0.707")
                .expect("valid");

            let mut buffer = tone(hz, 48_000, 48_000, 1);
            for block in buffer.chunks_mut(512) {
                plugin.apply(block, 1).expect("processes");
            }
            // Skip the first tenth of a second so the filter has settled.
            rms(&buffer[4800..])
        };

        let low = quiet(100.0);
        let high = quiet(10_000.0);

        assert!(low > 0.6, "100 Hz should pass, rms was {low}");
        assert!(high < 0.05, "10 kHz should not, rms was {high}");
    }

    #[test]
    fn a_high_pass_does_the_opposite() {
        let quiet = |hz: f32| {
            let mut plugin = filter(48_000);
            plugin
                .set_params_str("cutoff_hz: 1000, high_pass: true")
                .expect("valid");

            let mut buffer = tone(hz, 48_000, 48_000, 1);
            for block in buffer.chunks_mut(512) {
                plugin.apply(block, 1).expect("processes");
            }
            rms(&buffer[4800..])
        };

        assert!(quiet(100.0) < 0.05, "100 Hz should not pass");
        assert!(quiet(10_000.0) > 0.6, "10 kHz should");
    }

    #[test]
    fn eq_lifts_and_cuts_the_band_it_is_pointed_at() {
        let at = |gain_db: f64| {
            let mut plugin = eq(48_000);
            plugin
                .set_params(
                    &Params::new()
                        .with("mid_freq_hz", 1000.0)
                        .with("mid_gain_db", gain_db)
                        .with("mid_q", 2.0),
                )
                .expect("valid");

            let mut buffer = tone(1000.0, 48_000, 48_000, 1);
            for block in buffer.chunks_mut(512) {
                plugin.apply(block, 1).expect("processes");
            }
            rms(&buffer[4800..])
        };

        let flat = at(0.0);
        let boosted = at(12.0);
        let cut = at(-12.0);

        assert!((flat - 0.707).abs() < 0.02, "flat should pass, got {flat}");
        assert!(boosted > flat * 3.0, "+12 dB gave {boosted} against {flat}");
        assert!(cut < flat / 3.0, "-12 dB gave {cut} against {flat}");
    }

    #[test]
    fn the_compressor_reduces_what_is_over_the_threshold() {
        let mut plugin = compressor(48_000);
        plugin
            .set_params_str(
                "threshold_db: -20, ratio: 8, attack_ms: 1, release_ms: 50, knee_db: 0",
            )
            .expect("valid");

        // -6 dB, well over a -20 dB threshold.
        let loud = from_db(-6.0);
        let mut buffer: Vec<f32> = tone(440.0, 48_000, 48_000, 1)
            .iter()
            .map(|s| s * loud)
            .collect();

        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        // Settled, well past the attack.
        let out = to_db(peak(&buffer[24_000..]));

        // 14 dB over, at 8:1, comes out about 1.75 dB over: around -18 dB.
        assert!(
            (out - -18.25).abs() < 2.0,
            "expected about -18 dB out, got {out}"
        );
    }

    #[test]
    fn the_compressor_leaves_quiet_signal_alone() {
        let mut plugin = compressor(48_000);
        plugin
            .set_params_str("threshold_db: -20, ratio: 8, knee_db: 0, makeup_db: 0")
            .expect("valid");

        let quiet = from_db(-40.0);
        let mut buffer: Vec<f32> = tone(440.0, 48_000, 48_000, 1)
            .iter()
            .map(|s| s * quiet)
            .collect();

        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        let out = to_db(peak(&buffer[24_000..]));
        assert!((out - -40.0).abs() < 0.5, "expected -40 dB out, got {out}");
    }

    #[test]
    fn the_limiter_holds_the_ceiling() {
        let mut plugin = limiter(48_000);
        plugin
            .set_params_str("ceiling_db: -3, release_ms: 20")
            .expect("valid");

        let mut buffer: Vec<f32> = tone(440.0, 48_000, 48_000, 1).iter().map(|s| s * 4.0).collect();
        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        let out = to_db(peak(&buffer[2400..]));
        assert!(out <= -3.0 + 0.5, "expected under -3 dB, got {out}");
    }

    #[test]
    fn the_gate_cuts_what_is_under_the_threshold() {
        let mut plugin = gate(48_000);
        plugin
            .set_params_str("threshold_db: -30, attack_ms: 1, hold_ms: 5, release_ms: 10")
            .expect("valid");

        let quiet = from_db(-60.0);
        let mut buffer: Vec<f32> = tone(440.0, 48_000, 48_000, 1)
            .iter()
            .map(|s| s * quiet)
            .collect();
        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        let out = to_db(peak(&buffer[24_000..]));
        assert!(out < -100.0, "a closed gate should be quiet, got {out}");
    }

    #[test]
    fn the_gate_passes_what_is_over_it() {
        let mut plugin = gate(48_000);
        plugin
            .set_params_str("threshold_db: -30, attack_ms: 1, hold_ms: 5")
            .expect("valid");

        let mut buffer = tone(440.0, 48_000, 48_000, 1);
        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        let out = to_db(peak(&buffer[24_000..]));
        assert!(out > -1.0, "an open gate should pass, got {out}");
    }

    #[test]
    fn saturation_cannot_exceed_unity() {
        let mut plugin = saturation(48_000);
        plugin.set_params_str("drive: 50, mix: 1").expect("valid");

        let mut buffer: Vec<f32> = tone(440.0, 48_000, 4800, 1).iter().map(|s| s * 20.0).collect();
        plugin.apply(&mut buffer, 1).expect("processes");

        assert!(peak(&buffer) <= 1.0001, "reached {}", peak(&buffer));
    }

    #[test]
    fn tremolo_modulates_over_its_cycle() {
        let mut plugin = tremolo(48_000);
        plugin.set_params_str("rate_hz: 1, depth: 1").expect("valid");

        // A constant, so anything left is the LFO alone.
        let mut buffer = vec![1.0_f32; 48_000];
        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        // One full cycle at 1 Hz over one second: it should touch both ends.
        assert!(peak(&buffer) > 0.99, "never reached the top");
        assert!(
            buffer.iter().any(|s| s.abs() < 0.01),
            "never reached the bottom"
        );
    }

    #[test]
    fn a_delay_repeats_after_the_time_asked_for() {
        let mut plugin = delay(48_000);
        plugin
            .set_params_str("time_ms: 100, feedback: 0, mix: 1, damping_hz: 20000")
            .expect("valid");

        // One loud frame, then silence.
        let mut buffer = vec![0.0_f32; 48_000];
        buffer[0] = 1.0;

        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        // 100 ms at 48 kHz is frame 4800. The line is interpolated and the
        // damping filter smears it a little, so look at a window.
        let echo = peak(&buffer[4790..4815]);
        assert!(echo > 0.5, "no echo at 100 ms, peak was {echo}");

        let before = peak(&buffer[100..4700]);
        assert!(before < 0.01, "something arrived early: {before}");
    }

    #[test]
    fn delay_feedback_decays() {
        let mut plugin = delay(48_000);
        plugin
            .set_params_str("time_ms: 50, feedback: 0.5, mix: 1, damping_hz: 20000")
            .expect("valid");

        let mut buffer = vec![0.0_f32; 48_000];
        buffer[0] = 1.0;
        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        // Each repeat is 50 ms (2400 frames) later and quieter than the last.
        let first = peak(&buffer[2390..2415]);
        let second = peak(&buffer[4790..4815]);
        let third = peak(&buffer[7190..7215]);

        assert!(first > second && second > third, "{first} {second} {third}");
        assert!(second > 0.05, "the second repeat vanished: {second}");
    }

    #[test]
    fn chorus_and_flanger_change_the_signal_without_destroying_it() {
        for (name, mut plugin) in [
            ("chorus", chorus(48_000)),
            ("flanger", flanger(48_000)),
        ] {
            let dry = tone(440.0, 48_000, 48_000, 2);
            let mut wet = dry.clone();

            for block in wet.chunks_mut(512) {
                plugin.apply(block, 2).expect("processes");
            }

            // Settled, past the initial silence in the line.
            let changed = wet[24_000..]
                .iter()
                .zip(&dry[24_000..])
                .any(|(a, b)| (a - b).abs() > 0.01);
            assert!(changed, "{name} left the signal alone");

            let level = rms(&wet[24_000..]);
            assert!(
                level > 0.1 && level < 2.0,
                "{name} came out at {level}, which is not a mix of the input"
            );
        }
    }

    #[test]
    fn reverb_rings_on_after_the_input_stops() {
        let mut plugin = reverb(48_000);
        plugin
            .set_params_str("room_size: 0.9, damping: 0.2, mix: 1")
            .expect("valid");

        // A tenth of a second of noise-ish signal, then silence.
        let mut buffer = vec![0.0_f32; 96_000];
        for (index, sample) in buffer[..4800].iter_mut().enumerate() {
            *sample = (index as f32 * 0.37).sin() * 0.5;
        }

        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        // Half a second after the input stopped there should still be a tail.
        let tail = rms(&buffer[28_800..38_400]);
        assert!(tail > 1e-4, "no tail, rms was {tail}");

        // And it should be decaying, not sustaining.
        let later = rms(&buffer[76_800..86_400]);
        assert!(later < tail, "tail grew: {tail} then {later}");
    }

    #[test]
    fn reverb_pre_delay_holds_the_tail_back() {
        let mut plugin = reverb(48_000);
        plugin
            .set_params_str("room_size: 0.8, mix: 1, pre_delay_ms: 100")
            .expect("valid");

        let mut buffer = vec![0.0_f32; 48_000];
        buffer[0] = 1.0;
        for block in buffer.chunks_mut(512) {
            plugin.apply(block, 1).expect("processes");
        }

        // Nothing much before the pre-delay is up.
        let early = peak(&buffer[100..4000]);
        let after = peak(&buffer[4800..20_000]);
        assert!(early < after, "early {early} was not quieter than {after}");
    }

    // -------------------------------------------------------------------------
    //  Building by name
    // -------------------------------------------------------------------------

    #[test]
    fn create_builds_from_a_name_and_a_parameter_string() {
        let params = Params::parse("time_ms: 375, feedback: 0.4, mix: 0.3").expect("valid");
        let plugin = create("delay", 48_000, &params).expect("builds");

        assert_eq!(plugin.params().number("time_ms"), Some(375.0));
        assert_eq!(plugin.name, "delay");
    }

    #[test]
    fn create_refuses_a_kind_it_does_not_have() {
        let error = create("phaser", 48_000, &Params::new()).expect_err("no phaser yet");

        let message = error.to_string();
        assert!(message.contains("phaser"), "{message}");
        // The list of what there is, so the fix is visible.
        assert!(message.contains("chorus"), "{message}");
    }

    #[test]
    fn parameters_can_be_changed_after_the_plugin_is_built() {
        let mut plugin = compressor(48_000);
        assert_eq!(plugin.params().number("ratio"), Some(4.0));

        plugin.set_param("ratio", 10.0).expect("valid");
        assert_eq!(plugin.params().number("ratio"), Some(10.0));

        // And the rest are left where they were.
        assert_eq!(plugin.params().number("threshold_db"), Some(-18.0));
    }

    #[test]
    fn a_failed_set_changes_nothing() {
        let mut plugin = compressor(48_000);
        let before = plugin.params();

        // One good name and one bad one. Whichever order they are applied in,
        // neither should land.
        for params in [
            Params::new().with("ratio", 10.0).with("nonsense", 1.0),
            Params::new().with("aaa_nonsense", 1.0).with("ratio", 10.0),
        ] {
            assert!(plugin.set_params(&params).is_err());
            assert_eq!(plugin.params(), before, "a rejected set was partly applied");
        }

        // Same for a value that is merely out of range.
        let params = Params::new().with("ratio", 10.0).with("threshold_db", 500.0);
        assert!(plugin.set_params(&params).is_err());
        assert_eq!(plugin.params(), before);
    }
}
