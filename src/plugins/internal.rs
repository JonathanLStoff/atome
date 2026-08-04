//! Internal plugins: DSP written in Rust and compiled into the host.
//!
//! Unlike the other formats in this module there is nothing to scan for and
//! nothing to dlopen — an internal plugin *is* a function, and loading one is
//! a matter of putting that function where [`Plugin::apply`](super::Plugin::apply)
//! can reach it.
//!
//! Always compiled in: internal plugins link against nothing, so they need no
//! Cargo feature.
//!
//! # Two ways in
//!
//! Hand the function over directly, when you have it at the call site:
//!
//! ```
//! use atome::plugins::Plugin;
//!
//! let mut gain = Plugin::internal("half gain", |buffer: &mut [f32], _channels| {
//!     for sample in buffer {
//!         *sample *= 0.5;
//!     }
//! });
//! ```
//!
//! Or register it under a name and load it by that name later, which is what
//! you want when the chain is described by configuration rather than by code:
//!
//! ```
//! use atome::plugins::{internal, Plugin, PluginFormat};
//!
//! internal::register("half gain", |buffer: &mut [f32], _channels| {
//!     for sample in buffer {
//!         *sample *= 0.5;
//!     }
//! });
//!
//! let mut plugin = Plugin::new(
//!     "half gain".into(),
//!     "".into(),
//!     512,
//!     48_000,
//!     2,
//!     String::new(),
//!     PluginFormat::Internal,
//! );
//! plugin.load().unwrap();
//! ```
//!
//! # The signature
//!
//! Every internal plugin has the same shape: an interleaved `f32` buffer and
//! the channel count it is interleaved by, processed in place.
//!
//! `f32` rather than a generic sample type because a plugin is stored
//! type-erased behind a `dyn Fn`, and one boxed function cannot be generic
//! over whatever sample type the engine happens to be running. `f32` is also
//! what every other plugin format processes in, so this is the same conversion
//! the hosted formats pay rather than an extra one.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

/// What an internal plugin does to a block of audio.
///
/// `buffer` is interleaved by `channels`, so frame `f` of channel `c` lives at
/// `buffer[f * channels as usize + c]`. It is processed in place: whatever is
/// left in the buffer when the function returns is what the next plugin in the
/// chain hears.
///
/// `Send + Sync` because a plugin travels to the thread that owns the stream,
/// and `Fn` rather than `FnMut` because the same registered plugin can be
/// attached in several places at once. State that has to persist between
/// blocks goes behind the function's own interior mutability.
pub type ProcessFn = dyn Fn(&mut [f32], u16) + Send + Sync + 'static;

/// A named processing function, ready to be applied.
///
/// Cheap to clone — the function itself is shared, not copied — which is what
/// lets one registered plugin be attached to several devices.
#[derive(Clone)]
pub struct InternalPlugin {
    name: String,
    process: Arc<ProcessFn>,
    latency: usize,
}

impl InternalPlugin {
    /// Wraps a function up as a plugin.
    ///
    /// Latency defaults to zero; if the function delays its output, say so
    /// with [`with_latency`](Self::with_latency) so the engine can compensate.
    pub fn new<F>(name: impl Into<String>, process: F) -> Self
    where
        F: Fn(&mut [f32], u16) + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            process: Arc::new(process),
            latency: 0,
        }
    }

    /// Declares how many frames of delay this plugin introduces.
    ///
    /// Only meaningful for a plugin that actually delays its output — a
    /// lookahead limiter, an FFT-based effect, anything with a fixed block
    /// latency. Reported back through
    /// [`Plugin::latency`](super::Plugin::latency).
    #[must_use]
    pub fn with_latency(mut self, frames: usize) -> Self {
        self.latency = frames;
        self
    }

    /// The name this plugin was created or registered under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The delay this plugin introduces, in frames.
    pub fn latency(&self) -> usize {
        self.latency
    }

    /// Runs the function over one block.
    ///
    /// `buffer` is interleaved by `channels` and is processed in place.
    pub fn process(&self, buffer: &mut [f32], channels: u16) {
        (self.process)(buffer, channels)
    }
}

impl fmt::Debug for InternalPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The function has nothing printable about it, so the name is the
        // whole of the useful output.
        f.debug_struct("InternalPlugin")
            .field("name", &self.name)
            .field("latency", &self.latency)
            .finish_non_exhaustive()
    }
}

