//! Plugin hosting, one module per format.
//!
//! Every format is behind a Cargo feature, so a build only carries the ones it
//! asks for:
//!
//! | Feature | Module   | Availability     |
//! |---------|----------|------------------|
//! | `vst`   | [`vst`]  | all platforms    |
//! | `vst3`  | [`vst3`] | all platforms    |
//! | `au`    | [`au`]   | macOS / iOS only |
//!
//! `plugins` turns on all three at once.
//!
//! ```toml
//! atome = { version = "0.1", features = ["vst3", "au"] }
//! ```
//!
//! [`internal`] holds the effects built into the engine and is always compiled.
use cpal::Error;
use std::path::PathBuf;

use crate::output::SampleType;

pub mod internal;

#[cfg(feature = "au")]
pub mod au;
#[cfg(feature = "vst")]
pub mod vst;
#[cfg(feature = "vst3")]
pub mod vst3;

/// A plugin, and the configuration it was prepared with.
///
/// Where one is attached decides what it hears — see
/// [`AtomeDevice`](crate::device::AtomeDevice) and
/// [`AudioEngine`](crate::AudioEngine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
    pub buffer_size: usize,
    pub sample_rate: usize,
    pub channels: usize,
    pub params: String,
}

impl Plugin {
    /// TODO: process `buffer` in place.
    ///
    /// A stub, and will stay one until [section 11 of
    /// `planning/TODO.md`](https://docs.rs/atome) has a hosting backend — no
    /// format module under this one loads anything yet. It exists now so that
    /// the *order* of processing is settled and testable before any of it is
    /// real: per-input plugins run before routing, per-output plugins after
    /// mixing, and engine-wide plugins over everything.
    ///
    /// Silently returning `Ok(())` is deliberate. An unimplemented plugin that
    /// errors would make every engine with a plugin attached fail to run, and
    /// nothing here can go wrong in a way a caller could act on: the buffer is
    /// simply passed through unchanged.
    ///
    /// `buffer` is interleaved at `channels`.
    pub fn apply<S: SampleType>(&mut self, buffer: &mut [S], channels: u16) -> Result<(), Error> {
        let _ = (buffer, channels);

        Ok(())
    }

    /// TODO: report the latency this plugin introduces, in frames.
    ///
    /// Needed for delay compensation: a chain that delays one route and not
    /// another puts them out of phase with each other.
    pub fn latency(&self) -> usize {
        0
    }
}
