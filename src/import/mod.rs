//! Working out what an audio file actually is, and decoding it.
//!
//! Two separate questions, and conflating them is the usual mistake:
//!
//! - **Container** — how the file is wrapped. `.ogg`, `.mp4`, `.wav`.
//! - **Encoding** — how the audio itself is coded. AAC, Opus, PCM.
//!
//! One encoding turns up in many containers (AAC lives in MP4, ADTS, Matroska,
//! and more) and one container carries many encodings (Ogg holds Vorbis, Opus,
//! FLAC, or Speex). So identification is two steps: recognise the container by
//! its magic bytes, then read *that container's* header to find which encoding
//! is inside. Neither step looks at the file name.
//!
//! This module handles identification; the decoders themselves live one per
//! encoding in [`audio`]. There are two ways in:
//!
//! - [`stream`] decodes lazily, a block at a time, so memory is the caller's
//!   buffer rather than the whole file.
//! - [`decode`] runs that same stream to the end and hands back everything at
//!   once, for when the audio is known to be small.
//!
//! Supported encoding types:
//! * PCM
//! * MP3
//! * AAC (LC, HE-AAC, HE-AAC v2)
//! * Opus
//! * Vorbis
//! * FLAC
//! * ALAC
//! * AC-3 (Dolby Digital)
//! * E-AC-3 (Dolby Digital Plus)
//! * DTS
//! * DTS-HD Master Audio (DTS-HD MA)
//! * Dolby TrueHD
//! * WMA
//! * AMR-NB
//! * AMR-WB

use cpal::{Error, ErrorKind, SampleFormat, I24, U24};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::output::SampleType;

pub mod audio;

/// Bytes read from the front of a file to identify it. Every container magic
/// checked here lives well inside this; MP3 is the one that may need a second
/// look further in.
const HEADER_LEN: usize = 32;

/// How far past an ID3 tag to look for the first MPEG frame. Tags are often
/// followed by padding or junk, so the audio does not always start flush
/// against the end of the tag.
const MPEG_SYNC_SEARCH: usize = 8 * 1024;

/// The 16-byte GUID every ASF file (`.wma`, `.wmv`) starts with.
const ASF_HEADER_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];

/// GUID of an ASF Stream Properties Object, in ASF's own mixed byte order.
const ASF_STREAM_PROPERTIES_GUID: [u8; 16] = [
    0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];

/// GUID marking an ASF stream as audio rather than video.
const ASF_AUDIO_MEDIA_GUID: [u8; 16] = [
    0x40, 0x9E, 0x69, 0xF8, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B,
];

/// Size of the ASF Header Object: GUID, size, object count, two reserved bytes.
const ASF_HEADER_LEN: usize = 30;

/// GUID plus size, in front of every ASF object.
const ASF_OBJECT_HEADER_LEN: usize = 24;

/// Where the `WAVEFORMATEX` starts inside a Stream Properties Object: past the
/// object header, both type GUIDs, the time offset, two data lengths, the
/// flags, and the reserved word.
const ASF_FORMAT_TAG_OFFSET: usize = 78;

/// The WAVE format tag meaning "the real tag is in the sub-format GUID".
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Fixed part of an Ogg page header, ending with the segment count.
const OGG_PAGE_HEADER_LEN: usize = 27;

/// Bytes of a first packet needed to recognise any of the codec signatures.
const OGG_PACKET_PEEK: usize = 8;

/// How many Ogg pages to look through before giving up on finding audio. More
/// than one, because a stream can multiplex video ahead of the audio.
const OGG_PAGE_SCAN: usize = 16;

/// How much of a Matroska file to search for its `Tracks` element.
const MATROSKA_WINDOW: usize = 256 * 1024;

/// EBML element IDs on the way to a track's codec.
const EBML_SEGMENT: u32 = 0x1853_8067;
const EBML_TRACKS: u32 = 0x1654_AE6B;
const EBML_TRACK_ENTRY: u32 = 0xAE;
const EBML_TRACK_TYPE: u32 = 0x83;
const EBML_CODEC_ID: u32 = 0x86;

/// `TrackType` value for an audio track.
const MATROSKA_TRACK_AUDIO: u8 = 2;

/// MP4 boxes that hold nothing but other boxes on the way to a sample
/// description.
const MP4_CONTAINER_BOXES: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

/// How deep to recurse into MP4 boxes before calling the file malformed.
const MP4_MAX_DEPTH: u32 = 8;

/// Cap on chunks, boxes, or objects walked before a file is called malformed,
/// so a bad size field cannot spin this forever.
const MAX_CHUNKS: usize = 4096;

/// Cap on how much of any one chunk is read while identifying it. Only headers
/// are ever wanted, never the audio behind them.
const MAX_CHUNK_READ: usize = 4 * 1024;

/// How a file is wrapped, independent of what is coded inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Container {
    /// RIFF/WAVE.
    Wav,
    /// AIFF or AIFF-C.
    Aiff,
    /// Apple Core Audio Format.
    Caf,
    /// Ogg — Vorbis, Opus, FLAC, or Speex inside.
    Ogg,
    /// FLAC's own native stream container.
    Flac,
    /// ISO base media / MP4 / QuickTime: `.mp4`, `.m4a`, `.mov`.
    Mp4,
    /// Matroska or WebM.
    Matroska,
    /// Advanced Systems Format: `.wma`, `.wmv`.
    Asf,
    /// Bare AAC in ADTS framing.
    Adts,
    /// Bare MPEG audio frames, with or without an ID3 tag.
    Mpeg,
    /// Bare AC-3 / E-AC-3 sync frames.
    Ac3,
    /// Bare DTS sync frames.
    Dts,
    /// AMR storage format, narrow or wide band.
    Amr,
}

/// How the audio samples themselves are coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// Uncompressed samples, any width, integer or float.
    Pcm,
    Mp3,
    /// AAC Low Complexity — the plain, most common profile.
    AacLc,
    /// HE-AAC v1: AAC LC plus Spectral Band Replication.
    AacHe,
    /// HE-AAC v2: HE-AAC plus Parametric Stereo.
    AacHeV2,
    Opus,
    Vorbis,
    Flac,
    /// Apple Lossless.
    Alac,
    /// Dolby Digital.
    Ac3,
    /// Dolby Digital Plus.
    EAc3,
    Dts,
    /// DTS-HD Master Audio, the lossless extension.
    DtsHdMa,
    TrueHd,
    Wma,
    /// AMR narrowband, 8 kHz.
    AmrNb,
    /// AMR wideband, 16 kHz.
    AmrWb,
}