// -----------------------------------------------------------------------------
//  Registry
// -----------------------------------------------------------------------------

/// The process-wide name→plugin map backing [`register`] and [`get`].
///
/// A `RwLock` because registration happens once at startup and lookup happens
/// per `load()` — many readers, almost no writers. Nothing here is touched
/// from the audio thread: [`Plugin::load`](super::Plugin::load) resolves the
/// name once and keeps its own clone of the function, so `apply` never takes
/// the lock.
fn registry() -> &'static RwLock<HashMap<String, InternalPlugin>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, InternalPlugin>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Registers a function under a name, so a [`Plugin`](super::Plugin) built
/// with [`PluginFormat::Internal`](super::PluginFormat) can find it by that
/// name.
///
/// Returns whatever was registered under the name before, if anything.
/// Registering over a name does not disturb plugins already loaded from it —
/// they hold their own handle on the function they were given.
pub fn register<F>(name: impl Into<String>, process: F) -> Option<InternalPlugin>
where
    F: Fn(&mut [f32], u16) + Send + Sync + 'static,
{
    register_plugin(InternalPlugin::new(name, process))
}

/// Registers an already-built [`InternalPlugin`] under its own name.
///
/// The form to use when the plugin declares a latency, since [`register`] has
/// nowhere to put one.
pub fn register_plugin(plugin: InternalPlugin) -> Option<InternalPlugin> {
    let mut map = registry().write().unwrap_or_else(|e| e.into_inner());
    map.insert(plugin.name.clone(), plugin)
}

/// Looks a registered plugin up by name.
pub fn get(name: &str) -> Option<InternalPlugin> {
    let map = registry().read().unwrap_or_else(|e| e.into_inner());
    map.get(name).cloned()
}

/// Removes a registration, returning it if it was there.
///
/// Plugins already loaded from this name keep working — they hold the function
/// itself, not a reference to the registry.
pub fn unregister(name: &str) -> Option<InternalPlugin> {
    let mut map = registry().write().unwrap_or_else(|e| e.into_inner());
    map.remove(name)
}

/// Every registered name, in no particular order.
pub fn registered() -> Vec<String> {
    let map = registry().read().unwrap_or_else(|e| e.into_inner());
    map.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn processes_in_place() {
        let plugin = InternalPlugin::new("double", |buffer: &mut [f32], _| {
            for sample in buffer {
                *sample *= 2.0;
            }
        });

        let mut buffer = [0.25, -0.5, 0.125, 0.0];
        plugin.process(&mut buffer, 2);

        assert_eq!(buffer, [0.5, -1.0, 0.25, 0.0]);
    }

    #[test]
    fn sees_the_interleaving() {
        // Silences every channel but the first, which is only possible if the
        // function is told how the buffer is interleaved.
        let plugin = InternalPlugin::new("left only", |buffer: &mut [f32], channels| {
            for frame in buffer.chunks_mut(channels as usize) {
                for sample in &mut frame[1..] {
                    *sample = 0.0;
                }
            }
        });

        let mut buffer = [1.0, 1.0, 1.0, 1.0];
        plugin.process(&mut buffer, 2);

        assert_eq!(buffer, [1.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn latency_defaults_to_zero_and_is_declarable() {
        let plugin = InternalPlugin::new("noop", |_: &mut [f32], _| {});
        assert_eq!(plugin.latency(), 0);
        assert_eq!(plugin.with_latency(64).latency(), 64);
    }

    #[test]
    fn registry_round_trip() {
        register("registry round trip", |buffer: &mut [f32], _| {
            buffer.fill(1.0);
        });

        let found = get("registry round trip").expect("just registered");
        let mut buffer = [0.0; 3];
        found.process(&mut buffer, 1);
        assert_eq!(buffer, [1.0; 3]);

        assert!(registered().iter().any(|n| n == "registry round trip"));
        assert!(unregister("registry round trip").is_some());
        assert!(get("registry round trip").is_none());
    }

    #[test]
    fn a_clone_shares_the_function() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        let plugin = InternalPlugin::new("counter", move |_: &mut [f32], _| {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        let clone = plugin.clone();

        plugin.process(&mut [], 1);
        clone.process(&mut [], 1);

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
