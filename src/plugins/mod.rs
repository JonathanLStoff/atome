//! Plugin hosting, one module per format.
//!
//! | Format   | Module       | Feature | Availability     |
//! |----------|--------------|---------|------------------|
//! | Internal | [`internal`] | —       | always           |
//! | VST 2.4  | [`vst`]      | `vst`   | no backend yet   |
//! | VST 3    | [`vst3`]     | `vst3`  | all platforms    |
//! | AU v2    | [`au`]       | `au`    | macOS / iOS only |
//! | AU v3    | [`au3`]      | `au3`   | macOS / iOS only |
//!
//! `plugins` turns on all of them at once.
//!
//! ```toml
//! atome = { version = "0.8.0", features = ["vst3", "au3"] }
//! ```
//!
//! [Internal plugins](internal) are behind no feature because there is nothing
//! to link against: they are Rust functions compiled into the host, and the
//! shortest path to a working chain.
//!
//! ```
//! use atome::plugins::Plugin;
//!
//! let quieter = Plugin::internal("-6 dB", |buffer: &mut [f32], _channels| {
//!     for sample in buffer {
//!         *sample *= 0.5;
//!     }
//! });
//! ```
//!
//! The hosted formats all take the same route: describe the plugin with
//! [`Plugin::new`], call [`Plugin::load`] once off the audio thread, then
//! [`Plugin::apply`] per block.
//!
//! # Where a plugin runs
//!
//! Attaching is separate from loading — see
//! [`AtomeDevice`](crate::device::AtomeDevice) and
//! [`AudioEngine`](crate::AudioEngine) for the three levels a chain can hang
//! off, and what each one hears.

use std::fmt;
use std::path::PathBuf;

use cpal::{Error, ErrorKind};

use crate::output::SampleType;

pub mod internal;

#[cfg(all(feature = "au", target_vendor = "apple"))]
pub mod au;
#[cfg(all(feature = "au3", target_vendor = "apple"))]
pub mod au3;
#[cfg(feature = "vst")]
pub mod vst;
#[cfg(feature = "vst3")]
pub mod vst3;

// The shared hosting machinery — only worth compiling when there is a hosted
// format to use it.
#[cfg(any(
    feature = "vst3",
    all(any(feature = "au", feature = "au3"), target_vendor = "apple")
))]
mod host;

pub use internal::InternalPlugin;

/// Whether a hosted format is compiled into this build.
///
/// A feature can be on while the format is still unavailable — the AU features
/// resolve to nothing off Apple — so the module gates have to ask about the
/// platform too. This keeps that question phrased one way, in one place.
macro_rules! have {
    (au) => {
        cfg!(all(feature = "au", target_vendor = "apple"))
    };
    (au3) => {
        cfg!(all(feature = "au3", target_vendor = "apple"))
    };
    (vst3) => {
        cfg!(feature = "vst3")
    };
}

// -----------------------------------------------------------------------------
//  Plugin format
// -----------------------------------------------------------------------------

/// Which kind of plugin a [`Plugin`] describes.
///
/// Every variant exists in every build, whatever features are on. A format
/// this build cannot host is refused by [`Plugin::load`] with a message naming
/// the feature that would fix it — a better place to find out than a missing
/// enum variant at the call site, and it lets a configuration file name a
/// format the binary happens not to carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    /// A Rust function compiled into the host. See [`internal`].
    Internal,
    /// Apple Audio Unit v2. Needs `au`, on macOS or iOS.
    Au,
    /// Apple Audio Unit v3 App Extension. Needs `au3`, on macOS or iOS.
    Au3,
    /// Steinberg VST 2.4. No backend — see [`vst`].
    Vst,
    /// Steinberg VST 3. Needs `vst3`.
    Vst3,
}

impl PluginFormat {
    /// Whether this build can actually load this format.
    ///
    /// [`Internal`](Self::Internal) is always true; the rest depend on the
    /// features the crate was built with and the platform it was built for.
    pub fn is_available(self) -> bool {
        match self {
            Self::Internal => true,
            Self::Au => have!(au),
            Self::Au3 => have!(au3),
            Self::Vst => false,
            Self::Vst3 => have!(vst3),
        }
    }

