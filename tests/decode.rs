//! End-to-end decoding against real encoder output.
//!
//! Checking the tone back out is what separates "the decoder ran" from "the
//! decoder produced the right audio": a wrong sample format, a channel swap, or
//! a planar/interleaved mix-up all still return plausible-looking sample
//! counts.
//!
//! Only `test415hz.mp3` is committed. The rest of the formats need fixtures
//! this repository does not carry; generate them into `tests/test_data` with:
//!
//! ```sh
//! ffmpeg -f lavfi -i "sine=frequency=440:duration=1:sample_rate=48000" \
//!     -ac 2 tests/test_data/tone.wav
//! for spec in "flac tone.flac" "libmp3lame tone.mp3" "aac tone_aac.m4a" \
//!             "alac tone_alac.m4a" "libopus tone.opus" "pcm_s16be tone.aiff"; do
//!     set -- $spec
//!     ffmpeg -y -i tests/test_data/tone.wav -c:a "$1" "tests/test_data/$2"
//! done
//! ```
//!
//! Tests for a fixture that is not present skip rather than fail.

#![cfg(feature = "import")]

use std::path::{Path, PathBuf};

use atome::import::{self, Encoding};

fn test_data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test_data")
        .join(name)
}

/// Peak amplitude, for scaling the thresholds below to whatever level the
/// material happens to sit at.
fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0f32, |peak, sample| peak.max(sample.abs()))
}

/// The stretch between the first and last sample carrying real signal.
fn signal_region(channel: &[f32], level: f32) -> &[f32] {
    let floor = 0.05 * level;
    let first = channel.iter().position(|s| s.abs() > floor).unwrap_or(0);
    let last = channel
        .iter()
        .rposition(|s| s.abs() > floor)
        .unwrap_or(channel.len().saturating_sub(1));

    &channel[first..=last.max(first)]
}

/// Estimates the dominant frequency of one channel, over the region that
/// actually holds signal.
///
/// Thresholds are relative to the signal's own peak, never absolute: real
/// material sits wherever it was mastered, and this repository's test file
/// peaks at about 0.12. Leading and trailing near-silence is excluded and
/// crossings are counted with hysteresis, because a lossy codec's priming
/// region is not digital silence but low-level noise whose zero crossings would
/// otherwise be counted as signal.
fn dominant_frequency(samples: &[f32], channels: u16, rate: u32) -> f32 {
    let channel: Vec<f32> = samples.iter().step_by(channels as usize).copied().collect();
    let level = peak(&channel);
    let signal = signal_region(&channel, level);

    let (low, high) = (-0.3 * level, 0.3 * level);
    let mut cycles = 0usize;
    let mut armed = false;

    for &sample in signal {
        if sample < low {
            armed = true;
        } else if armed && sample > high {
            cycles += 1;
            armed = false;
        }
    }

    cycles as f32 * rate as f32 / signal.len() as f32
}

/// How much of the file holds actual signal, in seconds.
fn signal_seconds(samples: &[f32], channels: u16, rate: u32) -> f32 {
    let channel: Vec<f32> = samples.iter().step_by(channels as usize).copied().collect();
    let level = peak(&channel);

    signal_region(&channel, level).len() as f32 / rate as f32
}

/// Decodes `name` and checks the tone came back intact.
///
/// Returns `false` if the fixture is not present, so a caller can skip.
fn check_decoded(
    name: &str,
    expected: Encoding,
    rate: u32,
    tone: f32,
    seconds: f32,
    tolerance: f32,
) -> bool {
    let path = test_data(name);
    if !path.exists() {
        eprintln!("skipping {name}: fixture not present");
        return false;
    }

    let encoding = import::find_type(&path).expect("identify");
    assert_eq!(encoding, expected, "{name} identified wrongly");

    let decoded = import::decode(&path).unwrap_or_else(|error| panic!("{name}: {error}"));

    // The file's own type, whatever it happens to be. Every check below works
    // in f32, so convert once here rather than at each use.
    let samples = decoded.samples.to_vec::<f32>();

    assert_eq!(decoded.channels, 2, "{name} channel count");
    assert_eq!(decoded.sample_rate, rate, "{name} sample rate");

    // Loose on purpose: this catches silence and clipping, not level.
    let amplitude = peak(&samples);
    assert!(
        (0.01..=1.01).contains(&amplitude),
        "{name} peak amplitude {amplitude} is silence or clipping"
    );

    // The signal itself must be the right length. Counting decoded frames
    // instead would pass a file whose audio is right but which carries a
    // second of untrimmed encoder padding.
    let duration = signal_seconds(&samples, decoded.channels, decoded.sample_rate);
    assert!(
        (duration - seconds).abs() < seconds * 0.05,
        "{name} holds {duration}s of signal, expected about {seconds}s"
    );

    let frequency = dominant_frequency(&samples, decoded.channels, decoded.sample_rate);
    assert!(
        (frequency - tone).abs() < tolerance,
        "{name} decoded a {frequency} Hz tone, expected {tone}"
    );

    true
}

