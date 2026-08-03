//! Decoders, one per encoding, in two shapes.
//!
//! - `read_*` opens an [`AudioStream`] that decodes lazily into a sample type
//!   the *caller* picks, a block per call.
//! - `decode_*` reads the whole file at once, in the sample type the *file* is
//!   coded in.
//!
//! The difference is not an oversight. A stream writes into a buffer the caller
//! owns, so its type has to be known when the code is written; a one-shot
//! decode hands back a fresh buffer, so it can carry whatever the file turned
//! out to hold — and a 16-bit WAV really is `i16`, not something that has been
//! floated through `f32` on the way out. [`Samples::to_vec`](super::Samples::to_vec) converts when a
//! specific type is wanted after all.
//!
//! Both shapes are one line each over the shared pipeline below, so a codec is
//! only ever implemented once.
//!
//! # What backs each encoding
//!
//! | Encoding | Crate | Feature |
//! |---|---|---|
//! | PCM | `symphonia` | `import` |
//! | MP3 | `symphonia` | `import` |
//! | AAC-LC | `symphonia` | `import` |
//! | Vorbis | `symphonia` | `import` |
//! | FLAC | `symphonia` | `import` |
//! | ALAC | `symphonia` | `import` |
//! | HE-AAC v1/v2 | `symphonia-adapter-fdk-aac` | `import-he-aac` |
//! | Opus | `symphonia-adapter-libopus` | `import-opus` |
//! | AC-3, E-AC-3 | — | — |
//! | DTS, DTS-HD MA | — | — |
//! | Dolby TrueHD | — | — |
//! | WMA | — | — |
//! | AMR-NB, AMR-WB | — | — |
//!
//! Everything with a crate beside it works. The eight without one are not
//! oversights: nothing maintained in Rust decodes them, so they fail saying so
//! rather than pointing at a feature that would not help. Identification still
//! works — a DTS file is still correctly reported as DTS, it just cannot be
//! turned into samples.
//!
//! All eight working encodings share one pipeline. Symphonia splits container
//! from codec exactly as this module does, so the probe picks the demuxer and
//! the registry picks the decoder; the functions below only decide which cargo
//! feature had to be on to get there. That is why they are one-line wrappers
//! rather than eight separate decoders.
//!
//! They are dispatched to by [`stream_as`](super::stream_as) and
//! [`decode_as`](super::decode_as), which have already worked out the encoding
//! — nothing here identifies anything, it only decodes what it was told to.
//!
//! # Encoder delay and padding
//!
//! Lossy encoders prepend priming frames and pad the last packet, and playing
//! those back is what puts a click between two halves of a continuous
//! recording. Both are trimmed here wherever the file makes it possible:
//! Symphonia's decoders apply the per-packet trim the container declares, and
//! the stream additionally stops at the track's declared frame count, which is
//! what removes the trailing padding of the final packet.
//!
//! What cannot be fixed is a file that declares nothing. AAC in ADTS has no
//! way to signal its priming at all, and an MP4 written without an edit list
//! is no better off — such a file decodes with about 1024 frames of silence in
//! front, and no decoder can know to drop them.

use cpal::Error;
use std::path::Path;

use crate::output::SampleType;

use super::{feature_required, no_decoder, AudioStream, Decoded, Encoding};

#[cfg(feature = "import")]
mod backend {
    //! One Symphonia-backed [`AudioStream`] serving every encoding that has a
    //! decoder.
    //!
    //! Symphonia splits container from codec exactly as this module does, so
    //! there is nothing per-encoding here: the probe picks the demuxer, the
    //! registry picks the decoder, and the encoding this crate already
    //! identified only decides which cargo feature had to be on to get here.

    use std::fs::File;
    use std::path::Path;
    use std::sync::OnceLock;

    use cpal::{Error, ErrorKind, SampleFormat, I24, U24};
    use symphonia::core::audio::GenericAudioBufferRef;
    use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
    use symphonia::core::codecs::registry::CodecRegistry;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    use crate::output::SampleType;