    /// Why this build cannot load this format, for the error message.
    fn unavailable(self) -> String {
        match self {
            Self::Internal => unreachable!("internal plugins are always available"),
            Self::Au => "AU v2 hosting needs the 'au' feature, on macOS or iOS".into(),
            Self::Au3 => "AU v3 hosting needs the 'au3' feature, on macOS or iOS".into(),
            Self::Vst => "VST 2.4 hosting is not implemented — there is no backend behind the \
                          'vst' feature. Use VST 3 ('vst3') instead."
                .into(),
            Self::Vst3 => "VST 3 hosting needs the 'vst3' feature".into(),
        }
    }
}

// -----------------------------------------------------------------------------
//  Loaded instance
// -----------------------------------------------------------------------------

/// The loaded plugin itself, once [`Plugin::load`] has been through.
enum PluginInner {
    Internal(InternalPlugin),
    #[cfg(all(feature = "au", target_vendor = "apple"))]
    Au(au::Loaded),
    // v3 loads into the same `AuPlugin` v2 does — see `au3`'s documentation —
    // so it needs a variant of its own only when `au` is not also on.
    #[cfg(all(feature = "au3", target_vendor = "apple", not(feature = "au")))]
    Au3(au3::Loaded),
    #[cfg(feature = "vst3")]
    Vst3(vst3::Loaded),
}

// -----------------------------------------------------------------------------
//  Plugin
// -----------------------------------------------------------------------------

/// A plugin, and the configuration it was prepared with.
///
/// Two halves: a descriptor you can build, store, and compare freely, and —
/// after [`load`](Self::load) — the loaded instance behind it. Where one is
/// attached decides what it hears; see
/// [`AtomeDevice`](crate::device::AtomeDevice) and
/// [`AudioEngine`](crate::AudioEngine).
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
    pub buffer_size: usize,
    pub sample_rate: usize,
    pub channels: usize,
    pub params: String,
    pub format: PluginFormat,
    /// `None` until [`Plugin::load`].
    inner: Option<PluginInner>,
}

impl Plugin {
    /// Describes a plugin without loading it.
    ///
    /// Nothing is opened, scanned, or instantiated here — call
    /// [`load`](Self::load) for that. `buffer_size`, `sample_rate`, and
    /// `channels` are what the plugin will be activated for, so they should
    /// match the stream it is going to sit in.
    pub fn new(
        name: String,
        path: PathBuf,
        buffer_size: usize,
        sample_rate: usize,
        channels: usize,
        params: String,
        format: PluginFormat,
    ) -> Self {
        Self {
            name,
            path,
            buffer_size,
            sample_rate,
            channels,
            params,
            format,
            inner: None,
        }
    }

    /// An [internal plugin](internal), already loaded.
    ///
    /// The direct route: there is nothing to scan for, so the function is the
    /// plugin and this is the whole of loading it.
    ///
    /// ```
    /// use atome::plugins::Plugin;
    ///
    /// let mut invert = Plugin::internal("invert", |buffer: &mut [f32], _channels| {
    ///     for sample in buffer {
    ///         *sample = -*sample;
    ///     }
    /// });
    ///
    /// let mut block = [0.5_f32, -0.25];
    /// invert.apply(&mut block, 2).unwrap();
    /// assert_eq!(block, [-0.5, 0.25]);
    /// ```
    pub fn internal<F>(name: impl Into<String>, process: F) -> Self
    where
        F: Fn(&mut [f32], u16) + Send + Sync + 'static,
    {
        Self::from_internal(InternalPlugin::new(name, process))
    }

    /// An [internal plugin](internal) built elsewhere — one that declares a
    /// latency through [`InternalPlugin::with_latency`], or one pulled out of
    /// the registry with [`internal::get`].
    pub fn from_internal(plugin: InternalPlugin) -> Self {
        Self {
            name: plugin.name().to_string(),
            path: PathBuf::new(),
            buffer_size: 0,
            sample_rate: 0,
            channels: 0,
            params: String::new(),
            format: PluginFormat::Internal,
            inner: Some(PluginInner::Internal(plugin)),
        }
    }

