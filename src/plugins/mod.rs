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

pub mod internal;

#[cfg(feature = "au")]
pub mod au;
#[cfg(feature = "vst")]
pub mod vst;
#[cfg(feature = "vst3")]
pub mod vst3;
