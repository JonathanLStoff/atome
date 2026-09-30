//! FLAC, through `flacenc`, one block at a time.
//!
//! `flacenc`'s own entry point builds the whole encoded stream in memory before
//! a byte is written — fine for a sound effect, a few hundred megabytes for a
//! show's worth of audio. This writer encodes one block and writes it straight
//! to disk. What the header needs that is only known at the end — the sample
//! count, the MD5 of the audio, the largest and smallest frame — goes in last:
//! a placeholder header is written first and [`FlacWriter::finish`] overwrites
//! it in place. The header is a fixed 42 bytes, so nothing after it moves.
//!
//! `flacenc` takes 8 to 24 bits a sample. Sources wider than that — 32-bit
//! integer, and float, which is what every lossy decoder hands back — are
//! written at 24 bits: more resolution than any lossy codec delivered, and
//! more than any converter reproduces.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cpal::{Error, ErrorKind, I24, U24};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use crate::import::{Decoded, Samples};

/// The largest channel count FLAC can carry.
const MAX_CHANNELS: u16 = 8;

/// Samples a block handed to [`FlacWriter::write`] by the whole-file helpers,
/// per channel.
const CHUNK_FRAMES: usize = 4096;

/// What a finished FLAC file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacSummary {
    /// Frames of audio — one sample per channel each.
    pub frames: u64,
    /// Samples a second.
    pub sample_rate: u32,
    /// Channels.
    pub channels: u16,
    /// Bits a sample, as written.
    pub bits_per_sample: u8,
    /// The file's size.
    pub bytes: u64,
}

impl FlacSummary {
    /// How long the audio runs for, in seconds.
    pub fn duration(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate.max(1))
    }
}

/// Writes a FLAC file a block at a time.
///
/// ```no_run
/// use atome::export::FlacWriter;
///
/// let mut writer = FlacWriter::create("tone.flac", 48_000, 2, 16)?;
/// let second: Vec<i32> = (0..48_000)
///     .flat_map(|n| {
///         let sample = ((n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 8000.0) as i32;
///         [sample, sample]
///     })
///     .collect();
/// writer.write(&second)?;
/// let summary = writer.finish()?;
/// assert_eq!(summary.frames, 48_000);
/// # Ok::<(), cpal::Error>(())
/// ```
///
/// The file is not a valid FLAC until [`finish`](FlacWriter::finish) has run:
/// until then its header is a placeholder. Dropping a writer without finishing
/// leaves exactly that behind.
pub struct FlacWriter {
    path: PathBuf,
    file: BufWriter<File>,
    config: Verified<config::Encoder>,
    stream: Stream,
    buffer: (FrameBuf, Context),
    /// Interleaved samples short of a whole block, waiting for the rest.
    pending: Vec<i32>,
    block_size: usize,
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u8,
    header_len: usize,
    /// The smallest and largest encoded frame so far, in bytes.
    frame_sizes: Option<(usize, usize)>,
}

impl FlacWriter {
    /// A FLAC file at `path` for `channels` channels of `bits_per_sample`-bit
    /// audio at `sample_rate`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`] for 0 or more than 8 channels, a bit depth
    /// outside 8–24, or a sample rate FLAC cannot state; and whatever creating
    /// the file refuses.
    pub fn create(
        path: impl AsRef<Path>,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u8,
    ) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();