    use crate::import::{AudioStream, Decoded, Samples};

    /// Decoders available to every stream this module opens.
    ///
    /// Built once: registering a decoder walks the codecs it claims, and doing
    /// that per file would be work repeated for no reason.
    fn codecs() -> &'static CodecRegistry {
        static CODECS: OnceLock<CodecRegistry> = OnceLock::new();

        CODECS.get_or_init(|| {
            // Built from scratch rather than taken from `get_codecs`, which
            // hands back a shared `&'static` that cannot be added to.
            let mut registry = CodecRegistry::new();
            symphonia::default::register_enabled_codecs(&mut registry);

            // The adapters register at the same tier as Symphonia's own
            // decoders and so replace them for the codecs they claim. That is
            // what is wanted for AAC: libfdk-aac handles LC as well as the SBR
            // and PS that Symphonia has no decoder for, so one decoder covers
            // all three profiles rather than two disagreeing about which owns
            // plain LC.
            #[cfg(feature = "import-he-aac")]
            registry.register_audio_decoder::<symphonia_adapter_fdk_aac::AacDecoder>();

            #[cfg(feature = "import-opus")]
            registry.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();

            registry
        })
    }

    /// Opens `path` for streamed decoding, converting into `S` as it goes.
    pub(super) fn open<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
        Ok(Box::new(SymphoniaStream::open(path)?))
    }

    /// A decoder and the demuxer feeding it, plus whatever the last decoded
    /// packet produced that the caller has not taken yet.
    struct SymphoniaStream<S: SampleType> {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn AudioDecoder>,
        /// Packets from other tracks — a video track, a second language — are
        /// skipped rather than fed to a decoder that would reject them.
        track_id: u32,
        sample_rate: u32,
        channels: u16,
        total_frames: Option<u64>,
        /// What the file itself is coded in, as opposed to `S`. Taken from the
        /// header, so it is the convention-based reading rather than the one a
        /// decoded packet would confirm.
        source_format: SampleFormat,
        /// A decoded packet holds however many frames it holds, which will not
        /// line up with what the caller asks for, so the remainder waits here,
        /// already converted into `S`.
        pending: Vec<S>,
        /// Where a packet lands before conversion.
        ///
        /// Symphonia converts into its own sample traits, which `S` does not
        /// implement, so the bridge is `f32`: wide enough to carry i16 and i24
        /// exactly, and lossy only for the bottom bits of i32, u32, and f64.
        /// Kept as a field so the per-packet allocation happens once.
        scratch: Vec<f32>,
        /// How much of `pending` has already been handed over.
        taken: usize,
        /// Samples handed over so far, counted against `limit`.
        ///
        /// Samples rather than frames because a caller's buffer need not be a
        /// whole number of frames: counting frames would lose the remainder on
        /// every odd-sized read and let the limit drift.
        emitted: u64,
        /// Total samples the track declares, if it declares any.
        limit: Option<u64>,
    }

    /// Everything opening a file produces, before any audio is decoded.
    struct Opened {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn AudioDecoder>,
        track_id: u32,
        sample_rate: u32,
        channels: u16,
        total_frames: Option<u64>,
        /// Bits per sample as the container declares them, which is the only
        /// reliable statement of the file's width.
        bits: Option<u32>,
    }

    /// Probes `path`, picks its audio track, and builds a decoder for it.
    ///
    /// Shared by the streaming and the one-shot paths so there is one place
    /// that decides which track plays and what it is.
    fn open_parts(path: &Path) -> Result<Opened, Error> {
        let file = File::open(path).map_err(|error| {
            let kind = match error.kind() {
                std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                _ => ErrorKind::Other,
            };
            Error::with_message(kind, format!("could not read {}: {error}", path.display()))
        })?;

        let source = MediaSourceStream::new(Box::new(file), Default::default());

        // No `Hint`: the extension is exactly the thing this crate refuses to
        // trust, and the probe finds the container from its bytes.
        let format = symphonia::default::get_probe()
            .probe(
                &Hint::new(),
                source,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|error| decode_failed(path, "container", error))?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| unreadable(path, "no audio track"))?;

        let track_id = track.id;
        let total_frames = track.num_frames;

        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| unreadable(path, "audio track declares no codec"))?;

        // `gapless` is on by default and is what trims encoder delay and
        // padding — the difference between a seamless album and a click
        // between every track.
        let decoder = codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|error| decode_failed(path, "codec", error))?;

        // The decoder is the authority on rate and channel count, not the
        // container: HE-AAC decodes at twice the rate its frame headers
        // declare, and Opus always comes out at 48 kHz whatever it went in as.
        // Falling back to the container's values only when the decoder has
        // none.
        let decoded = decoder.codec_params();
        let sample_rate = decoded
            .sample_rate
            .or(params.sample_rate)
            .ok_or_else(|| unreadable(path, "no sample rate"))?;
        let channels = decoded
            .channels
            .as_ref()
            .or(params.channels.as_ref())
            .map(|channels| channels.count())
            .ok_or_else(|| unreadable(path, "no channel count"))?;

        // `sample_format` is left unset by every decoder tried here, so the
        // bit depth is what there is to go on.
        let bits = decoded.bits_per_sample.or(params.bits_per_sample);

        Ok(Opened {
            format,
            decoder,
            track_id,
            sample_rate,
            channels: channels as u16,
            total_frames,
            bits,
        })
    }

    /// Whether a decoded buffer holds unsigned integers, signed integers, or
    /// floats — the half of the answer a bit depth cannot give.
    #[derive(Clone, Copy)]
    enum Kind {
        Unsigned,
        Signed,
        Float,
    }

    fn buffer_kind(buffer: &GenericAudioBufferRef<'_>) -> Kind {
        match buffer {
            GenericAudioBufferRef::U8(_)
            | GenericAudioBufferRef::U16(_)
            | GenericAudioBufferRef::U24(_)
            | GenericAudioBufferRef::U32(_) => Kind::Unsigned,
            GenericAudioBufferRef::S8(_)
            | GenericAudioBufferRef::S16(_)
            | GenericAudioBufferRef::S24(_)
            | GenericAudioBufferRef::S32(_) => Kind::Signed,
            GenericAudioBufferRef::F32(_) | GenericAudioBufferRef::F64(_) => Kind::Float,
        }
    }

    /// The format the file's samples are in.
    ///
    /// Two facts decide it and neither is enough alone. The declared bit depth
    /// gives the width — a 16-bit FLAC says 16 even though its decoder hands
    /// back `i32` buffers, so trusting the buffer would report every FLAC as
    /// 32-bit. The buffer says whether those bits are signed, unsigned, or
    /// floating point — 8-bit PCM is unsigned where everything wider is signed,
    /// so trusting the depth alone would get `u8` wrong.
    ///
    /// Before anything has been decoded there is no buffer, and convention
    /// fills that half in: 8-bit unsigned, wider signed, and a codec declaring
    /// no width at all is a lossy one that decodes to float.
    fn native_format(bits: Option<u32>, kind: Option<Kind>) -> SampleFormat {
        let kind = kind.unwrap_or(match bits {
            Some(8) => Kind::Unsigned,
            Some(_) => Kind::Signed,
            None => Kind::Float,
        });

        match (kind, bits) {
            (Kind::Float, Some(64)) => SampleFormat::F64,
            (Kind::Float, _) => SampleFormat::F32,
            (Kind::Unsigned, Some(..=8)) => SampleFormat::U8,
            (Kind::Unsigned, Some(9..=16)) => SampleFormat::U16,
            (Kind::Unsigned, Some(17..=24)) => SampleFormat::U24,
            (Kind::Unsigned, _) => SampleFormat::U32,
            (Kind::Signed, Some(..=8)) => SampleFormat::I8,
            (Kind::Signed, Some(9..=16)) => SampleFormat::I16,
            (Kind::Signed, Some(17..=24)) => SampleFormat::I24,
            (Kind::Signed, _) => SampleFormat::I32,
        }
    }

    /// Decodes the whole file, keeping the sample type the file is coded in.
    ///
    /// The decoded buffer is a tagged union in Symphonia too, so its variant is
    /// the answer — no guessing from the header, and no conversion on the way
    /// out.
    pub(super) fn decode(path: &Path) -> Result<Decoded, Error> {
        let Opened {
            mut format,
            mut decoder,
            track_id,
            sample_rate,
            channels,
            total_frames,
            bits,
        } = open_parts(path)?;

        let limit = total_frames.map(|frames| frames.saturating_mul(channels.max(1) as u64));
        let mut collected: Option<Samples> = None;
        let mut emitted = 0u64;

        loop {
            if matches!(limit, Some(limit) if emitted >= limit) {
                break;
            }

            let packet = match format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) | Err(SymphoniaError::ResetRequired) => break,
                Err(error) => return Err(stream_failed("demuxing", error)),
            };

            if packet.track_id != track_id {
                continue;
            }

            let buffer = match decoder.decode(&packet) {
                Ok(buffer) => buffer,
                // One corrupt packet is not a corrupt file.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::ResetRequired) => break,
                Err(error) => return Err(stream_failed("decoding", error)),
            };

            // The first packet settles the format, since it is the only place
            // the signedness shows. Every later packet converts into that same
            // type, which is what keeps one file to one `Samples` variant.
            let samples = collected.get_or_insert_with(|| {
                empty_for(native_format(bits, Some(buffer_kind(&buffer))))
            });
            let room = limit.map(|limit| (limit - emitted) as usize);
            emitted += append(samples, &buffer, room) as u64;
        }

        Ok(Decoded {
            // An empty file never produced a packet, so the header's word is
            // all there is.
            samples: collected.unwrap_or_else(|| empty_for(native_format(bits, None))),
            sample_rate,
            channels,
        })
    }

    /// An empty buffer of the given format.
    fn empty_for(format: SampleFormat) -> Samples {
        match format {
            SampleFormat::U8 => Samples::U8(Vec::new()),
            SampleFormat::I8 => Samples::I8(Vec::new()),
            SampleFormat::U16 => Samples::U16(Vec::new()),
            SampleFormat::I16 => Samples::I16(Vec::new()),
            SampleFormat::U24 => Samples::U24(Vec::new()),
            SampleFormat::I24 => Samples::I24(Vec::new()),
            SampleFormat::U32 => Samples::U32(Vec::new()),
            SampleFormat::I32 => Samples::I32(Vec::new()),
            SampleFormat::F64 => Samples::F64(Vec::new()),
            // Includes the 64-bit integer and DSD formats cpal names but no
            // decoder here produces.
            _ => Samples::F32(Vec::new()),
        }
    }

    /// Appends a decoded packet, in its own type, and reports how many samples
    /// were taken.
    ///
    /// `room` caps the append at the track's declared length, which is what
    /// trims the final packet's encoder padding.
    fn append(samples: &mut Samples, buffer: &GenericAudioBufferRef<'_>, room: Option<usize>) -> usize {
        /// Copies into a scratch of the packet's own type, then appends as much
        /// of it as `room` allows.
        macro_rules! take {
            ($out:expr, $scratch_ty:ty, $convert:expr) => {{
                let mut scratch: Vec<$scratch_ty> = Vec::new();
                buffer.copy_to_vec_interleaved(&mut scratch);

                let count = room.map_or(scratch.len(), |room| room.min(scratch.len()));
                $out.extend(scratch[..count].iter().copied().map($convert));
                count
            }};
        }

        match samples {
            Samples::U8(out) => take!(out, u8, |sample| sample),
            Samples::I8(out) => take!(out, i8, |sample| sample),
            Samples::U16(out) => take!(out, u16, |sample| sample),
            Samples::I16(out) => take!(out, i16, |sample| sample),
            Samples::U32(out) => take!(out, u32, |sample| sample),
            Samples::I32(out) => take!(out, i32, |sample| sample),
            Samples::F32(out) => take!(out, f32, |sample| sample),
            Samples::F64(out) => take!(out, f64, |sample| sample),
            // The 24-bit types are the two where Symphonia and cpal disagree:
            // both wrap a 32-bit integer, but they are different types, and
            // Symphonia makes no promise its value is in range.
            Samples::U24(out) => take!(out, symphonia::core::audio::sample::u24, |sample| {
                U24::new(sample.0 as i32).unwrap_or(<U24 as SampleType>::SILENCE)
            }),
            Samples::I24(out) => take!(out, symphonia::core::audio::sample::i24, |sample| {
                I24::new(sample.0).unwrap_or(<I24 as SampleType>::SILENCE)
            }),
        }
    }

    impl<S: SampleType> SymphoniaStream<S> {
        fn open(path: &Path) -> Result<Self, Error> {
            let Opened {
                format,
                decoder,
                track_id,
                sample_rate,
                channels,
                total_frames,
                bits,
            } = open_parts(path)?;

            Ok(SymphoniaStream {
                format,
                decoder,
                track_id,
                sample_rate,
                channels,
                total_frames,
                source_format: native_format(bits, None),
                pending: Vec::new(),
                scratch: Vec::new(),
                taken: 0,
                emitted: 0,
                limit: total_frames.map(|frames| frames.saturating_mul(channels.max(1) as u64)),
            })
        }

        /// Decodes packets until one yields audio, leaving it in `pending`.
        ///
        /// Returns `false` at the end of the track.
        fn decode_next(&mut self) -> Result<bool, Error> {
            loop {
                let packet = match self.format.next_packet() {
                    Ok(Some(packet)) => packet,
                    Ok(None) => return Ok(false),
                    // A reset means the track list changed underneath us, which
                    // this stream has no way to follow — end rather than
                    // silently decode something else.
                    Err(SymphoniaError::ResetRequired) => return Ok(false),
                    Err(error) => return Err(stream_failed("demuxing", error)),
                };

                if packet.track_id != self.track_id {
                    continue;
                }

                match self.decoder.decode(&packet) {
                    Ok(decoded) => {
                        self.scratch.clear();
                        decoded.copy_to_vec_interleaved(&mut self.scratch);

                        self.pending.clear();
                        self.pending
                            .extend(self.scratch.iter().copied().map(S::from_f32));
                        self.taken = 0;

                        // A packet can legitimately decode to nothing — a
                        // priming frame wholly trimmed by gapless handling —
                        // so keep going rather than reporting the end.
                        if !self.pending.is_empty() {
                            return Ok(true);
                        }
                    }
                    // One corrupt packet is not a corrupt file: skip it and
                    // carry on, which is what a player does.
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(SymphoniaError::ResetRequired) => return Ok(false),
                    Err(error) => return Err(stream_failed("decoding", error)),
                }
            }
        }
    }

    impl<S: SampleType> AudioStream<S> for SymphoniaStream<S> {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn total_frames(&self) -> Option<u64> {
            self.total_frames
        }

        fn source_format(&self) -> SampleFormat {
            self.source_format
        }

        fn read(&mut self, out: &mut [S]) -> Result<usize, Error> {
            // Stop at the track's declared length. A decoder emits whole
            // packets, so the last one runs past the end of the audio and into
            // the encoder's padding; the container's frame count is the only
            // thing that says where the music actually stopped.
            //
            // This only ever trims. Where a container over-declares — Ogg Opus
            // counts the frames it will pre-skip — the limit sits above what
            // the decoder produces and nothing is cut.
            let remaining = match self.limit {
                Some(limit) if self.emitted >= limit => return Ok(0),
                Some(limit) => Some(limit - self.emitted),
                None => None,
            };

            if self.taken >= self.pending.len() && !self.decode_next()? {
                return Ok(0);
            }

            let available = &self.pending[self.taken..];
            let mut count = available.len().min(out.len());
            if let Some(remaining) = remaining {
                count = count.min(remaining as usize);
            }

            out[..count].copy_from_slice(&available[..count]);
            self.taken += count;
            self.emitted += count as u64;

            Ok(count)
        }
    }

    /// The file is not the format it was identified as, or is damaged.
    fn decode_failed(path: &Path, what: &str, error: SymphoniaError) -> Error {
        Error::with_message(
            ErrorKind::InvalidInput,
            format!("could not read the {what} of {}: {error}", path.display()),
        )
    }

    /// The file parsed but does not carry what decoding needs.
    fn unreadable(path: &Path, what: &str) -> Error {
        Error::with_message(
            ErrorKind::InvalidInput,
            format!("{}: {what}", path.display()),
        )
    }

    /// A failure part-way through, once decoding is under way.
    fn stream_failed(what: &str, error: SymphoniaError) -> Error {
        let kind = match error {
            SymphoniaError::IoError(_) => ErrorKind::Other,
            SymphoniaError::Unsupported(_) => ErrorKind::UnsupportedOperation,
            _ => ErrorKind::InvalidInput,
        };

        Error::with_message(kind, format!("{what} failed: {error}"))
    }
}