/// Interleaved samples in whatever type the file is coded in.
///
/// Which one that is is a fact about the file, not a choice: a 16-bit WAV is
/// `I16`, a 24-bit FLAC is `I24`, an MP3 is `F32`. So it cannot be a type
/// parameter — nothing is known until the header has been read — and this enum
/// carries the answer instead.
///
/// Match on it to get at the samples without converting anything, or call
/// [`to_vec`](Self::to_vec) when a particular type is needed.
#[derive(Clone, Debug, PartialEq)]
pub enum Samples {
    U8(Vec<u8>),
    I8(Vec<i8>),
    U16(Vec<u16>),
    I16(Vec<i16>),
    U24(Vec<U24>),
    I24(Vec<I24>),
    U32(Vec<u32>),
    I32(Vec<i32>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

/// Applies `$body` to whichever vector is inside, so the ten variants do not
/// have to be written out for every operation that treats them alike.
macro_rules! with_samples {
    ($samples:expr, |$vec:ident| $body:expr) => {
        match $samples {
            Samples::U8($vec) => $body,
            Samples::I8($vec) => $body,
            Samples::U16($vec) => $body,
            Samples::I16($vec) => $body,
            Samples::U24($vec) => $body,
            Samples::I24($vec) => $body,
            Samples::U32($vec) => $body,
            Samples::I32($vec) => $body,
            Samples::F32($vec) => $body,
            Samples::F64($vec) => $body,
        }
    };
}

impl Samples {
    /// The format these samples are in — the file's, not the caller's.
    pub fn format(&self) -> SampleFormat {
        match self {
            Samples::U8(_) => SampleFormat::U8,
            Samples::I8(_) => SampleFormat::I8,
            Samples::U16(_) => SampleFormat::U16,
            Samples::I16(_) => SampleFormat::I16,
            Samples::U24(_) => SampleFormat::U24,
            Samples::I24(_) => SampleFormat::I24,
            Samples::U32(_) => SampleFormat::U32,
            Samples::I32(_) => SampleFormat::I32,
            Samples::F32(_) => SampleFormat::F32,
            Samples::F64(_) => SampleFormat::F64,
        }
    }

    /// How many samples there are, across all channels.
    pub fn len(&self) -> usize {
        with_samples!(self, |vec| vec.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Converts to `S`, for feeding something statically typed — an
    /// [`OutputClass<S>`](crate::output::OutputClass), most often.
    ///
    /// Free of charge only when nothing has to change. The conversion goes
    /// through `f32`, which is the one bridge [`SampleType`] guarantees between
    /// any two sample types: exact for everything up to 24 bits, and lossy in
    /// the bottom bits for `i32`, `u32`, and `f64`. Match on the enum instead
    /// where the file's own type is what you want.
    pub fn to_vec<S: SampleType>(&self) -> Vec<S> {
        with_samples!(self, |vec| vec
            .iter()
            .map(|sample| S::from_f32(SampleType::to_f32(*sample)))
            .collect())
    }
}

/// Decoded audio, interleaved, in the file's own sample type.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    /// Interleaved samples: frame 0 channel 0, frame 0 channel 1, frame 1 …
    pub samples: Samples,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Decoded {
    /// The format the file's samples are in.
    pub fn sample_format(&self) -> SampleFormat {
        self.samples.format()
    }

    /// Frames, as opposed to samples: one frame is one sample per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// How long the audio runs for, in seconds.
    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }
}

/// A decoder handing back audio a block at a time.
///
/// [`decode`] holds an entire file in memory at once, which is fine for a drum
/// hit and ruinous for an hour-long recording — decoded audio is far larger
/// than the file it came from, so a 60 MB MP3 lands as roughly 600 MB of `f32`.
/// A stream decodes only what is asked for, so the cost is the caller's buffer
/// rather than the whole file.
///
/// Implementors decode lazily: the work happens inside [`read`](Self::read),
/// not when the stream is opened.
pub trait AudioStream<S: SampleType>: Send {
    /// Sample rate of what [`read`](Self::read) produces.
    fn sample_rate(&self) -> u32;

    /// Channel count of what [`read`](Self::read) produces.
    fn channels(&self) -> u16;

    /// The sample format `S` is laid out as — the caller's choice of buffer.
    fn sample_format(&self) -> SampleFormat {
        S::format()
    }

    /// The format the file itself is coded in.
    ///
    /// Reading converts into `S`, so this is the only way to see what the file
    /// actually held: pick `S` to match it and nothing is converted at all.
    /// [`decode`] uses this to hand back the file's own type.
    fn source_format(&self) -> SampleFormat;

    /// Total frames, when the container declares it.
    ///
    /// `None` where it does not, or where the declared value cannot be trusted
    /// — a VBR MP3 with no Xing header, a truncated download. Treat it as a
    /// hint for sizing a buffer, never as a guarantee of what will arrive.
    fn total_frames(&self) -> Option<u64> {
        None
    }

    /// Fills `out` with interleaved samples and returns how many were written.
    ///
    /// Only `Ok(0)` means the end: a short read is ordinary, since a decoder
    /// hands over whatever the frame it just decoded held rather than padding
    /// to fit. Give it a buffer sized to a whole number of frames — a partial
    /// frame at the end of a block would leave the channels interleaved out of
    /// step.
    fn read(&mut self, out: &mut [S]) -> Result<usize, Error>;
}

/// Samples per block when draining a stream into memory.
const DRAIN_BLOCK: usize = 8192;

/// Cap on how much a declared frame count is trusted to pre-allocate: a header
/// claiming billions of frames must not be able to ask for that much memory
/// before a single sample has been decoded.
const MAX_DRAIN_RESERVE: usize = 1 << 24;

/// Reads a stream to its end, collecting it into one buffer of `S`.
///
/// Takes the stream by reference so the caller keeps it afterwards — the rate
/// and channel count live there, and a stream that has been consumed cannot be
/// asked.
///
/// This is the streaming counterpart to [`decode`], which reads in the file's
/// own type instead of a chosen one.
pub fn drain<S: SampleType>(stream: &mut dyn AudioStream<S>) -> Result<Vec<S>, Error> {
    let channels = stream.channels();

    let mut samples = Vec::new();
    if let Some(frames) = stream.total_frames() {
        let estimate = (frames as usize).saturating_mul(channels.max(1) as usize);
        samples.reserve(estimate.min(MAX_DRAIN_RESERVE));
    }

    let mut block = vec![S::SILENCE; DRAIN_BLOCK];
    loop {
        let read = stream.read(&mut block)?;
        if read == 0 {
            break;
        }
        samples.extend_from_slice(&block[..read]);
    }

    Ok(samples)
}

/// Identifies how the audio in `path` is encoded, by reading the file.
///
/// Names are ignored throughout. Extensions lie: `.ogg` says nothing about
/// whether the audio inside is Vorbis or Opus, `.m4a` covers both AAC and ALAC,
/// downloads arrive misnamed, and anything can be renamed by hand.
///
/// Fails with [`ErrorKind::InvalidInput`] if the file matches no known
/// container, and [`ErrorKind::UnsupportedOperation`] where the container is
/// recognised but reading the encoding out of it is not written yet.
pub fn find_type(path: &Path) -> Result<Encoding, Error> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let container = read_container(&mut file, path)?;

    encoding_in(container, &mut file, path)
}

/// Identifies how `path` is wrapped, without looking inside for the encoding.
pub fn find_container(path: &Path) -> Result<Container, Error> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;

    read_container(&mut file, path)
}

/// Decodes the audio in `path` to interleaved samples.
///
/// Identifies the encoding first, then hands off to the decoder for it.
pub fn decode(path: &Path) -> Result<Decoded, Error> {
    let encoding = find_type(path)?;

    decode_as(path, encoding)
}

/// Opens the audio in `path` for reading a block at a time.
///
/// The streaming counterpart to [`decode`]: same identification, same decoders,
/// but nothing is decoded until [`AudioStream::read`] asks for it. Prefer this
/// for anything long, anything played back progressively, or anywhere the
/// decoded size is not known to be small.
pub fn stream<S: SampleType>(path: &Path) -> Result<Box<dyn AudioStream<S>>, Error> {
    let encoding = find_type(path)?;

    stream_as(path, encoding)
}

/// Opens `path` as `encoding` for streaming, skipping identification.
pub fn stream_as<S: SampleType>(
    path: &Path,
    encoding: Encoding,
) -> Result<Box<dyn AudioStream<S>>, Error> {
    match encoding {
        Encoding::Pcm => audio::read_pcm(path),
        Encoding::Mp3 => audio::read_mp3(path),
        Encoding::AacLc | Encoding::AacHe | Encoding::AacHeV2 => audio::read_aac(path, encoding),
        Encoding::Opus => audio::read_opus(path),
        Encoding::Vorbis => audio::read_vorbis(path),
        Encoding::Flac => audio::read_flac(path),
        Encoding::Alac => audio::read_alac(path),
        Encoding::Ac3 => audio::read_ac3(path),
        Encoding::EAc3 => audio::read_eac3(path),
        Encoding::Dts => audio::read_dts(path),
        Encoding::DtsHdMa => audio::read_dts_hd_ma(path),
        Encoding::TrueHd => audio::read_truehd(path),
        Encoding::Wma => audio::read_wma(path),
        Encoding::AmrNb => audio::read_amr_nb(path),
        Encoding::AmrWb => audio::read_amr_wb(path),
    }
}

/// Decodes `path` as `encoding`, skipping identification.
///
/// Use when the encoding is already known — from a container that was parsed
/// for other reasons, or from a caller that knows what it fetched.
pub fn decode_as(path: &Path, encoding: Encoding) -> Result<Decoded, Error> {
    match encoding {
        Encoding::Pcm => audio::decode_pcm(path),
        Encoding::Mp3 => audio::decode_mp3(path),
        // One decoder family: HE-AAC is AAC LC with SBR, HE-AAC v2 adds PS on
        // top, so the profile changes what gets reconstructed, not which
        // decoder runs.
        Encoding::AacLc | Encoding::AacHe | Encoding::AacHeV2 => audio::decode_aac(path, encoding),
        Encoding::Opus => audio::decode_opus(path),
        Encoding::Vorbis => audio::decode_vorbis(path),
        Encoding::Flac => audio::decode_flac(path),
        Encoding::Alac => audio::decode_alac(path),
        Encoding::Ac3 => audio::decode_ac3(path),
        Encoding::EAc3 => audio::decode_eac3(path),
        Encoding::Dts => audio::decode_dts(path),
        Encoding::DtsHdMa => audio::decode_dts_hd_ma(path),
        Encoding::TrueHd => audio::decode_truehd(path),
        Encoding::Wma => audio::decode_wma(path),
        Encoding::AmrNb => audio::decode_amr_nb(path),
        Encoding::AmrWb => audio::decode_amr_wb(path),
    }
}

// ---------------------------------------------------------------------------
// Container identification
// ---------------------------------------------------------------------------

/// Reads the front of `file` and matches it against every container magic.
fn read_container(file: &mut File, path: &Path) -> Result<Container, Error> {
    let mut buffer = [0u8; HEADER_LEN];
    let read = read_up_to(file, &mut buffer).map_err(|error| io_error(path, error))?;
    let header = &buffer[..read];

    if let Some(container) = sniff_container(header) {
        return Ok(container);
    }

    // MPEG audio last: it is the only one with no magic number, matched on a
    // pattern loose enough to turn up by chance in another format's data.
    if is_mpeg_audio(file, header).map_err(|error| io_error(path, error))? {
        return Ok(Container::Mpeg);
    }

    Err(Error::with_message(
        ErrorKind::InvalidInput,
        format!("unrecognised audio format: {}", path.display()),
    ))
}

/// Matches the containers that announce themselves with a magic number.
fn sniff_container(header: &[u8]) -> Option<Container> {
    // RIFF is a generic container — AVI is RIFF too — so the form type at byte
    // 8 is what actually makes it a WAV. Same shape for AIFF's FORM.
    if tag_at(header, 0, b"RIFF") && tag_at(header, 8, b"WAVE") {
        return Some(Container::Wav);
    }
    if tag_at(header, 0, b"FORM") && (tag_at(header, 8, b"AIFF") || tag_at(header, 8, b"AIFC")) {
        return Some(Container::Aiff);
    }
    if tag_at(header, 0, b"caff") {
        return Some(Container::Caf);
    }
    if tag_at(header, 0, b"fLaC") {
        return Some(Container::Flac);
    }
    if tag_at(header, 0, b"OggS") {
        return Some(Container::Ogg);
    }
    // ISO base media: a size field first, then the brand at byte 4. QuickTime
    // `.mov` files are the same layout.
    if tag_at(header, 4, b"ftyp") {
        return Some(Container::Mp4);
    }
    // EBML header — Matroska and WebM share it, and which one it is only shows
    // up in the DocType further in.
    if tag_at(header, 0, &[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Container::Matroska);
    }
    if tag_at(header, 0, &ASF_HEADER_GUID) {
        return Some(Container::Asf);
    }
    if tag_at(header, 0, b"#!AMR") {
        return Some(Container::Amr);
    }
    // AC-3 and E-AC-3 share a syncword; which one it is comes from the
    // bitstream ID a few bytes later.
    if tag_at(header, 0, &[0x0B, 0x77]) {
        return Some(Container::Ac3);
    }
    if is_dts_sync(header) {
        return Some(Container::Dts);
    }
    // ADTS before MPEG audio: both start 0xFF Ex, and the layer bits are what
    // tell them apart — 00 is reserved in MPEG audio and required by ADTS.
    if is_adts_header(header) {
        return Some(Container::Adts);
    }

    None
}

/// Whether this looks like bare MPEG audio (an MP3, in practice).
///
/// It has no container and no magic of its own: it is a run of MPEG frames,
/// recognised by finding a plausible frame header. The sync word is only eleven
/// set bits, so the rest of the header's fields get validated too rather than
/// trusting the sync alone.
///
/// Only the very start of the audio is considered, so a file with leading junk
/// ahead of the first frame — and no ID3 tag to explain it — is not recognised.
fn is_mpeg_audio(file: &mut File, header: &[u8]) -> io::Result<bool> {
    // An ID3v2 tag sits in front of the audio, so the first frame is past it.
    if tag_at(header, 0, b"ID3") && header.len() >= 10 {
        file.seek(SeekFrom::Start(id3_len(header) as u64))?;

        let mut window = vec![0u8; MPEG_SYNC_SEARCH];
        let read = read_up_to(file, &mut window)?;
        return Ok(window[..read].windows(4).any(is_frame_header));
    }

    Ok(is_frame_header(header))
}

// ---------------------------------------------------------------------------
// Encoding identification, per container
// ---------------------------------------------------------------------------

/// Finds which encoding `container` is carrying.
fn encoding_in(container: Container, file: &mut File, path: &Path) -> Result<Encoding, Error> {
    match container {
        // Containers that carry exactly one thing: the magic already answered it.
        Container::Flac => Ok(Encoding::Flac),
        Container::Mpeg => Ok(Encoding::Mp3),

        // Resolvable from bytes already at hand.
        Container::Amr => encoding_in_amr(file, path),
        Container::Ac3 => encoding_in_ac3(file, path),
        Container::Dts => encoding_in_dts(file, path),
        Container::Adts => encoding_in_adts(file, path),

        // Containers needing a real header parse.
        Container::Wav => encoding_in_wav(file, path),
        Container::Aiff => encoding_in_aiff(file, path),
        Container::Caf => encoding_in_caf(file, path),
        Container::Ogg => encoding_in_ogg(file, path),
        Container::Mp4 => encoding_in_mp4(file, path),
        Container::Matroska => encoding_in_matroska(file, path),
        Container::Asf => encoding_in_asf(file, path),
    }
}

/// AMR says which band it is in its own magic string.
fn encoding_in_amr(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let header = header_from_start(file, path, 9)?;

    // Single-channel forms. Multi-channel AMR ("#!AMR_MC1.0") exists and is not
    // handled here.
    if tag_at(&header, 0, b"#!AMR-WB\n") {
        Ok(Encoding::AmrWb)
    } else if tag_at(&header, 0, b"#!AMR\n") {
        Ok(Encoding::AmrNb)
    } else {
        Err(Error::with_message(
            ErrorKind::InvalidInput,
            format!("unrecognised AMR variant: {}", path.display()),
        ))
    }
}

/// AC-3 and E-AC-3 share the `0x0B77` syncword; the bitstream ID separates them.
///
/// `bsid` sits at the same bit position in both — five bits at the top of byte
/// 5 — precisely so a decoder can tell which it is before parsing anything
/// else. 16 means E-AC-3; the legacy values (8 and below) mean AC-3.
fn encoding_in_ac3(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let header = header_from_start(file, path, 6)?;

    if header.len() < 6 {
        return Err(Error::with_message(
            ErrorKind::InvalidInput,
            format!("truncated AC-3 sync frame: {}", path.display()),
        ));
    }

    match header[5] >> 3 {
        16 => Ok(Encoding::EAc3),
        _ => Ok(Encoding::Ac3),
    }
}

/// TODO: separate plain DTS from its lossless extension.
///
/// The core substream is what the syncword found, and every DTS file has one.
/// DTS-HD MA is an *extension* substream (`0x64582025`) carrying a lossless
/// asset alongside that core, so telling them apart means walking to the
/// extension and reading its asset descriptor — not something the first few
/// bytes answer.
fn encoding_in_dts(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let _ = (file, path);

    // Reporting the core is right for a core-only file and wrong for DTS-HD MA,
    // which will decode as lossy DTS until the extension parse exists.
    Ok(Encoding::Dts)
}

/// ADTS names its profile in the frame header.
///
/// Only the base profile is visible here. HE-AAC v1 and v2 are AAC LC plus SBR
/// and PS, which are signalled *inside* the audio payload ("implicit
/// signalling"), so an HE-AAC stream still reports LC until the payload is
/// examined.
fn encoding_in_adts(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let header = header_from_start(file, path, 3)?;

    if header.len() < 3 {
        return Err(Error::with_message(
            ErrorKind::InvalidInput,
            format!("truncated ADTS frame: {}", path.display()),
        ));
    }

    // Two bits, holding audioObjectType - 1: 00 Main, 01 LC, 10 SSR, 11 LTP.
    match header[2] >> 6 {
        0b01 => Ok(Encoding::AacLc),
        profile => Err(Error::with_message(
            ErrorKind::UnsupportedOperation,
            format!("unsupported AAC profile {profile} in {}", path.display()),
        )),
    }
}

/// Reads the format tag out of the `fmt ` chunk.
///
/// Chunks are not in a fixed order, so this walks them rather than assuming
/// `fmt ` comes first.
fn encoding_in_wav(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let fmt = riff_chunk(file, path, b"fmt ", u32_le_at)?
        .ok_or_else(|| malformed(path, "WAV with no `fmt ` chunk"))?;

    let mut tag = u16_le_at(&fmt, 0).ok_or_else(|| malformed(path, "WAV `fmt ` chunk"))?;

    // WAVE_FORMAT_EXTENSIBLE does not name the codec itself: the real tag is
    // the first field of the sub-format GUID, 24 bytes in.
    if tag == WAVE_FORMAT_EXTENSIBLE {
        tag = u16_le_at(&fmt, 24)
            .ok_or_else(|| malformed(path, "WAV extensible `fmt ` chunk"))?;
    }

    wave_format_tag(tag).ok_or_else(|| unsupported(path, format!("WAVE format tag {tag:#06x}")))
}

/// Reads the compression type out of the `COMM` chunk.
///
/// Plain AIFF is always PCM and says nothing further. AIFF-C adds a
/// four-character compression type, 18 bytes into `COMM`, past the channel
/// count, frame count, sample size, and the 80-bit extended-precision rate.
fn encoding_in_aiff(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let header = header_from_start(file, path, 12)?;
    let compressed = tag_at(&header, 8, b"AIFC");

    if !compressed {
        return Ok(Encoding::Pcm);
    }

    let comm = riff_chunk(file, path, b"COMM", u32_be_at)?
        .ok_or_else(|| malformed(path, "AIFF with no `COMM` chunk"))?;
    let compression = fourcc_at(&comm, 18).ok_or_else(|| malformed(path, "AIFF-C `COMM` chunk"))?;

    aiff_compression(&compression)
        .ok_or_else(|| unsupported(path, format!("AIFF-C compression {}", fourcc_name(&compression))))
}

/// Reads the format ID out of the `desc` chunk.
fn encoding_in_caf(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let desc = caf_chunk(file, path, b"desc")?
        .ok_or_else(|| malformed(path, "CAF with no `desc` chunk"))?;

    // Past the 64-bit sample rate.
    let format = fourcc_at(&desc, 8).ok_or_else(|| malformed(path, "CAF `desc` chunk"))?;

    caf_format(&format)
        .ok_or_else(|| unsupported(path, format!("CAF format {}", fourcc_name(&format))))
}

/// Reads the codec out of the first packet of a page.
///
/// Each codec identifies itself at the start of its own first packet. Reaching
/// that packet means stepping over the 27-byte page header *and* its
/// variable-length segment table, so the offset is not fixed.
///
/// Several pages are checked rather than only the first: an Ogg stream can
/// multiplex video and audio, and the audio need not come first.
fn encoding_in_ogg(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let mut offset = 0u64;

    for _ in 0..OGG_PAGE_SCAN {
        let header = read_at(file, path, offset, OGG_PAGE_HEADER_LEN)?;
        if header.len() < OGG_PAGE_HEADER_LEN || !tag_at(&header, 0, b"OggS") {
            break;
        }

        // The last header byte counts the segments; the table that follows is
        // one byte per segment, and those bytes sum to the page's body length.
        let segments = header[OGG_PAGE_HEADER_LEN - 1] as usize;
        let table = read_at(file, path, offset + OGG_PAGE_HEADER_LEN as u64, segments)?;
        if table.len() < segments {
            break;
        }

        let packet_at = offset + OGG_PAGE_HEADER_LEN as u64 + segments as u64;
        let packet = read_at(file, path, packet_at, OGG_PACKET_PEEK)?;
        if let Some(encoding) = ogg_codec(&packet) {
            return Ok(encoding);
        }

        let body: usize = table.iter().map(|&length| length as usize).sum();
        offset = packet_at + body as u64;
    }

    Err(unsupported(path, "Ogg codec".to_string()))
}

/// Walks the box tree to a sample description and reads the codec from it.
///
/// `moov` → `trak` → `mdia` → `minf` → `stbl` → `stsd`, then the sample entry's
/// four-character code.
///
/// Video tracks are skipped without needing to read their handler: their sample
/// entries (`avc1`, `hvc1`, and the rest) simply do not map to an audio
/// encoding, so the walk moves on. A file with several *audio* tracks resolves
/// to the first one — real track selection belongs with the demuxer, not here.
fn encoding_in_mp4(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let end = file
        .metadata()
        .map_err(|error| io_error(path, error))?
        .len();

    // `moov` is sometimes at the end of the file rather than the front, so this
    // walks the whole box tree rather than assuming where it sits.
    mp4_encoding(file, path, 0, end, 0)?
        .ok_or_else(|| unsupported(path, "MP4 audio track".to_string()))
}

/// Reads the audio track's `CodecID` out of the EBML tree.
///
/// Only the front of the file is searched: `Tracks` is written before the
/// clusters in any file a player can stream, so if it is not in that window it
/// is not somewhere worth scanning a whole video file for.
fn encoding_in_matroska(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    let data = read_at(file, path, 0, MATROSKA_WINDOW)?;

    matroska_encoding(&data).ok_or_else(|| unsupported(path, "Matroska audio track".to_string()))
}

/// Reads the format tag out of the audio stream's properties object.
///
/// ASF describes each stream in a GUID-keyed object list. The audio stream's
/// properties carry a `WAVEFORMATEX` — the same tag space WAV uses, so the same
/// mapping applies.
fn encoding_in_asf(file: &mut File, path: &Path) -> Result<Encoding, Error> {
    // Past the header object's own GUID, size, object count, and reserved pair.
    let mut offset = ASF_HEADER_LEN as u64;

    for _ in 0..MAX_CHUNKS {
        let header = read_at(file, path, offset, ASF_OBJECT_HEADER_LEN)?;
        if header.len() < ASF_OBJECT_HEADER_LEN {
            break;
        }

        let size = u64_le_at(&header, 16).ok_or_else(|| malformed(path, "ASF object header"))?;
        if size < ASF_OBJECT_HEADER_LEN as u64 {
            break;
        }

        if header[..16] == ASF_STREAM_PROPERTIES_GUID {
            let object = read_at(file, path, offset, size.min(MAX_CHUNK_READ as u64) as usize)?;

            // Stream type sits right after the object's own GUID and size, and
            // says whether this stream is the audio one.
            if object.len() >= ASF_FORMAT_TAG_OFFSET + 2
                && object[24..40] == ASF_AUDIO_MEDIA_GUID
            {
                let tag = u16_le_at(&object, ASF_FORMAT_TAG_OFFSET)
                    .ok_or_else(|| malformed(path, "ASF stream properties"))?;

                return wave_format_tag(tag)
                    .ok_or_else(|| unsupported(path, format!("WAVE format tag {tag:#06x}")));
            }
        }

        offset += size;
    }

    Err(unsupported(path, "ASF audio stream".to_string()))
}

// ---------------------------------------------------------------------------
// Container walking
// ---------------------------------------------------------------------------

/// Finds a top-level chunk in a RIFF-shaped file and returns its payload.
///
/// Covers both RIFF and AIFF: they are the same layout — a 12-byte file header,
/// then a run of four-character-code chunks each with its own size — differing
/// only in whether that size is little- or big-endian, which `size_at` supplies.
///
/// Only the head of a chunk is returned. This is identification, so the audio
/// itself is never wanted.
fn riff_chunk(
    file: &mut File,
    path: &Path,
    id: &[u8; 4],
    size_at: fn(&[u8], usize) -> Option<u32>,
) -> Result<Option<Vec<u8>>, Error> {
    let mut offset = 12u64;

    for _ in 0..MAX_CHUNKS {
        let header = read_at(file, path, offset, 8)?;
        if header.len() < 8 {
            return Ok(None);
        }

        let size = size_at(&header, 4).ok_or_else(|| malformed(path, "RIFF chunk header"))? as u64;

        if &header[..4] == id {
            let want = size.min(MAX_CHUNK_READ as u64) as usize;
            return Ok(Some(read_at(file, path, offset + 8, want)?));
        }

        // Chunks are word-aligned: an odd size is followed by a pad byte that
        // the size does not count.
        offset += 8 + size + (size & 1);
    }

    Ok(None)
}

/// Finds a CAF chunk and returns its payload.
///
/// Same idea as [`riff_chunk`], but CAF's file header is 8 bytes and its chunk
/// sizes are signed 64-bit big-endian — negative meaning "runs to the end of
/// the file", which only the final chunk may use.
fn caf_chunk(file: &mut File, path: &Path, id: &[u8; 4]) -> Result<Option<Vec<u8>>, Error> {
    let mut offset = 8u64;

    for _ in 0..MAX_CHUNKS {
        let header = read_at(file, path, offset, 12)?;
        if header.len() < 12 {
            return Ok(None);
        }

        let size = i64_be_at(&header, 4).ok_or_else(|| malformed(path, "CAF chunk header"))?;

        if &header[..4] == id {
            let want = if size < 0 {
                MAX_CHUNK_READ
            } else {
                (size as u64).min(MAX_CHUNK_READ as u64) as usize
            };
            return Ok(Some(read_at(file, path, offset + 12, want)?));
        }

        if size < 0 {
            return Ok(None);
        }
        offset += 12 + size as u64;
    }

    Ok(None)
}

/// Walks MP4 boxes from `start` to `end`, descending into the containers on the
/// way to a sample description.
fn mp4_encoding(
    file: &mut File,
    path: &Path,
    start: u64,
    end: u64,
    depth: u32,
) -> Result<Option<Encoding>, Error> {
    if depth > MP4_MAX_DEPTH {
        return Ok(None);
    }

    let mut offset = start;

    for _ in 0..MAX_CHUNKS {
        if offset + 8 > end {
            break;
        }

        let header = read_at(file, path, offset, 16)?;
        if header.len() < 8 {
            break;
        }

        let declared =
            u32_be_at(&header, 0).ok_or_else(|| malformed(path, "MP4 box header"))? as u64;
        let kind = fourcc_at(&header, 4).ok_or_else(|| malformed(path, "MP4 box header"))?;

        // Size 1 means the real 64-bit size follows the type; size 0 means the
        // box runs to the end of the file.
        let (size, body) = match declared {
            1 => (
                u64_be_at(&header, 8).ok_or_else(|| malformed(path, "MP4 large box header"))?,
                offset + 16,
            ),
            0 => (end - offset, offset + 8),
            _ => (declared, offset + 8),
        };

        // A size that does not even cover its own header would walk backwards.
        if size < body - offset {
            break;
        }
        let body_end = (offset + size).min(end);

        if &kind == b"stsd" {
            if let Some(encoding) = stsd_encoding(file, path, body, body_end)? {
                return Ok(Some(encoding));
            }
        } else if MP4_CONTAINER_BOXES.iter().any(|container| *container == &kind) {
            if let Some(encoding) = mp4_encoding(file, path, body, body_end, depth + 1)? {
                return Ok(Some(encoding));
            }
        }

        offset += size;
    }

    Ok(None)
}

/// Reads the codec out of a sample description's entries.
///
/// Entries whose format is not audio — a video track's `avc1`, say — map to
/// nothing and are stepped over, which is why no handler lookup is needed to
/// avoid video tracks.
fn stsd_encoding(
    file: &mut File,
    path: &Path,
    start: u64,
    end: u64,
) -> Result<Option<Encoding>, Error> {
    // Past the version, flags, and entry count.
    let mut offset = start + 8;

    for _ in 0..MAX_CHUNKS {
        if offset + 8 > end {
            break;
        }

        let header = read_at(file, path, offset, 8)?;
        if header.len() < 8 {
            break;
        }

        let size = u32_be_at(&header, 0).ok_or_else(|| malformed(path, "MP4 sample entry"))? as u64;
        let format = fourcc_at(&header, 4).ok_or_else(|| malformed(path, "MP4 sample entry"))?;
        if size < 8 {
            break;
        }

        if &format == b"mp4a" {
            // `mp4a` names a family, not a codec: the object type in the nested
            // `esds` says which member it is, and the AudioSpecificConfig below
            // that says which AAC profile.
            let length = size.min(MAX_CHUNK_READ as u64) as usize;
            let entry = read_at(file, path, offset, length)?;

            // An `esds` that will not parse still leaves this as AAC, which is
            // what `mp4a` means in all but a handful of files.
            return Ok(Some(esds_encoding(&entry).unwrap_or(Encoding::AacLc)));
        }

        if let Some(encoding) = mp4_sample_format(&format) {
            return Ok(Some(encoding));
        }

        offset += size;
    }

    Ok(None)
}

/// Reads the codec out of an `mp4a` entry's `esds` box.
///
/// The box nests three MPEG-4 descriptors: an `ES_Descriptor` holding a
/// `DecoderConfigDescriptor` holding the codec's own `DecoderSpecificInfo`.
fn esds_encoding(entry: &[u8]) -> Option<Encoding> {
    // Past the box's own type and its version/flags word.
    let start = find_bytes(entry, b"esds")? + 8;
    let descriptors = entry.get(start..)?;

    let stream = descriptor(descriptors, 0x03)?;

    // ES_ID and a flags byte, then whichever optional fields the flags switch
    // on — each has to be stepped over to reach the config descriptor.
    let flags = *stream.get(2)?;
    let mut offset = 3;
    if flags & 0x80 != 0 {
        offset += 2; // depends-on ES_ID
    }
    if flags & 0x40 != 0 {
        offset += 1 + *stream.get(offset)? as usize; // length-prefixed URL
    }
    if flags & 0x20 != 0 {
        offset += 2; // OCR ES_ID
    }

    let config = descriptor(stream.get(offset..)?, 0x04)?;

    match *config.first()? {
        // MPEG-4 audio: the profile is in the AudioSpecificConfig below this.
        0x40 => {
            // Past the stream type, buffer size, and the two bitrates.
            let specific = descriptor(config.get(13..)?, 0x05)?;
            audio_object_type(specific)
        }
        0x66 | 0x67 | 0x68 => Some(Encoding::AacLc), // MPEG-2 AAC Main/LC/SSR
        0x69 | 0x6B => Some(Encoding::Mp3),          // MPEG-2 and MPEG-1 audio
        0xA5 => Some(Encoding::Ac3),
        0xA6 => Some(Encoding::EAc3),
        0xA9 => Some(Encoding::Dts),
        0xDD => Some(Encoding::Vorbis),
        _ => None,
    }
}

/// Steps into an MPEG-4 descriptor of `tag`, returning its payload.
///
/// Descriptor lengths are "expandable": seven bits per byte, with the top bit
/// set on every byte but the last, so the length is one to four bytes long.
fn descriptor(data: &[u8], tag: u8) -> Option<&[u8]> {
    if *data.first()? != tag {
        return None;
    }

    let mut length = 0usize;
    let mut offset = 1;

    for _ in 0..4 {
        let byte = *data.get(offset)?;
        offset += 1;
        length = (length << 7) | (byte & 0x7F) as usize;

        if byte & 0x80 == 0 {
            break;
        }
    }

    data.get(offset..(offset + length).min(data.len()))
}

/// Reads the audio object type from the front of an `AudioSpecificConfig`.
///
/// Five bits, or the escape value 31 followed by six more that are added to 32.
/// Explicit signalling puts SBR or PS here directly; a stream that only signals
/// them implicitly, inside the payload, still reads as plain LC.
fn audio_object_type(config: &[u8]) -> Option<Encoding> {
    let first = *config.first()?;
    let mut object_type = (first >> 3) as u16;

    if object_type == 31 {
        let second = *config.get(1)?;
        object_type = 32 + (((first & 0x07) as u16) << 3 | (second >> 5) as u16);
    }

    match object_type {
        2 => Some(Encoding::AacLc),
        5 => Some(Encoding::AacHe),
        29 => Some(Encoding::AacHeV2),
        _ => None,
    }
}

/// Finds the audio track's codec in an EBML tree.
fn matroska_encoding(data: &[u8]) -> Option<Encoding> {
    let segment = ebml_child(data, EBML_SEGMENT)?;
    let tracks = ebml_child(segment, EBML_TRACKS)?;

    let mut offset = 0;

    while offset < tracks.len() {
        let (id, payload, next) = ebml_element(tracks, offset)?;

        if id == EBML_TRACK_ENTRY {
            if let Some(encoding) = track_entry_encoding(payload) {
                return Some(encoding);
            }
        }

        if next <= offset {
            break;
        }
        offset = next;
    }

    None
}

/// The codec of a `TrackEntry`, if it is an audio track at all.
fn track_entry_encoding(entry: &[u8]) -> Option<Encoding> {
    if *ebml_child(entry, EBML_TRACK_TYPE)?.first()? != MATROSKA_TRACK_AUDIO {
        return None;
    }

    matroska_codec(ebml_child(entry, EBML_CODEC_ID)?)
}

/// The payload of the first child element carrying `id`.
fn ebml_child(data: &[u8], id: u32) -> Option<&[u8]> {
    let mut offset = 0;

    while offset < data.len() {
        let (found, payload, next) = ebml_element(data, offset)?;

        if found == id {
            return Some(payload);
        }
        if next <= offset {
            return None;
        }
        offset = next;
    }

    None
}

/// Reads one EBML element at `offset`: its ID, its payload, and where the next
/// one starts.
fn ebml_element(data: &[u8], offset: usize) -> Option<(u32, &[u8], usize)> {
    let (id, after_id) = ebml_id(data, offset)?;
    let (size, after_size) = ebml_size(data, after_id)?;

    // An unknown size runs to the end of what is available, which is legal for
    // `Segment` and is why this is not simply an error.
    let end = match size {
        Some(size) => after_size.checked_add(size as usize)?.min(data.len()),
        None => data.len(),
    };

    Some((id, data.get(after_size..end)?, end))
}

/// Reads an EBML element ID, marker bits and all — the marker is part of the ID
/// rather than only a length prefix, so it is kept.
fn ebml_id(data: &[u8], offset: usize) -> Option<(u32, usize)> {
    let length = ebml_marker_len(*data.get(offset)?)?;
    if length > 4 {
        return None;
    }

    let mut id = 0u32;
    for index in 0..length {
        id = (id << 8) | *data.get(offset + index)? as u32;
    }

    Some((id, offset + length))
}

/// Reads an EBML size, whose marker bit *is* stripped, unlike an ID's.
///
/// A size with every value bit set means "unknown", reported here as `None`.
fn ebml_size(data: &[u8], offset: usize) -> Option<(Option<u64>, usize)> {
    let first = *data.get(offset)?;
    let length = ebml_marker_len(first)?;
    if length > 8 {
        return None;
    }

    let mut value = first as u64 & (0xFF >> length);
    let mut unknown = value == (0xFFu64 >> length);

    for index in 1..length {
        let byte = *data.get(offset + index)?;
        value = (value << 8) | byte as u64;
        unknown &= byte == 0xFF;
    }

    Some((if unknown { None } else { Some(value) }, offset + length))
}

/// How many bytes an EBML ID or size occupies, from the position of the highest
/// set bit in its first byte.
fn ebml_marker_len(byte: u8) -> Option<usize> {
    (byte != 0).then(|| byte.leading_zeros() as usize + 1)
}

// ---------------------------------------------------------------------------
// Codec tables
// ---------------------------------------------------------------------------

/// Maps a `WAVEFORMATEX` format tag, as used by both WAV and ASF.
fn wave_format_tag(tag: u16) -> Option<Encoding> {
    Some(match tag {
        // PCM, IEEE float, A-law, µ-law. The companded pair are still PCM as
        // far as the container is concerned; expanding them is the decoder's
        // job.
        0x0001 | 0x0003 | 0x0006 | 0x0007 => Encoding::Pcm,
        0x0055 => Encoding::Mp3,
        0x00FF | 0x1600 | 0x1601 => Encoding::AacLc,
        0x000A | 0x0160 | 0x0161 | 0x0162 | 0x0163 => Encoding::Wma,
        0x0092 | 0x2000 => Encoding::Ac3,
        0x2001 => Encoding::Dts,
        0xF1AC => Encoding::Flac,
        _ => return None,
    })
}

/// Maps an AIFF-C compression type.
fn aiff_compression(compression: &[u8; 4]) -> Option<Encoding> {
    Some(match compression {
        // Uncompressed, in every endianness and width AIFF-C spells out.
        b"NONE" | b"sowt" | b"twos" | b"raw " | b"in24" | b"in32" | b"fl32" | b"FL32"
        | b"fl64" | b"FL64" => Encoding::Pcm,
        b"ulaw" | b"ULAW" | b"alaw" | b"ALAW" => Encoding::Pcm,
        b"alac" => Encoding::Alac,
        b".mp3" => Encoding::Mp3,
        b"aac " => Encoding::AacLc,
        _ => return None,
    })
}

/// Maps a CAF format ID.
fn caf_format(format: &[u8; 4]) -> Option<Encoding> {
    Some(match format {
        b"lpcm" => Encoding::Pcm,
        b"ulaw" | b"alaw" => Encoding::Pcm,
        b"alac" => Encoding::Alac,
        b"aac " => Encoding::AacLc,
        b".mp3" => Encoding::Mp3,
        b"flac" => Encoding::Flac,
        b"opus" => Encoding::Opus,
        _ => return None,
    })
}

/// Maps an MP4 sample entry's four-character code.
///
/// `mp4a` is deliberately absent: it needs its `esds` read, so it is handled
/// where the entry's bytes are still to hand.
fn mp4_sample_format(format: &[u8; 4]) -> Option<Encoding> {
    Some(match format {
        b"alac" => Encoding::Alac,
        b"ac-3" => Encoding::Ac3,
        b"ec-3" => Encoding::EAc3,
        b"dtsc" | b"dtse" | b"dtsh" => Encoding::Dts,
        b"dtsl" => Encoding::DtsHdMa,
        b"Opus" => Encoding::Opus,
        b"fLaC" => Encoding::Flac,
        b"mlpa" => Encoding::TrueHd,
        b"samr" => Encoding::AmrNb,
        b"sawb" => Encoding::AmrWb,
        b".mp3" | b"mp3 " => Encoding::Mp3,
        b"sowt" | b"twos" | b"lpcm" | b"raw " | b"in24" | b"in32" | b"fl32" | b"fl64"
        | b"NONE" => Encoding::Pcm,
        b"ulaw" | b"alaw" => Encoding::Pcm,
        _ => return None,
    })
}

/// Maps a Matroska `CodecID` string.
fn matroska_codec(codec: &[u8]) -> Option<Encoding> {
    let codec = std::str::from_utf8(codec).ok()?.trim_end_matches('\0');

    Some(match codec {
        "A_OPUS" => Encoding::Opus,
        "A_VORBIS" => Encoding::Vorbis,
        "A_FLAC" => Encoding::Flac,
        "A_ALAC" => Encoding::Alac,
        "A_TRUEHD" => Encoding::TrueHd,
        "A_MPEG/L3" => Encoding::Mp3,
        "A_EAC3" => Encoding::EAc3,
        "A_DTS/LOSSLESS" => Encoding::DtsHdMa,
        // The AAC IDs spell the profile out when they know it — "A_AAC/MPEG4/LC
        // /SBR" — while a bare "A_AAC" leaves it to the track's CodecPrivate.
        _ if codec.starts_with("A_AAC") => {
            if codec.ends_with("/SBR") {
                Encoding::AacHe
            } else {
                Encoding::AacLc
            }
        }
        _ if codec.starts_with("A_PCM") => Encoding::Pcm,
        _ if codec.starts_with("A_AC3") => Encoding::Ac3,
        _ if codec.starts_with("A_DTS") => Encoding::Dts,
        _ => return None,
    })
}

/// Maps the signature at the start of an Ogg codec's first packet.
///
/// Speex is recognisable here too but has no [`Encoding`] of its own, so it
/// falls through as unsupported rather than being mislabelled.
fn ogg_codec(packet: &[u8]) -> Option<Encoding> {
    if tag_at(packet, 0, b"OpusHead") {
        return Some(Encoding::Opus);
    }
    if tag_at(packet, 0, b"\x01vorbis") {
        return Some(Encoding::Vorbis);
    }
    if tag_at(packet, 0, b"\x7FFLAC") {
        return Some(Encoding::Flac);
    }

    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether `bytes` starts with a valid MPEG audio frame header.
///
/// Past the sync word, every field with a reserved value is checked: a "header"
/// claiming a reserved version, layer, bitrate, or sample rate is some other
/// file's data lining up by coincidence, not a frame.
fn is_frame_header(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }

    // Eleven set bits: all of byte 0, then the top three of byte 1.
    if bytes[0] != 0xFF || bytes[1] & 0xE0 != 0xE0 {
        return false;
    }

    let version = (bytes[1] >> 3) & 0b11;
    let layer = (bytes[1] >> 1) & 0b11;
    let bitrate = bytes[2] >> 4;
    let sample_rate = (bytes[2] >> 2) & 0b11;

    version != 0b01 && layer != 0b00 && bitrate != 0b1111 && sample_rate != 0b11
}

/// Whether `bytes` starts with an ADTS frame header.
///
/// A 12-bit sync word, then the layer bits, which ADTS requires to be `00` —
/// the same value MPEG audio reserves. That is what keeps AAC and MP3 apart,
/// since both open with `0xFF` and three more set bits.
fn is_adts_header(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] & 0xF6 == 0xF0
}