    /// Whether [`load`](Self::load) has been through successfully.
    ///
    /// An unloaded plugin is not an error to [`apply`](Self::apply) — it
    /// passes audio through untouched — so this is how you check that a chain
    /// is doing what you think it is.
    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    /// Loads the plugin.
    ///
    /// For an internal plugin this resolves `name` against [`internal`]'s
    /// registry. For the hosted formats it scans `path`, instantiates what it
    /// finds, and activates it for this descriptor's `channels`,
    /// `sample_rate`, and `buffer_size`.
    ///
    /// Does nothing if the plugin is already loaded.
    ///
    /// **Not for the audio thread.** Scanning walks the filesystem, loading
    /// opens dylibs, and activation allocates — seconds, in the worst case.
    ///
    /// # Errors
    ///
    /// - The format is not one this build can host — see
    ///   [`PluginFormat::is_available`]
    /// - [`PluginFormat::Internal`] and nothing is registered under `name`
    /// - No plugin of that format at `path`, or it will not load
    /// - The plugin has no bus layout carrying `channels` channels in and out
    pub fn load(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.inner.is_some() {
            return Ok(());
        }

        let format = self.detect_format();

        if !format.is_available() {
            return Err(format.unavailable().into());
        }

        self.inner = Some(match format {
            PluginFormat::Internal => {
                let plugin = internal::get(&self.name).ok_or_else(|| {
                    format!(
                        "no internal plugin registered as '{}' — register one with \
                         atome::plugins::internal::register, or build it directly with \
                         Plugin::internal",
                        self.name
                    )
                })?;
                PluginInner::Internal(plugin)
            }

            #[cfg(all(feature = "au", target_vendor = "apple"))]
            PluginFormat::Au => PluginInner::Au(au::load(
                &self.path,
                self.channels,
                self.sample_rate as f64,
                self.buffer_size,
            )?),

            // With `au` on, a v3 load produces the same `AuPlugin` and so goes
            // into the same variant.
            #[cfg(all(feature = "au3", target_vendor = "apple", feature = "au"))]
            PluginFormat::Au3 => PluginInner::Au(au3::load(
                &self.path,
                self.channels,
                self.sample_rate as f64,
                self.buffer_size,
            )?),
            #[cfg(all(feature = "au3", target_vendor = "apple", not(feature = "au")))]
            PluginFormat::Au3 => PluginInner::Au3(au3::load(
                &self.path,
                self.channels,
                self.sample_rate as f64,
                self.buffer_size,
            )?),

            #[cfg(feature = "vst3")]
            PluginFormat::Vst3 => PluginInner::Vst3(vst3::load(
                &self.path,
                self.channels,
                self.sample_rate as f64,
                self.buffer_size,
            )?),

            // Everything `is_available` lets through is handled above. This
            // arm catches the formats it does not, which the check has already
            // returned for — so reaching it would be a bug in that pairing.
            #[allow(unreachable_patterns)]
            other => return Err(other.unavailable().into()),
        });

        Ok(())
    }

    /// Works out what format to load, from the descriptor or from the path.
    ///
    /// A format stated explicitly is taken at its word. [`Internal`] is the
    /// one that gets second-guessed, since it is also the variant a caller
    /// falls into without meaning to: when the descriptor says `Internal` but
    /// carries a path that looks like a plugin bundle, the path wins.
    ///
    /// [`Internal`]: PluginFormat::Internal
    fn detect_format(&self) -> PluginFormat {
        if self.format != PluginFormat::Internal {
            return self.format;
        }

        // A bundle is a directory whose *name* ends in the suffix, so match on
        // the name rather than on `extension()` — which answers the same for a
        // file and for a directory, and is not what tells them apart.
        let name = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if name.ends_with(".vst3") {
            PluginFormat::Vst3
        } else if name.ends_with(".component") {
            PluginFormat::Au
        } else if name.ends_with(".appex") {
            PluginFormat::Au3
        } else {
            // Nothing bundle-shaped to go on, so the descriptor meant what it
            // said: an internal plugin, to be looked up by name.
            PluginFormat::Internal
        }
    }

    // -------------------------------------------------------------------------
    //  Audio processing
    // -------------------------------------------------------------------------