// ---------------------------------------------------------------------------
// Uncompressed
// ---------------------------------------------------------------------------

/// Streams PCM.
///
/// The one case with no codec: the container declares a width and an
/// endianness, and the samples convert straight across. Widths in the wild are
/// u8, i16, i24, i32, f32, and f64, plus A-law and µ-law, which expand through
/// their tables rather than scaling.
///
/// The format that gains most from streaming rather than [`decode_pcm`]: an
/// hour of stereo is well over a gigabyte once it is `f32` in memory, and it
/// was already the bulkiest thing on disk before decoding.
pub(super) fn read_pcm<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    #[cfg(feature = "import")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("PCM decoding", "import"))
    }
}

/// Decodes PCM in full. See [`read_pcm`] for the work.
pub(super) fn decode_pcm(path: &Path) -> Result<Decoded, Error> {
    #[cfg(feature = "import")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("PCM decoding", "import"))
    }
}

// ---------------------------------------------------------------------------
// Lossy
// ---------------------------------------------------------------------------

/// Streams MP3.
///
/// The VBR headers (Xing/Info/VBRI) and the LAME delay/padding pair are read
/// for us, which is what makes playback gapless and what decides whether
/// [`total_frames`](AudioStream::total_frames) can answer at all — a VBR file
/// carrying none of those headers has no trustworthy length.
///
/// Frames borrow from their predecessors through the "bit reservoir", so a
/// decoder cannot start cleanly at an arbitrary frame. That costs nothing here,
/// where reading only ever goes forward, but it is the reason seeking to an
/// exact sample will need decoding to run up to it.
pub(super) fn read_mp3<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    #[cfg(feature = "import")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("MP3 decoding", "import"))
    }
}