/// Whether `bytes` starts with a DTS sync word.
///
/// DTS ships in four bit-packings — 14- and 16-bit words, big- and
/// little-endian — each with its own byte pattern for the same sync value.
fn is_dts_sync(bytes: &[u8]) -> bool {
    const SYNC_WORDS: [[u8; 4]; 5] = [
        [0x7F, 0xFE, 0x80, 0x01], // 16-bit big-endian
        [0xFE, 0x7F, 0x01, 0x80], // 16-bit little-endian
        [0x1F, 0xFF, 0xE8, 0x00], // 14-bit big-endian
        [0xFF, 0x1F, 0x00, 0xE8], // 14-bit little-endian
        [0x64, 0x58, 0x20, 0x25], // extension substream (DTS-HD)
    ];

    SYNC_WORDS.iter().any(|sync| tag_at(bytes, 0, sync))
}

/// Total length of the ID3v2 tag at the front of `header`: a 10-byte head, the
/// size it declares, and a 10-byte footer if the flags say there is one.
///
/// The size is stored "syncsafe" — seven bits per byte, so that no byte of it
/// can be mistaken for a frame sync — which is why this shifts by 7 rather than
/// reading a plain big-endian integer.
///
/// `header` must be at least 10 bytes.
fn id3_len(header: &[u8]) -> usize {
    let size = header[6..10]
        .iter()
        .fold(0usize, |total, byte| (total << 7) | (byte & 0x7F) as usize);
    let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };

    10 + size + footer
}

