//! VST 2.4 plugin hosting. Enabled by the `vst` feature.
//!
//! Backed by the [`vst`](https://docs.rs/vst/0.4.0) crate, which is pulled in
//! by that same feature — start from its `host::PluginLoader` to load a plugin
//! and drive its `AEffect` dispatcher.
//!
//! Nothing is implemented here yet. Note also that the VST 2 SDK is no longer
//! licensed by Steinberg — check what you can ship before leaning on this one.