/// Decodes MP3 in full. See [`read_mp3`] for the work.
pub(super) fn decode_mp3(path: &Path) -> Result<Decoded, Error> {
    #[cfg(feature = "import")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("MP3 decoding", "import"))
    }
}

/// Streams AAC.
///
/// Two decoders behind one function, which is why `encoding` is passed in:
///
/// - [`Encoding::AacLc`] is Symphonia's native decoder, on `import`.
/// - [`Encoding::AacHe`] and [`Encoding::AacHeV2`] are not — Symphonia has no
///   SBR or PS — so they need `symphonia-adapter-fdk-aac` and the
///   `import-he-aac` feature.
///
/// The profile is checked here rather than left to the decoder because of what
/// going wrong looks like: an HE-AAC stream handed to an LC-only decoder does
/// not fail, it decodes the LC core and drops the high band, so the file plays
/// through sounding dull and no error is ever raised. Refusing up front turns a
/// silent quality loss into a build-time missing feature.
///
/// HE-AAC reconstructs the high band from SBR data and v2 rebuilds stereo from
/// a PS-coded mono core, so both output at twice the core's sample rate. The
/// stream reports the decoder's rate rather than the container's for exactly
/// this reason.
pub(super) fn read_aac<S: SampleType>(
    path: &Path,
    encoding: Encoding,
) -> Result<Box<dyn AudioStream<S>>, Error> {
    // HE-AAC needs the adapter, whether or not plain `import` is on.
    if matches!(encoding, Encoding::AacHe | Encoding::AacHeV2) && !cfg!(feature = "import-he-aac") {
        return Err(feature_required("HE-AAC decoding", "import-he-aac"));
    }

    #[cfg(feature = "import")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("AAC decoding", "import"))
    }
}