/// Whether `header` carries `tag` at `offset`, without panicking on a file too
/// short to reach it.
fn tag_at(header: &[u8], offset: usize, tag: &[u8]) -> bool {
    header.len() >= offset + tag.len() && &header[offset..offset + tag.len()] == tag
}

/// Rewinds `file` and reads up to `len` bytes from the start.
///
/// The rewind matters: identification may already have seeked forward looking
/// for a frame sync, so the read position is not to be trusted.
fn header_from_start(file: &mut File, path: &Path, len: usize) -> Result<Vec<u8>, Error> {
    read_at(file, path, 0, len)
}

/// Reads up to `len` bytes at `offset`, returning fewer if the file ends first.
///
/// Every parser here seeks explicitly rather than reading forward, since a
/// container's own size fields are what say where to look next.
fn read_at(file: &mut File, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| io_error(path, error))?;

    let mut buffer = vec![0u8; len];
    let read = read_up_to(file, &mut buffer).map_err(|error| io_error(path, error))?;
    buffer.truncate(read);

    Ok(buffer)
}

/// The first index at which `needle` appears in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Reads a four-character code, the tag half of every chunk and box header.
fn fourcc_at(data: &[u8], offset: usize) -> Option<[u8; 4]> {
    data.get(offset..offset + 4)?.try_into().ok()
}