/// Streams the same file and checks it matches the one-shot decode exactly.
fn check_streamed(name: &str) {
    let path = test_data(name);
    if !path.exists() {
        return;
    }

    let whole = import::decode(&path).expect("decode");
    let whole_samples = whole.samples.to_vec::<f32>();
    let mut stream = import::stream::<f32>(&path).expect("stream");

    assert_eq!(stream.sample_rate(), whole.sample_rate, "{name} stream rate");
    assert_eq!(stream.channels(), whole.channels, "{name} stream channels");

    // A deliberately awkward buffer: not a power of two, not a multiple of the
    // channel count, and smaller than any decoder's packet.
    let mut block = vec![0f32; 101];
    let mut streamed = Vec::new();

    loop {
        let read = stream.read(&mut block).expect("stream read");
        if read == 0 {
            break;
        }
        streamed.extend_from_slice(&block[..read]);
    }

    assert_eq!(
        streamed, whole_samples,
        "{name} streamed different audio to decoding in one shot"
    );
}

/// Five seconds of 415 Hz at 44.1 kHz — a real file rather than one generated
/// for this test, and quiet enough (peaking near 0.12) to catch any threshold
/// that assumes a full-scale signal.
#[test]
fn real_world_mp3() {
    assert!(
        check_decoded("test415hz.mp3", Encoding::Mp3, 44_100, 415.0, 5.0, 3.0),
        "tests/test_data/test415hz.mp3 is missing"
    );
    check_streamed("test415hz.mp3");
}

#[test]
fn wav_pcm() {
    if check_decoded("tone.wav", Encoding::Pcm, 48_000, 440.0, 1.0, 1.0) {
        check_streamed("tone.wav");
    }
}

#[test]
fn aiff_pcm() {
    if check_decoded("tone.aiff", Encoding::Pcm, 48_000, 440.0, 1.0, 1.0) {
        check_streamed("tone.aiff");
    }
}

#[test]
fn caf_pcm() {
    if check_decoded("tone.caf", Encoding::Pcm, 48_000, 440.0, 1.0, 1.0) {
        check_streamed("tone.caf");
    }
}

#[test]
fn flac() {
    if check_decoded("tone.flac", Encoding::Flac, 48_000, 440.0, 1.0, 1.0) {
        check_streamed("tone.flac");
    }
}

#[test]
fn mp3() {
    // Lossy, so the encoder's own filtering moves the edges around a little.
    if check_decoded("tone.mp3", Encoding::Mp3, 48_000, 440.0, 1.0, 5.0) {
        check_streamed("tone.mp3");
    }
}

#[test]
fn vorbis_in_ogg() {
    if check_decoded("tone.ogg", Encoding::Vorbis, 48_000, 440.0, 1.0, 5.0) {
        check_streamed("tone.ogg");
    }
}

#[test]
fn aac_in_mp4() {
    if check_decoded("tone_aac.m4a", Encoding::AacLc, 48_000, 440.0, 1.0, 5.0) {
        check_streamed("tone_aac.m4a");
    }
}

#[test]
fn aac_in_adts() {
    if check_decoded("tone.aac", Encoding::AacLc, 48_000, 440.0, 1.0, 5.0) {
        check_streamed("tone.aac");
    }
}

#[test]
fn alac_in_mp4() {
    if check_decoded("tone_alac.m4a", Encoding::Alac, 48_000, 440.0, 1.0, 1.0) {
        check_streamed("tone_alac.m4a");
    }
}

#[test]
#[cfg(feature = "import-opus")]
fn opus_in_ogg() {
    if check_decoded("tone.opus", Encoding::Opus, 48_000, 440.0, 1.0, 5.0) {
        check_streamed("tone.opus");
    }
}

#[test]
#[cfg(feature = "import-opus")]
fn opus_in_matroska() {
    if check_decoded("tone_opus.mkv", Encoding::Opus, 48_000, 440.0, 1.0, 5.0) {
        check_streamed("tone_opus.mkv");
    }
}

/// Formats with no Rust decoder must say exactly that, rather than pointing at
/// a cargo feature that would not help.
#[test]
fn formats_without_a_decoder_fail_clearly() {
    for encoding in [
        Encoding::Ac3,
        Encoding::EAc3,
        Encoding::Dts,
        Encoding::DtsHdMa,
        Encoding::TrueHd,
        Encoding::Wma,
        Encoding::AmrNb,
        Encoding::AmrWb,
    ] {
        let error = import::stream_as::<f32>(&test_data("test415hz.mp3"), encoding)
            .err()
            .expect("must fail");

        let message = error.message().unwrap_or_default();
        assert!(message.contains("no Rust decoder"), "{encoding:?}: {message}");
        assert!(!message.contains("feature"), "{encoding:?}: {message}");
    }
}
