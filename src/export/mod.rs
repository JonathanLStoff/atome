//! Writing audio out.
//!
//! FLAC so far: lossless, royalty-free, and the format a video's audio is split
//! into when vtome imports a file, so it can be played here without the
//! video's own audio codec — AAC, usually — being decoded every time.
//!
//! - [`FlacWriter`] writes samples a block at a time, never holding a file
//!   whole.
//! - [`write_flac`] writes a [`Decoded`](crate::import::Decoded) in one call.
//! - [`to_flac`] decodes any file `import` reads — including the audio track
//!   of a video — straight into FLAC, streamed end to end. Needs `import` too.

pub mod flac;

pub use flac::{write_flac, FlacSummary, FlacWriter};

#[cfg(feature = "import")]
pub use flac::to_flac;