/// A four-character code as text, for error messages. Codes are nominally
/// printable ASCII, but a malformed file's is whatever happened to be there.
fn fourcc_name(code: &[u8; 4]) -> String {
    String::from_utf8_lossy(code).into_owned()
}

fn u16_le_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn u32_le_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u32_be_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u64_le_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn u64_be_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn i64_be_at(data: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_be_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Fills as much of `buffer` as the file has, returning how many bytes landed.
///
/// Unlike `read_exact`, a short file is not an error — a 40-byte file is simply
/// not one of these formats. `Read::read` is also allowed to return fewer bytes
/// than asked for without being at the end, hence the loop.
fn read_up_to(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;

    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(filled)
}

/// A decoder for this exists but was compiled out, naming the cargo feature
/// that carries it.
///
/// Separate from [`no_decoder`] on purpose: this one the caller can fix, and
/// the message says how.
fn feature_required(what: &str, feature: &str) -> Error {
    Error::with_message(
        ErrorKind::UnsupportedOperation,
        format!("{what} requires the `{feature}` feature"),
    )
}

/// No Rust decoder for this format exists to wire up, so no feature will help.
///
/// Identification still works, which is the point of saying so plainly: the
/// file was read correctly and named correctly, and only decoding is missing.
fn no_decoder(what: &str) -> Error {
    Error::with_message(
        ErrorKind::UnsupportedOperation,
        format!("{what} has no Rust decoder available; the format is identified but cannot be decoded"),
    )
}

/// The file is the container it claims to be, but its structure does not hold
/// together — a size field running past the end, a required chunk missing.
fn malformed(path: &Path, what: &str) -> Error {
    Error::with_message(
        ErrorKind::InvalidInput,
        format!("malformed {what}: {}", path.display()),
    )
}

/// The file parsed fine and names a codec this crate has no [`Encoding`] for.
fn unsupported(path: &Path, what: String) -> Error {
    Error::with_message(
        ErrorKind::UnsupportedOperation,
        format!("unsupported {what}: {}", path.display()),
    )
}

/// Wraps an IO failure, keeping the path in the message since the caller only
/// gets a [`cpal::Error`] back and would otherwise lose track of which file
/// failed.
fn io_error(path: &Path, error: io::Error) -> Error {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Other,
    };

    Error::with_message(kind, format!("could not read {}: {error}", path.display()))
}
