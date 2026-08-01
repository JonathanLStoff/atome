//! ASIO-specific output helpers. Enabled by the `asio` feature.
//!
//! Windows only in practice: cpal only builds its ASIO backend on Windows, so
//! [`host`] returns `None` everywhere else even with the feature on.
//!
//! Building this on Windows needs the ASIO SDK and LLVM/Clang on `PATH`; see
//! cpal's README for the environment it expects.

use cpal::Host;

/// The ASIO host, or `None` if this build has no working ASIO backend.
///
/// Prefer this over `cpal::default_host()` when you specifically want ASIO —
/// the default host is WASAPI on Windows even in an ASIO-enabled build.
///
/// Looked up by name rather than through `HostId::Asio`, because cpal puts that
/// variant behind `#[cfg(windows)]` — it does not exist on other platforms even
/// with the `asio` feature on. This way the module compiles everywhere and
/// simply reports `None` where the backend isn't available.
pub fn host() -> Option<Host> {
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case("asio"))?;

    cpal::host_from_id(id).ok()
}

/// Whether this build can reach ASIO devices right now.
pub fn is_available() -> bool {
    host().is_some()
}