/// Decodes AAC in full. See [`read_aac`] for the work.
pub(super) fn decode_aac(path: &Path, encoding: Encoding) -> Result<Decoded, Error> {
    // The same refusal as `read_aac`: an LC-only decoder would take an HE-AAC
    // file and quietly drop its high band.
    if matches!(encoding, Encoding::AacHe | Encoding::AacHeV2) && !cfg!(feature = "import-he-aac") {
        return Err(feature_required("HE-AAC decoding", "import-he-aac"));
    }

    #[cfg(feature = "import")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("AAC decoding", "import"))
    }
}

/// Streams Opus, via `symphonia-adapter-libopus`.
///
/// Decodes at 48 kHz whatever rate it was encoded at, so the rate reported is
/// the decoder's and not the container's. The `pre_skip` frames named in
/// `OpusHead` are swallowed during the first reads, so the first sample handed
/// back is the first real one.
pub(super) fn read_opus<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    #[cfg(feature = "import-opus")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import-opus"))]
    {
        let _ = path;
        Err(feature_required("Opus decoding", "import-opus"))
    }
}

/// Decodes Opus in full. See [`read_opus`] for the work.
pub(super) fn decode_opus(path: &Path) -> Result<Decoded, Error> {
    #[cfg(feature = "import-opus")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import-opus"))]
    {
        let _ = path;
        Err(feature_required("Opus decoding", "import-opus"))
    }
}

