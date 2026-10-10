//! FLAC writing, checked by reading the files back with atome's own decoder.
//!
//! Lossless means exactly equal, so every round trip here compares samples
//! with `==`, not within a tolerance. The lengths are deliberately not whole
//! blocks, so the short block at the end — the one a streaming encoder most
//! easily drops — is always exercised.
//!
//! ```text
//! cargo test --features import,export --test export_flac
//! ```

#![cfg(all(feature = "import", feature = "export"))]

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use atome::export::{to_flac, to_flac_into, write_flac, write_flac_into, FlacWriter};
use atome::import::{self, Decoded, Samples};
use cpal::I24;

/// A scratch file that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Scratch(std::env::temp_dir().join(format!("atome-{}-{name}", std::process::id())))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// A sine at `hertz`, `amplitude` of full scale, interleaved over `channels`
/// with each channel a little out of phase so they cannot be confused.
fn sine(frames: usize, channels: usize, rate: u32, hertz: f64, full_scale: f64) -> Vec<i32> {
    (0..frames)
        .flat_map(|frame| {
            (0..channels).map(move |channel| {
                let phase = frame as f64 * hertz * std::f64::consts::TAU / f64::from(rate)
                    + channel as f64 * 0.7;
                (phase.sin() * full_scale * 0.8).round() as i32
            })
        })
        .collect()
}

#[test]
fn sixteen_bit_stereo_comes_back_bit_for_bit() {
    let file = Scratch::new("16.flac");
    let original = sine(10_007, 2, 48_000, 441.0, 32_767.0);

    let mut writer = FlacWriter::create(&file.0, 48_000, 2, 16).unwrap();
    // In uneven pieces, as a stream would hand them over.
    for piece in original.chunks(2 * 1_234) {
        writer.write(piece).unwrap();
    }
    let summary = writer.finish().unwrap();

    assert_eq!(summary.frames, 10_007);
    assert_eq!(std::fs::read(&file.0).unwrap()[..4], *b"fLaC");

    let decoded = import::decode(&file.0).unwrap();
    assert_eq!((decoded.sample_rate, decoded.channels), (48_000, 2));

    let Samples::I16(samples) = decoded.samples else {
        panic!("a 16-bit FLAC should decode as I16, not {:?}", decoded.samples.format());
    };

    let expected: Vec<i16> = original.iter().map(|&sample| sample as i16).collect();
    assert_eq!(samples, expected);
}

#[test]
fn twenty_four_bit_mono_comes_back_bit_for_bit_through_write_flac() {
    let file = Scratch::new("24.flac");
    let original = sine(5_003, 1, 96_000, 1_000.0, 8_388_607.0);

    let decoded = Decoded {
        samples: Samples::I24(original.iter().map(|&sample| I24::new(sample).unwrap()).collect()),
        sample_rate: 96_000,
        channels: 1,
    };

    let summary = write_flac(&file.0, &decoded).unwrap();
    assert_eq!((summary.frames, summary.bits_per_sample), (5_003, 24));

    let back = import::decode(&file.0).unwrap();
    assert_eq!(back.sample_rate, 96_000);
    assert_eq!(back.samples, decoded.samples);
}

/// A lossy source — MP3 decodes to float — lands at 24 bits, and those 24
/// bits are exactly what the MP3 decoder produced.
#[test]
fn a_lossy_file_becomes_24_bit_flac_holding_exactly_what_it_decoded_to() {
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test_data/test415hz.mp3");
    let file = Scratch::new("from-mp3.flac");

    let summary = to_flac(&mp3, &file.0).unwrap();
    assert_eq!(summary.bits_per_sample, 24);

    let straight = import::decode(&mp3).unwrap();
    let through_flac = import::decode(&file.0).unwrap();

    assert_eq!(through_flac.sample_rate, straight.sample_rate);
    assert_eq!(through_flac.channels, straight.channels);
    assert_eq!(summary.frames as usize, straight.frames());

    let Samples::I24(flac) = through_flac.samples else {
        panic!("24-bit FLAC should decode as I24");
    };

    let expected: Vec<I24> = straight.samples.to_vec();
    assert_eq!(flac.len(), expected.len());
    assert!(flac == expected, "the FLAC holds something other than what the MP3 decoded to");
}

/// What vtome's importer does with a video: the soundtrack out, as FLAC, with
/// the picture never touched. Skipped without the fixture, which
/// `tests/test_data/make_fixtures.sh` makes.
#[test]
fn a_video_soundtrack_comes_out_as_flac() {
    let video = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test_data/tone_video.mp4");

    if !video.exists() {
        eprintln!("skipping: {} is missing — run make_fixtures.sh", video.display());
        return;
    }

    let file = Scratch::new("from-video.flac");
    let summary = to_flac(&video, &file.0).unwrap();

    assert_eq!((summary.sample_rate, summary.channels), (48_000, 2));
    // One second, give or take the AAC encoder's priming.
    assert!(
        (0.95..1.1).contains(&summary.duration()),
        "{} s of audio from a one-second video",
        summary.duration()
    );

    let back = import::decode(&file.0).unwrap();
    assert_eq!(back.frames() as u64, summary.frames);
}