    /// Runs one block through the plugin, in place.
    ///
    /// `buffer` is interleaved by `channels`. An unloaded plugin passes the
    /// audio through untouched rather than failing — a chain holding a plugin
    /// that never loaded should be quiet about it in the callback, not stop
    /// the stream.
    ///
    /// # Errors
    ///
    /// - `buffer` is not a whole number of `channels`-wide frames
    /// - `channels` is not what the plugin was loaded and activated for
    /// - The plugin itself failed on the block
    pub fn apply<S: SampleType>(&mut self, buffer: &mut [S], channels: u16) -> Result<(), Error> {
        let Some(inner) = &mut self.inner else {
            return Ok(());
        };

        match inner {
            PluginInner::Internal(plugin) => apply_internal(plugin, buffer, channels),

            #[cfg(all(feature = "au", target_vendor = "apple"))]
            PluginInner::Au(plugin) => plugin.process(buffer, channels).map_err(host_error),
            #[cfg(all(feature = "au3", target_vendor = "apple", not(feature = "au")))]
            PluginInner::Au3(plugin) => plugin.process(buffer, channels).map_err(host_error),
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => plugin.process(buffer, channels).map_err(host_error),
        }
    }

    /// The delay this plugin introduces, in frames.
    ///
    /// Needed for delay compensation: a chain that delays one route and not
    /// another puts them out of phase with each other.
    ///
    /// Internal plugins report what they declared through
    /// [`InternalPlugin::with_latency`]. The hosted formats report zero —
    /// `truce-rack-core` exposes no latency on its plugin traits, so there is
    /// nothing to ask. Read a zero from a VST3 or an AU as "not implemented",
    /// not as "no delay".
    pub fn latency(&self) -> usize {
        match &self.inner {
            Some(PluginInner::Internal(plugin)) => plugin.latency(),
            _ => 0,
        }
    }
}

/// Runs an internal plugin, converting to `f32` and back when the engine is
/// not already running in it.
///
/// The scratch buffer is the cost of the type erasure that lets one boxed
/// function serve every sample type — see [`internal::ProcessFn`]. It is
/// allocated per block, which is an allocation on the audio thread and the
/// one thing here that is not yet good enough for a small buffer.
fn apply_internal<S: SampleType>(
    plugin: &InternalPlugin,
    buffer: &mut [S],
    channels: u16,
) -> Result<(), Error> {
    if channels == 0 || buffer.len() % channels as usize != 0 {
        return Err(Error::with_message(
            ErrorKind::InvalidInput,
            format!(
                "{} samples is not a whole number of {channels}-channel frames",
                buffer.len()
            ),
        ));
    }

    let mut scratch: Vec<f32> = buffer.iter().map(|sample| sample.to_f32()).collect();
    plugin.process(&mut scratch, channels);

    for (sample, processed) in buffer.iter_mut().zip(scratch) {
        *sample = S::from_f32(processed);
    }

    Ok(())
}

/// Turns a backend error into the `cpal::Error` the engine speaks.
///
/// The chain runs inside the stream's own error type, so a plugin failure has
/// to arrive as one of those. The backend's own message is carried through
/// whole — it is the only part that says what actually went wrong.
#[cfg(any(
    feature = "vst3",
    all(any(feature = "au", feature = "au3"), target_vendor = "apple")
))]
fn host_error(error: truce_rack_core::error::Error) -> Error {
    Error::with_message(ErrorKind::BackendError, error.to_string())
}

// -----------------------------------------------------------------------------
//  Descriptor-only traits
// -----------------------------------------------------------------------------

/// Clones the descriptor. What happens to the loaded instance depends on what
/// it is, and the difference is not worth hiding:
///
/// - An [internal plugin](internal) is a shared function, so the clone comes
///   back loaded too, running that same function.
/// - A hosted plugin is a live handle on a dylib or an XPC connection. A
///   second one of those has no sound meaning, so the clone comes back
///   unloaded and needs its own [`load`](Plugin::load).
///
/// `Clone` exists at all because [`AtomeDevice`](crate::device::AtomeDevice)
/// derives it and carries a plugin chain.
impl Clone for Plugin {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            path: self.path.clone(),
            buffer_size: self.buffer_size,
            sample_rate: self.sample_rate,
            channels: self.channels,
            params: self.params.clone(),
            format: self.format,
            inner: match &self.inner {
                Some(PluginInner::Internal(plugin)) => Some(PluginInner::Internal(plugin.clone())),
                _ => None,
            },
        }
    }
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("buffer_size", &self.buffer_size)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("params", &self.params)
            .field("format", &self.format)
            .field("loaded", &self.is_loaded())
            .finish()
    }
}

