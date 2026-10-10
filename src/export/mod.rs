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
//!
//! Each has a form taking a writer instead of a path — [`FlacWriter::new`],
//! [`write_flac_into`], [`to_flac_into`] — for output that is not a file of its
//! own: a `Cursor`, or an entry of a bundle. Anything `Write + Seek` will do.

pub mod flac;

pub use flac::{write_flac, write_flac_into, FlacSummary, FlacWriter};

#[cfg(feature = "import")]
pub use flac::{to_flac, to_flac_into};