/// Streams Vorbis.
///
/// The three setup headers — identification, comment, setup — are consumed when
/// the stream is opened, since no audio packet will decode until they have been
/// read. A file truncated before the end of them fails at [`stream`] rather
/// than part-way through reading.
///
/// [`stream`]: super::stream
pub(super) fn read_vorbis<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    #[cfg(feature = "import")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("Vorbis decoding", "import"))
    }
}

/// Decodes Vorbis in full. See [`read_vorbis`] for the work.
pub(super) fn decode_vorbis(path: &Path) -> Result<Decoded, Error> {
    #[cfg(feature = "import")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("Vorbis decoding", "import"))
    }
}

/// No Rust decoder for WMA — WMA v1/v2, Pro, Lossless, and Voice are four
/// distinct codecs behind one name, and none of them has one.
pub(super) fn read_wma<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("WMA streaming"))
}

/// Fails the same way [`read_wma`] does.
pub(super) fn decode_wma(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("WMA decoding"))
}

// ---------------------------------------------------------------------------
// Lossless
// ---------------------------------------------------------------------------

/// Streams FLAC.
///
/// The same codec whether it arrived in FLAC's own container, in Ogg, or in
/// MP4 — only the framing differs, and the demuxer absorbs that difference
/// before the decoder sees a packet.
///
/// `STREAMINFO` carries an exact frame count, so this is one of the few formats
/// where [`total_frames`](AudioStream::total_frames) is exact rather than
/// absent or a guess.
pub(super) fn read_flac<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    #[cfg(feature = "import")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("FLAC decoding", "import"))
    }
}

