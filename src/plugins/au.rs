//! Audio Unit v2 plugin hosting. Enabled by the `au` feature.
//!
//! macOS and iOS only — Audio Units are an Apple framework, so both the
//! backing crate and this module are declared for Apple targets alone. Turning
//! the feature on elsewhere pulls in nothing and leaves
//! [`PluginFormat::Au`](super::PluginFormat::Au) a format that
//! [`Plugin::load`](super::Plugin::load) refuses, with a message saying why.
//!
//! Backed by [`truce-rack-au`](https://docs.rs/truce-rack-au/1.1.5). Everything
//! atome adds on top — activation, deinterleaving, block splitting — is shared
//! with the other hosted formats and lives in [`host`](super::host).
//!
//! A v2 Audio Unit is a `.component` bundle under
//! `/Library/Audio/Plug-Ins/Components`, or the equivalent in `~/Library`.
//! For v3 App Extensions, see [`au3`](super::au3).

use std::path::Path;

use truce_rack_au::{AuPlugin, AuScanner};
use truce_rack_core::{
    error::{Error as CoreError, Result as CoreResult},
    scanner::PluginScanner,
};

use super::host::Hosted;

/// The loaded form of an Audio Unit, with the scratch its blocks need.
///
/// Shared with [`au3`](super::au3): a v3 App Extension is driven through the
/// same `AudioComponentInstance`, so it loads into this same type.
pub(crate) type Loaded = Hosted<AuPlugin>;

/// Scans `path` and loads the first Audio Unit it finds there.
///
/// # Errors
///
/// Fails if the scan finds no AU at `path`, if the component will not
/// instantiate, or if the plugin declares no `channels`-in/`channels`-out
/// layout.
pub(crate) fn load(
    path: &Path,
    channels: usize,
    sample_rate: f64,
    max_block: usize,
) -> CoreResult<Loaded> {
    let scanner = AuScanner::new();

    let info = scanner
        .scan_path(path)?
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::PluginNotFound(path.display().to_string()))?;

    Hosted::new(scanner.load(&info)?, channels, sample_rate, max_block)
}
