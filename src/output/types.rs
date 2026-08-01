//! Shared value and sample types describing an output.

use cpal::{FromSample, Sample, SampleFormat, SizedSample};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputType {
    // Windows
    ASIO,
    WASAPI,
    DirectSound,
    WDMKS,
    MME,
    // macOS
    CoreAudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleRate {
    Hz8k = 8000,
    Hz11_025k = 11025,
    Hz16k = 16000,
    Hz44_1k = 44100,
    Hz48k = 48000,
    Hz88_2k = 88200,
    Hz96k = 96000,
    Hz176_4k = 176400,
    Hz192k = 192000,
    Hz352_8 = 352800,
}

/// # SampleType
///
/// Marker trait for the *real* Rust types a sample can be stored in, so a
/// function can take `samples: &[S] where S: SampleType` and be called with a
/// plain `&[f32]`, `&[i16]`, `&[u8]`, ... with no conversion at the call site.
///
/// It only ever bounds a type parameter — `S: SampleType` reads as "S is a
/// sample type", so `OutputClass<f32>` or `data_callback::<i16>` names the
/// concrete type directly and the compiler picks the conversions for it.
///
/// Implemented (via the blanket impl below) for every type cpal can hand to a
/// stream callback:
///
/// `i8, i16, I24, i32, i64, u8, u16, U24, u32, u64, f32, f64`
///
/// `cpal::I24` / `cpal::U24` are `i32` / `u32` under the hood, just clamped to
/// a 24-bit range — they exist as separate newtypes only so the conversion
/// scales by 2^23 instead of 2^31. If you already have plain `i32` data, pass
/// `&[i32]`; use `I24` only when the values really are 24-bit.
///
/// DSD (`DsdU8`/`DsdU16`/`DsdU32`) has no type of its own: it is a 1-bit
/// bitstream packed into `u8`/`u16`/`u32`, so it stays a [`SampleFormat`]-level
/// concept and is not part of this trait.
pub trait SampleType: SizedSample + Send + 'static {
    /// The value that means silence for this type: `0` for the signed and float
    /// formats, the midpoint (`128`, `32768`, …) for the unsigned ones.
    const SILENCE: Self;

    /// The [`SampleFormat`] this type is laid out as on a device.
    fn format() -> SampleFormat;

    /// Sums two samples **in their own format** — no float round-trip, so an
    /// `i16` pipeline stays `i16` from `add_samples` all the way to the device.
    ///
    /// Silence is the identity: mixing with [`SILENCE`](Self::SILENCE) is a
    /// no-op, including for the unsigned formats where silence isn't zero.
    ///
    /// Summing has the range of the format itself, so loud sources can overflow
    /// it (wrapping in release, panicking in debug) exactly as an integer `+`
    /// would. Leave headroom, or attenuate before mixing.
    fn mix(self, other: Self) -> Self;

    /// Convert to `f32` in `-1.0..=1.0`. Not used by the pipeline — only for
    /// callers that want to inspect or process samples in float.
    fn to_f32(self) -> f32;

    /// Convert back from an `f32` in `-1.0..=1.0`.
    fn from_f32(sample: f32) -> Self;
}

impl<T> SampleType for T
where
    T: SizedSample + Send + 'static + FromSample<f32>,
    f32: FromSample<T>,
{
    const SILENCE: Self = <T as Sample>::EQUILIBRIUM;

    fn format() -> SampleFormat {
        <T as SizedSample>::FORMAT
    }

    fn mix(self, other: Self) -> Self {
        // `add_amp` adds an offset *from* equilibrium, which is what makes this
        // correct for the unsigned formats too.
        self.add_amp(other.to_signed_sample())
    }

    fn to_f32(self) -> f32 {
        self.to_sample::<f32>()
    }

    fn from_f32(sample: f32) -> Self {
        sample.to_sample::<T>()
    }
}