#[test]
fn an_empty_stream_is_still_a_valid_file() {
    let file = Scratch::new("empty.flac");

    let summary = FlacWriter::create(&file.0, 44_100, 2, 16).unwrap().finish().unwrap();

    assert_eq!(summary.frames, 0);
    assert_eq!(std::fs::read(&file.0).unwrap().len(), 42, "header only");
}

/// The same samples through a path and through a writer must be the same bytes:
/// a writer is a different destination, not a different encoder.
#[test]
fn a_writer_gets_exactly_the_bytes_a_path_would() {
    let file = Scratch::new("path-twin.flac");
    let original = sine(10_007, 2, 48_000, 441.0, 32_767.0);

    let mut by_path = FlacWriter::create(&file.0, 48_000, 2, 16).unwrap();
    by_path.write(&original).unwrap();
    let path_summary = by_path.finish().unwrap();

    let mut sink = Cursor::new(Vec::new());
    let mut by_writer = FlacWriter::new(&mut sink, 48_000, 2, 16).unwrap();
    by_writer.write(&original).unwrap();
    let writer_summary = by_writer.finish().unwrap();

    assert_eq!(path_summary, writer_summary);
    assert_eq!(sink.get_ref(), &std::fs::read(&file.0).unwrap());
    assert_eq!(writer_summary.bytes, sink.get_ref().len() as u64);
}

#[test]
fn a_writer_that_already_holds_bytes_has_its_own_header_patched() {
    // A container around the stream: 100 bytes of something else first. The
    // header patch must land at the stream's start, not at byte zero.
    let original = sine(5_000, 1, 44_100, 440.0, 32_767.0);

    let mut sink = Cursor::new(vec![0xAA_u8; 100]);
    sink.seek(SeekFrom::End(0)).unwrap();

    let mut writer = FlacWriter::new(&mut sink, 44_100, 1, 16).unwrap();
    writer.write(&original).unwrap();
    let summary = writer.finish().unwrap();

    let bytes = sink.into_inner();
    assert_eq!(bytes[..100], [0xAA; 100], "the leading bytes were overwritten");
    assert_eq!(&bytes[100..104], b"fLaC");
    assert_eq!(summary.bytes, (bytes.len() - 100) as u64);

    let file = Scratch::new("offset.flac");
    std::fs::write(&file.0, &bytes[100..]).unwrap();
    let decoded = import::decode(&file.0).unwrap();
    assert_eq!(decoded.frames(), 5_000);
}

#[test]
fn finishing_hands_the_writer_back_at_the_end_of_the_stream() {
    let mut writer = FlacWriter::new(Cursor::new(Vec::new()), 44_100, 1, 16).unwrap();
    writer.write(&[1, 2, 3, 4]).unwrap();

    let (summary, mut sink) = writer.finish_into_inner().unwrap();

    assert_eq!(sink.position(), summary.bytes);
    // A trailing box written by whoever owns the writer lands after the stream.
    sink.write_all(b"after").unwrap();
    assert_eq!(&sink.get_ref()[summary.bytes as usize..], b"after");
}

#[test]
fn write_flac_into_matches_write_flac() {
    let file = Scratch::new("write-flac-twin.flac");
    let decoded = Decoded {
        sample_rate: 96_000,
        channels: 1,
        samples: Samples::I24(
            sine(5_003, 1, 96_000, 1_000.0, 8_388_607.0)
                .into_iter()
                .map(|sample| I24::new(sample).unwrap())
                .collect(),
        ),
    };

    write_flac(&file.0, &decoded).unwrap();

    let mut sink = Cursor::new(Vec::new());
    write_flac_into(&mut sink, &decoded).unwrap();

    assert_eq!(sink.into_inner(), std::fs::read(&file.0).unwrap());
}

#[test]
fn to_flac_into_matches_to_flac() {
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test_data/test415hz.mp3");
    let file = Scratch::new("to-flac-twin.flac");

    let by_path = to_flac(&mp3, &file.0).unwrap();

    let mut sink = Cursor::new(Vec::new());
    let by_writer = to_flac_into(&mp3, &mut sink).unwrap();

    assert_eq!(by_path, by_writer);
    assert_eq!(sink.into_inner(), std::fs::read(&file.0).unwrap());
}

#[test]
fn a_refused_stream_is_refused_before_the_writer_is_touched() {
    let mut sink = Cursor::new(vec![7_u8; 10]);

    assert!(FlacWriter::new(&mut sink, 48_000, 0, 16).is_err());
    assert!(FlacWriter::new(&mut sink, 48_000, 2, 7).is_err());
    assert_eq!(sink.into_inner(), vec![7_u8; 10]);
}