        if !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(invalid(format!(
                "FLAC carries 1 to {MAX_CHANNELS} channels, not {channels}"
            )));
        }

        if !(8..=24).contains(&bits_per_sample) {
            return Err(invalid(format!(
                "the FLAC encoder writes 8 to 24 bits a sample, not {bits_per_sample}"
            )));
        }

        let config = config::Encoder::default();
        let block_size = config.block_size;
        let config = config
            .into_verified()
            .map_err(|(_, error)| encoder_error("the encoder configuration", error))?;

        let stream = Stream::new(
            sample_rate as usize,
            usize::from(channels),
            usize::from(bits_per_sample),
        )
        .map_err(|error| invalid(format!("a {sample_rate} Hz FLAC stream: {error}")))?;

        let header = header_bytes(&stream)?;

        let file = File::create(&path).map_err(|error| io_error(&path, "create", error))?;
        let mut file = BufWriter::new(file);
        file.write_all(&header)
            .map_err(|error| io_error(&path, "write", error))?;

        let buffer = (
            FrameBuf::with_size(usize::from(channels), block_size)
                .map_err(|error| encoder_error("the frame buffer", error))?,
            Context::new(usize::from(bits_per_sample), usize::from(channels)),
        );

        Ok(FlacWriter {
            path,
            file,
            config,
            stream,
            buffer,
            pending: Vec::with_capacity(block_size * usize::from(channels)),
            block_size,
            channels: usize::from(channels),
            sample_rate,
            bits_per_sample,
            header_len: header.len(),
            frame_sizes: None,
        })
    }

    /// Adds interleaved samples, each already in the signed range of the bit
    /// depth the writer was made for — `-32768..=32767` at 16 bits.
    ///
    /// Any number of whole frames: a block is encoded whenever enough have
    /// arrived, and the remainder waits for the next call or for `finish`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`] for a count that is not whole frames, or a
    /// sample outside the bit depth; whatever writing the file refuses.
    pub fn write(&mut self, interleaved: &[i32]) -> Result<(), Error> {
        if interleaved.len() % self.channels != 0 {
            return Err(invalid(format!(
                "{} samples is not a whole number of {}-channel frames",
                interleaved.len(),
                self.channels
            )));
        }

        self.pending.extend_from_slice(interleaved);

        let whole_block = self.block_size * self.channels;

        while self.pending.len() >= whole_block {
            let block: Vec<i32> = self.pending.drain(..whole_block).collect();
            self.encode(&block)?;
        }

        Ok(())
    }

    /// Encodes whatever is left, writes the real header, and closes the file.
    ///
    /// # Errors
    ///
    /// As [`write`](FlacWriter::write).
    pub fn finish(mut self) -> Result<FlacSummary, Error> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.encode(&rest)?;
        }

        let context = &self.buffer.1;
        let frames = context.total_samples() as u64;

        let info = self.stream.stream_info_mut();
        info.set_block_sizes(self.block_size, self.block_size)
            .map_err(|error| encoder_error("the block sizes", error))?;
        if let Some((smallest, largest)) = self.frame_sizes {
            info.set_frame_sizes(smallest, largest)
                .map_err(|error| encoder_error("the frame sizes", error))?;
        }
        info.set_md5_digest(&context.md5_digest());
        info.set_total_samples(context.total_samples());

        let header = header_bytes(&self.stream)?;

        // Always the same 42 bytes; anything else would overwrite audio.
        if header.len() != self.header_len {
            return Err(Error::with_message(
                ErrorKind::Other,
                format!(
                    "the finished FLAC header is {} bytes where the placeholder was {}",
                    header.len(),
                    self.header_len
                ),
            ));
        }

        let path = self.path.clone();
        let failed = |error| io_error(&path, "write", error);

        self.file.flush().map_err(failed)?;
        let file = self.file.get_mut();
        let bytes = file.stream_position().map_err(failed)?;
        file.seek(SeekFrom::Start(0)).map_err(failed)?;
        file.write_all(&header).map_err(failed)?;
        file.sync_all().map_err(failed)?;

        Ok(FlacSummary {
            frames,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
            bits_per_sample: self.bits_per_sample,
            bytes,
        })
    }

    /// Encodes one block — whole, or the short one at the end — and writes it.
    fn encode(&mut self, interleaved: &[i32]) -> Result<(), Error> {
        self.buffer
            .fill_interleaved(interleaved)
            .map_err(|error| encoder_error("a block of samples", error))?;

        let number = self.buffer.1.current_frame_number().unwrap_or(0);

        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.buffer.0,
            number,
            self.stream.stream_info(),
        )
        .map_err(|error| {
            invalid(format!(
                "encoding FLAC frame {number}: {error} — are the samples within {} bits?",
                self.bits_per_sample
            ))
        })?;

        let mut sink = ByteSink::new();
        frame
            .write(&mut sink)
            .map_err(|error| encoder_error("a frame", error))?;
        let bytes = sink.as_slice();

        self.file
            .write_all(bytes)
            .map_err(|error| io_error(&self.path, "write", error))?;

        self.frame_sizes = Some(match self.frame_sizes {
            None => (bytes.len(), bytes.len()),
            Some((smallest, largest)) => (smallest.min(bytes.len()), largest.max(bytes.len())),
        });

        Ok(())
    }
}

/// Writes decoded audio to a FLAC file at `path`, at the bit depth its samples
/// need: 8, 16, or 24 — 24 for 32-bit integer and float sources.
///
/// # Errors
///
/// As [`FlacWriter`].
pub fn write_flac(path: impl AsRef<Path>, decoded: &Decoded) -> Result<FlacSummary, Error> {
    let bits = bits_for(&decoded.samples);
    let mut writer = FlacWriter::create(path, decoded.sample_rate, decoded.channels, bits)?;
    let chunk = CHUNK_FRAMES * usize::from(decoded.channels.max(1));

    macro_rules! write_as {
        ($samples:expr, $convert:expr) => {
            for block in $samples.chunks(chunk) {
                let block: Vec<i32> = block.iter().map($convert).collect();
                writer.write(&block)?;
            }
        };
    }

    match &decoded.samples {
        Samples::U8(samples) => write_as!(samples, |&sample| i32::from(sample) - 128),
        Samples::I8(samples) => write_as!(samples, |&sample| i32::from(sample)),
        Samples::U16(samples) => write_as!(samples, |&sample| i32::from(sample) - 32_768),
        Samples::I16(samples) => write_as!(samples, |&sample| i32::from(sample)),
        Samples::U24(samples) => write_as!(samples, |&sample: &U24| sample.inner() - (1 << 23)),
        Samples::I24(samples) => write_as!(samples, |&sample: &I24| sample.inner()),
        // The bottom eight bits go: below the noise floor of anything that
        // was ever recorded, and the most FLAC's encoder here will take.
        Samples::U32(samples) => write_as!(samples, |&sample| ((sample ^ 0x8000_0000) as i32) >> 8),
        Samples::I32(samples) => write_as!(samples, |&sample| sample >> 8),
        Samples::F32(samples) => write_as!(samples, |&sample| float_to_24(f64::from(sample))),
        Samples::F64(samples) => write_as!(samples, |&sample| float_to_24(sample)),
    }

    writer.finish()
}

