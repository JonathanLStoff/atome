//! VST 3 plugin hosting. Enabled by the `vst3` feature.
//!
//! Backed by [`truce-rack-vst3`](https://docs.rs/truce-rack-vst3/1.1.5), which
//! provides the scanner and the loaded instance. Everything atome adds on top
//! — activation, deinterleaving, block splitting — is shared with the other
//! hosted formats and lives in [`host`](super::host).
//!
//! A VST3 bundle is a directory ending in `.vst3`, so
//! [`Plugin::load`](super::Plugin::load) recognises one by that suffix.
//!
//! ```no_run
//! use atome::plugins::{Plugin, PluginFormat};
//!
//! let mut plugin = Plugin::new(
//!     "Pro-Q".into(),
//!     "/Library/Audio/Plug-Ins/VST3/FabFilter Pro-Q 3.vst3".into(),
//!     512,
//!     48_000,
//!     2,
//!     String::new(),
//!     PluginFormat::Vst3,
//! );
//! plugin.load()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::path::Path;

use truce_rack_core::{
    error::{Error as CoreError, Result as CoreResult},
    scanner::PluginScanner,
};
use truce_rack_vst3::{Vst3Plugin, Vst3Scanner};

use super::host::Hosted;

/// The loaded form of a VST3 plugin, with the scratch its blocks need.
pub(crate) type Loaded = Hosted<Vst3Plugin>;

/// Scans `path` and loads the first VST3 it finds there.
///
/// "First" rather than "the one you meant": a single `.vst3` bundle can
/// declare several plugins, and the descriptor's name is a label for atome's
/// own bookkeeping rather than something guaranteed to match what the bundle
/// calls its plugins. Point at a bundle that holds one, or scan yourself with
/// [`Vst3Scanner`] and pick.
///
/// # Errors
///
/// Fails if the scan finds no VST3 at `path`, if the module will not load, or
/// if the plugin declares no `channels`-in/`channels`-out layout.
pub(crate) fn load(
    path: &Path,
    channels: usize,
    sample_rate: f64,
    max_block: usize,
) -> CoreResult<Loaded> {
    let scanner = Vst3Scanner::new();

    let info = scanner
        .scan_path(path)?
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::PluginNotFound(path.display().to_string()))?;

    Hosted::new(scanner.load(&info)?, channels, sample_rate, max_block)
}
