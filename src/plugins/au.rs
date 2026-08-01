//! Audio Unit plugin hosting. Enabled by the `au` feature.
//!
//! macOS and iOS only — Audio Units are an Apple framework, so this module has
//! nothing to offer on other platforms even with the feature on.
//!
//! Nothing is implemented yet: hosting AUs means binding AudioToolbox /
//! AudioUnit, which needs a backend crate (`coreaudio-sys` or equivalent) added
//! as an optional dependency behind this feature first.