/// Decodes FLAC in full. See [`read_flac`] for the work.
pub(super) fn decode_flac(path: &Path) -> Result<Decoded, Error> {
    #[cfg(feature = "import")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("FLAC decoding", "import"))
    }
}

/// Streams ALAC.
///
/// The codec carries no self-describing header, so the decoder is configured
/// from the magic cookie in the container. That is why a bare ALAC bitstream
/// with nothing wrapped around it cannot be opened at all — there is nowhere
/// for the cookie to have come from.
pub(super) fn read_alac<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    #[cfg(feature = "import")]
    {
        backend::open(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("ALAC decoding", "import"))
    }
}

/// Decodes ALAC in full. See [`read_alac`] for the work.
pub(super) fn decode_alac(path: &Path) -> Result<Decoded, Error> {
    #[cfg(feature = "import")]
    {
        backend::decode(path)
    }

    #[cfg(not(feature = "import"))]
    {
        let _ = path;
        Err(feature_required("ALAC decoding", "import"))
    }
}

/// No Rust decoder for Dolby TrueHD. MLP-based and lossless; it arrives from
/// Blu-ray and inside Matroska, and nothing in Rust decodes it.
pub(super) fn read_truehd<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("Dolby TrueHD streaming"))
}

/// Fails the same way [`read_truehd`] does.
pub(super) fn decode_truehd(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("Dolby TrueHD decoding"))
}