/// Compares descriptors, not instances.
///
/// Two plugins are equal when they describe the same plugin. The loaded
/// instance is a consequence of the descriptor, and a `dyn Fn` has no equality
/// to offer in any case — so a plugin equals its freshly-loaded self, which is
/// what makes this useful for finding one in a chain.
impl PartialEq for Plugin {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.path == other.path
            && self.buffer_size == other.buffer_size
            && self.sample_rate == other.sample_rate
            && self.channels == other.channels
            && self.params == other.params
            && self.format == other.format
    }
}

impl Eq for Plugin {}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(name: &str, format: PluginFormat) -> Plugin {
        Plugin::new(
            name.into(),
            PathBuf::new(),
            512,
            48_000,
            2,
            String::new(),
            format,
        )
    }

    #[test]
    fn an_unloaded_plugin_passes_audio_through() {
        let mut plugin = descriptor("never loaded", PluginFormat::Vst3);
        let mut buffer = [0.25_f32, -0.5];

        plugin.apply(&mut buffer, 2).expect("pass-through");

        assert_eq!(buffer, [0.25, -0.5]);
        assert!(!plugin.is_loaded());
    }

    #[test]
    fn internal_plugins_are_loaded_on_construction() {
        let plugin = Plugin::internal("gain", |_: &mut [f32], _| {});
        assert!(plugin.is_loaded());
        assert_eq!(plugin.format, PluginFormat::Internal);
    }

    #[test]
    fn applies_to_a_non_float_engine() {
        // i16 in, i16 out, with the plugin seeing f32 in between.
        let mut plugin = Plugin::internal("silence", |buffer: &mut [f32], _| {
            buffer.fill(0.0);
        });

        let mut buffer = [i16::MAX, i16::MIN, 1234];
        plugin.apply(&mut buffer, 1).expect("processed");

        assert_eq!(buffer, [0, 0, 0]);
    }

    #[test]
    fn round_trips_a_float_engine_untouched() {
        let mut plugin = Plugin::internal("noop", |_: &mut [f32], _| {});

        let mut buffer = [0.123_f32, -0.789, 1.0, -1.0];
        let before = buffer;
        plugin.apply(&mut buffer, 2).expect("processed");

        assert_eq!(buffer, before);
    }

    #[test]
    fn the_plugin_sees_the_interleaving_it_was_told_about() {
        let mut plugin = Plugin::internal("right only", |buffer: &mut [f32], channels| {
            for frame in buffer.chunks_mut(channels as usize) {
                frame[0] = 0.0;
            }
        });

        let mut buffer = [1.0_f32, 1.0, 1.0, 1.0];
        plugin.apply(&mut buffer, 2).expect("processed");

        assert_eq!(buffer, [0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn refuses_a_partial_frame() {
        let mut plugin = Plugin::internal("noop", |_: &mut [f32], _| {});

        // Three samples cannot be a whole number of stereo frames.
        let mut buffer = [0.0_f32; 3];
        assert!(plugin.apply(&mut buffer, 2).is_err());
    }

    #[test]
    fn refuses_zero_channels() {
        let mut plugin = Plugin::internal("noop", |_: &mut [f32], _| {});
        let mut buffer = [0.0_f32; 4];
        assert!(plugin.apply(&mut buffer, 0).is_err());
    }

    #[test]
    fn loads_an_internal_plugin_by_name() {
        internal::register("load by name", |buffer: &mut [f32], _| buffer.fill(1.0));

        let mut plugin = descriptor("load by name", PluginFormat::Internal);
        assert!(!plugin.is_loaded());
        plugin.load().expect("registered");
        assert!(plugin.is_loaded());

        let mut buffer = [0.0_f32; 2];
        plugin.apply(&mut buffer, 2).expect("processed");
        assert_eq!(buffer, [1.0, 1.0]);

        internal::unregister("load by name");
    }

    #[test]
    fn an_unregistered_name_is_an_error() {
        let mut plugin = descriptor("nothing registered under this", PluginFormat::Internal);
        let error = plugin.load().expect_err("not registered");
        assert!(error.to_string().contains("no internal plugin registered"));
    }

    #[test]
    fn loading_twice_is_a_no_op() {
        internal::register("loaded twice", |_: &mut [f32], _| {});

        let mut plugin = descriptor("loaded twice", PluginFormat::Internal);
        plugin.load().expect("first");
        plugin.load().expect("second");

        internal::unregister("loaded twice");
    }

    #[test]
    fn a_declared_latency_reaches_the_engine() {
        let lookahead = InternalPlugin::new("lookahead", |_: &mut [f32], _| {}).with_latency(128);
        assert_eq!(Plugin::from_internal(lookahead).latency(), 128);

        // Nothing loaded means nothing delayed.
        assert_eq!(descriptor("unloaded", PluginFormat::Vst3).latency(), 0);
    }

    #[test]
    fn cloning_keeps_an_internal_plugin_working() {
        let plugin = Plugin::internal("shared", |buffer: &mut [f32], _| buffer.fill(0.5));
        let mut clone = plugin.clone();

        assert!(clone.is_loaded());

        let mut buffer = [0.0_f32; 2];
        clone.apply(&mut buffer, 2).expect("processed");
        assert_eq!(buffer, [0.5, 0.5]);
    }

    #[test]
    fn equality_ignores_whether_a_plugin_is_loaded() {
        internal::register("compared", |_: &mut [f32], _| {});

        let unloaded = descriptor("compared", PluginFormat::Internal);
        let mut loaded = unloaded.clone();
        loaded.load().expect("registered");

        assert_eq!(unloaded, loaded);

        internal::unregister("compared");
    }

    #[test]
    fn a_path_overrides_a_defaulted_internal_format() {
        let plugin = Plugin::new(
            "Pro-Q".into(),
            "/Library/Audio/Plug-Ins/VST3/Pro-Q 3.vst3".into(),
            512,
            48_000,
            2,
            String::new(),
            PluginFormat::Internal,
        );

        assert_eq!(plugin.detect_format(), PluginFormat::Vst3);
    }

    #[test]
    fn bundle_suffixes_map_to_their_formats() {
        for (path, expected) in [
            ("/x/Thing.vst3", PluginFormat::Vst3),
            ("/x/Thing.component", PluginFormat::Au),
            ("/x/Thing.appex", PluginFormat::Au3),
            ("/x/Thing", PluginFormat::Internal),
        ] {
            let plugin = Plugin::new(
                "Thing".into(),
                path.into(),
                512,
                48_000,
                2,
                String::new(),
                PluginFormat::Internal,
            );
            assert_eq!(plugin.detect_format(), expected, "{path}");
        }
    }

    #[test]
    fn an_explicit_format_is_taken_at_its_word() {
        // A `.vst3` path does not make this anything other than the AU it says
        // it is.
        let plugin = Plugin::new(
            "Thing".into(),
            "/x/Thing.vst3".into(),
            512,
            48_000,
            2,
            String::new(),
            PluginFormat::Au,
        );

        assert_eq!(plugin.detect_format(), PluginFormat::Au);
    }

    #[test]
    fn internal_is_available_everywhere() {
        assert!(PluginFormat::Internal.is_available());
    }

    #[test]
    fn vst2_is_never_available() {
        // The feature exists; the backend does not. Loading says so, rather
        // than failing somewhere less obvious.
        assert!(!PluginFormat::Vst.is_available());

        let mut plugin = descriptor("some vst2", PluginFormat::Vst);
        let error = plugin.load().expect_err("no VST2 backend");
        assert!(error.to_string().contains("not implemented"));
    }

    #[test]
    fn an_unavailable_format_names_its_feature() {
        for format in [PluginFormat::Au, PluginFormat::Au3, PluginFormat::Vst3] {
            if format.is_available() {
                continue;
            }

            let mut plugin = descriptor("absent", format);
            let error = plugin.load().expect_err("feature is off");
            let message = error.to_string();

            assert!(
                message.contains("feature"),
                "{format:?} should say which feature is missing, said: {message}"
            );
        }
    }
}
