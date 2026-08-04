//! VST 2.4 plugin hosting. Enabled by the `vst` feature.
//!
//! **Not implemented, and not merely unfinished.** The feature exists so the
//! format can be named; [`PluginFormat::Vst`](super::PluginFormat::Vst)
//! reports [`is_available`](super::PluginFormat::is_available) as `false` on
//! every build, and [`Plugin::load`](super::Plugin::load) refuses it with a
//! message saying so. Use [VST 3](super::vst3) instead.
//!
//! # Why there is no backend
//!
//! The `truce-rack` family this crate hosts through has no VST2 crate, so
//! there is nothing to wire up the way [`vst3`](super::vst3) and [`au`](super::au)
//! are wired up. The `vst` crate on crates.io is a different shape entirely —
//! its own `AEffect` dispatcher rather than `truce-rack-core`'s traits — and
//! is unmaintained.
//!
//! The licence is the other half of it: Steinberg no longer issues the VST 2
//! SDK, so what a host may legally ship is a question to answer before writing
//! any of this, not after.
