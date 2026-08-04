//! Audio Unit v3 plugin hosting. Enabled by the `au3` feature.
//!
//! macOS and iOS only, on the same terms as [`au`](super::au).
//!
//! # What v3 changes, and what it does not
//!
//! A v3 Audio Unit ships as a sandboxed App Extension discovered through
//! `NSExtension`, not as a `.component` dylib, and the host talks to it over
//! XPC. That changes discovery entirely — which is why this is a separate
//! feature with its own scanner,
//! [`truce-rack-au3`](https://docs.rs/truce-rack-au3/1.1.5).
//!
//! It changes rendering not at all. Once an `AudioComponentInstance` is in
//! hand the interface is the v2 one, so a loaded v3 plugin is the same
//! [`AuPlugin`](truce_rack_au::AuPlugin) a v2 load produces, and both go
//! through the same [`host`](super::host) machinery.
//!
//! # Scanning finds more than loading can open
//!
//! `truce-rack-au3` scans by filtering the `AudioComponentFindNext` walk on
//! `kAudioComponentFlag_IsV3AudioUnit`, then forwards loading to
//! `truce-rack-au`. Extensions flagged
//! `kAudioComponentFlag_RequiresAsyncInstantiation` need
//! `AudioComponentInstantiate` with a completion block, which that crate has
//! not implemented — so they are found by a scan and fail on load. The error
//! comes from the backing crate and is returned as-is.

use std::path::Path;

use truce_rack_au3::Au3Scanner;
use truce_rack_core::{
    error::{Error as CoreError, Result as CoreResult},
    scanner::PluginScanner,
};

use super::host::Hosted;

/// The loaded form of a v3 Audio Unit.
///
/// The same type [`au::Loaded`](super::au::Loaded) resolves to, for the reason
/// in this module's own documentation: v3 differs in how it is found, not in
/// how it renders.
pub(crate) type Loaded = Hosted<truce_rack_au::AuPlugin>;

/// Scans `path` for v3 App Extensions and loads the first one.
///
/// # Errors
///
/// Fails if the scan finds no v3 Audio Unit at `path`, if the extension
/// requires async instantiation (see this module's documentation), or if the
/// plugin declares no `channels`-in/`channels`-out layout.
pub(crate) fn load(
    path: &Path,
    channels: usize,
    sample_rate: f64,
    max_block: usize,
) -> CoreResult<Loaded> {
    let scanner = Au3Scanner::new();

    let info = scanner
        .scan_path(path)?
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::PluginNotFound(path.display().to_string()))?;

    Hosted::new(scanner.load(&info)?, channels, sample_rate, max_block)
}