/// Decodes the audio in `input` — any file [`import`](crate::import) reads,
/// the audio track of a video included — and writes it to `output` as FLAC,
/// a block at a time, so a two-hour file is never held decoded in memory.
///
/// 8- and 16-bit sources are written at 16 bits; everything wider, and every
/// lossy source, at 24.
///
/// # Errors
///
/// Whatever identifying or decoding `input` refuses, and as [`FlacWriter`].
#[cfg(feature = "import")]
pub fn to_flac(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<FlacSummary, Error> {
    use cpal::SampleFormat;

    use crate::import;

    let input = input.as_ref();
    let encoding = import::find_type(input)?;

    // Opened once to learn what the file holds; decoding is lazy, so this
    // costs a header read.
    let probe = import::stream_as::<f32>(input, encoding)?;
    let narrow = matches!(
        probe.source_format(),
        SampleFormat::U8 | SampleFormat::I8 | SampleFormat::U16 | SampleFormat::I16
    );
    drop(probe);

    if narrow {
        let stream = import::stream_as::<i16>(input, encoding)?;
        pump(stream, output.as_ref(), 16, i32::from)
    } else {
        let stream = import::stream_as::<I24>(input, encoding)?;
        pump(stream, output.as_ref(), 24, |sample: I24| sample.inner())
    }
}

/// Reads `stream` to its end into a FLAC file.
#[cfg(feature = "import")]
fn pump<S: crate::output::SampleType>(
    mut stream: Box<dyn crate::import::AudioStream<S>>,
    output: &Path,
    bits: u8,
    convert: impl Fn(S) -> i32,
) -> Result<FlacSummary, Error> {
    let channels = stream.channels();
    let mut writer = FlacWriter::create(output, stream.sample_rate(), channels, bits)?;

    let mut block = vec![S::SILENCE; CHUNK_FRAMES * usize::from(channels.max(1))];
    let mut samples = Vec::with_capacity(block.len());

    loop {
        let read = stream.read(&mut block)?;
        if read == 0 {
            break;
        }

        samples.clear();
        samples.extend(block[..read].iter().map(|sample| convert(*sample)));
        writer.write(&samples)?;
    }

    writer.finish()
}

/// The bit depth a set of samples needs in FLAC.
fn bits_for(samples: &Samples) -> u8 {
    match samples {
        Samples::U8(_) | Samples::I8(_) => 8,
        Samples::U16(_) | Samples::I16(_) => 16,
        _ => 24,
    }
}

/// A float sample in `-1.0..=1.0` as a 24-bit integer, clipped rather than
/// wrapped: a decoder's overshoot past full scale becomes full scale, not the
/// opposite extreme.
fn float_to_24(sample: f64) -> i32 {
    const FULL: f64 = 8_388_607.0;

    (sample.clamp(-1.0, 1.0) * FULL).round() as i32
}

/// "fLaC" and the STREAMINFO block, with no frames after them.
fn header_bytes(stream: &Stream) -> Result<Vec<u8>, Error> {
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| encoder_error("the FLAC header", error))?;

    Ok(sink.into_inner())
}

fn invalid(message: String) -> Error {
    Error::with_message(ErrorKind::InvalidInput, message)
}

fn encoder_error(what: &str, error: impl std::fmt::Display) -> Error {
    Error::with_message(ErrorKind::Other, format!("FLAC encoder, {what}: {error}"))
}

fn io_error(path: &Path, doing: &str, error: std::io::Error) -> Error {
    let kind = match error.kind() {
        std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Other,
    };

    Error::with_message(kind, format!("could not {doing} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_become_24_bit_integers_clipped_at_full_scale() {
        assert_eq!(float_to_24(0.0), 0);
        assert_eq!(float_to_24(1.0), 8_388_607);
        assert_eq!(float_to_24(-1.0), -8_388_607);
        assert_eq!(float_to_24(1.5), 8_388_607, "an overshoot clips, it does not wrap");
    }

    #[test]
    fn impossible_streams_are_refused_before_a_file_is_made() {
        let directory = std::env::temp_dir();

        for (channels, bits) in [(0, 16), (9, 16), (2, 7), (2, 32)] {
            let path = directory.join(format!("atome-refused-{channels}-{bits}.flac"));
            assert!(FlacWriter::create(&path, 48_000, channels, bits).is_err());
            assert!(!path.exists(), "a refused writer left {} behind", path.display());
        }
    }

    #[test]
    fn a_part_frame_is_refused() {
        let path = std::env::temp_dir().join(format!("atome-part-frame-{}.flac", std::process::id()));
        let mut writer = FlacWriter::create(&path, 48_000, 2, 16).unwrap();

        assert!(writer.write(&[1, 2, 3]).is_err());

        drop(writer);
        std::fs::remove_file(path).ok();
    }
}