// ---------------------------------------------------------------------------
// Surround
// ---------------------------------------------------------------------------

/// No Rust decoder for AC-3. Partial implementations exist in the `rust-av`
/// project, but nothing published and maintained to depend on.
pub(super) fn read_ac3<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("AC-3 streaming"))
}

/// Fails the same way [`read_ac3`] does.
pub(super) fn decode_ac3(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("AC-3 decoding"))
}

/// No Rust decoder for E-AC-3 — a superset of AC-3 in coding tools, so it does
/// not come for free even if [`read_ac3`] ever gets one.
pub(super) fn read_eac3<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("E-AC-3 streaming"))
}

/// Fails the same way [`read_eac3`] does.
pub(super) fn decode_eac3(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("E-AC-3 decoding"))
}

/// No Rust decoder for DTS.
pub(super) fn read_dts<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("DTS streaming"))
}

/// Fails the same way [`read_dts`] does.
pub(super) fn decode_dts(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("DTS decoding"))
}

/// No Rust decoder for DTS-HD Master Audio. Lossless, and layered on top of a
/// lossy DTS core, so it needs the extension substream as well as the core that
/// [`read_dts`] has no decoder for either.
pub(super) fn read_dts_hd_ma<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("DTS-HD Master Audio streaming"))
}

/// Fails the same way [`read_dts_hd_ma`] does.
pub(super) fn decode_dts_hd_ma(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("DTS-HD Master Audio decoding"))
}

// ---------------------------------------------------------------------------
// Speech
// ---------------------------------------------------------------------------

/// No Rust decoder for AMR narrowband. 8 kHz, mono, speech only. Bindings to
/// the reference C codec would be the way in, but none are published.
pub(super) fn read_amr_nb<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("AMR-NB streaming"))
}

/// Fails the same way [`read_amr_nb`] does.
pub(super) fn decode_amr_nb(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("AMR-NB decoding"))
}

/// No Rust decoder for AMR wideband. 16 kHz, mono, speech only.
pub(super) fn read_amr_wb<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let _ = path;

    Err(no_decoder("AMR-WB streaming"))
}

/// Fails the same way [`read_amr_wb`] does.
pub(super) fn decode_amr_wb(path: &Path) -> Result<Decoded, Error> {
    let _ = path;

    Err(no_decoder("AMR-WB decoding"))
}
