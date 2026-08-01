//! VST 3 plugin hosting. Enabled by the `vst3` feature.
//!
//! Backed by the [`vst3-host`](https://docs.rs/vst3-host/0.9.0) crate, which is
//! pulled in by that same feature. Its `cpal-backend` is turned off — atome
//! drives the audio stream itself — but `process-isolation` is on, so a plugin
//! that crashes takes down only its own process.
//!
//! Nothing is implemented here yet.
