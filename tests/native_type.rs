//! `decode` must hand back the sample type the *file* is in, not one chosen by
//! the caller.
//!
//! The FLAC cases are the ones that matter: Symphonia decodes every FLAC into
//! `i32` buffers whatever the file's depth, so reading the type off the decoded
//! buffer would report a 16-bit FLAC as 32-bit. The declared bit depth is what
//! settles the width, and the buffer only settles signedness.
//!
//! Fixtures beyond `test415hz.mp3` are not committed; generate them with:
//!
//! ```sh
//! cd tests/test_data
//! ffmpeg -f lavfi -i "sine=frequency=440:duration=1:sample_rate=48000" -ac 2 tone.wav
//! ffmpeg -y -i tone.wav -c:a pcm_u8    tone8.wav
//! ffmpeg -y -i tone.wav -c:a pcm_s24le tone24.wav
//! ffmpeg -y -i tone.wav -c:a pcm_s32le tone32.wav
//! ffmpeg -y -i tone.wav -c:a pcm_f32le tone32f.wav
//! ffmpeg -y -i tone.wav -c:a flac      tone.flac
//! ffmpeg -y -i tone.wav -c:a flac -sample_fmt s32 tone24.flac
//! ffmpeg -y -i tone.wav -c:a alac      tone_alac.m4a
//! ```
//!
//! Anything missing is skipped rather than failed.
#![cfg(feature = "import")]

use std::path::{Path, PathBuf};

use atome::import::{self, Samples};
use cpal::SampleFormat;

fn test_data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("test_data").join(name)
}

fn format_of(name: &str) -> Option<SampleFormat> {
    let path = test_data(name);
    if !path.exists() {
        eprintln!("skipping {name}: fixture not present");
        return None;
    }
    Some(import::decode(&path).unwrap_or_else(|e| panic!("{name}: {e}")).sample_format())
}

#[test]
fn each_file_decodes_to_its_own_type() {
    for (name, expected) in [
        ("tone8.wav", SampleFormat::U8),
        ("tone.wav", SampleFormat::I16),
        ("tone24.wav", SampleFormat::I24),
        ("tone32.wav", SampleFormat::I32),
        ("tone32f.wav", SampleFormat::F32),
        ("tone.aiff", SampleFormat::I16),
        ("tone.caf", SampleFormat::I16),
        ("tone.flac", SampleFormat::I16),
        ("tone24.flac", SampleFormat::I24),
        // ALAC declares no bit depth, so its decoder's own buffer type is
        // the best available answer.
        ("tone_alac.m4a", SampleFormat::I32),
    ] {
        if let Some(actual) = format_of(name) {
            assert_eq!(actual, expected, "{name} decoded as {actual:?}");
            println!("{name:16} -> {actual:?}");
        }
    }

    // Lossy codecs decode to float whatever went in, which is also a fact
    // about the file rather than a choice.
    for name in ["tone.mp3", "tone.ogg", "tone_aac.m4a", "test415hz.mp3"] {
        if let Some(actual) = format_of(name) {
            println!("{name:16} -> {actual:?}");
        }
    }
}

/// The samples really are in that type, not a converted copy.
#[test]
fn the_variant_matches_the_reported_format() {
    for name in ["tone8.wav", "tone.wav", "tone24.wav", "tone.mp3"] {
        let path = test_data(name);
        if !path.exists() { continue; }

        let decoded = import::decode(&path).expect("decode");
        let matches = matches!(
            (&decoded.samples, decoded.sample_format()),
            (Samples::U8(_), SampleFormat::U8)
                | (Samples::I16(_), SampleFormat::I16)
                | (Samples::I24(_), SampleFormat::I24)
                | (Samples::I32(_), SampleFormat::I32)
                | (Samples::F32(_), SampleFormat::F32)
        );
        assert!(matches, "{name}: variant and reported format disagree");
    }
}

/// 24-bit values must land in range: Symphonia makes no promise about its own
/// i24, and cpal's rejects anything outside 24 bits.
#[test]
fn twenty_four_bit_values_are_in_range() {
    let path = test_data("tone24.wav");
    if !path.exists() { return; }

    let decoded = import::decode(&path).expect("decode");
    let Samples::I24(samples) = &decoded.samples else {
        panic!("expected I24, got {:?}", decoded.sample_format());
    };

    let peak = samples.iter().map(|s| s.inner().abs()).max().unwrap_or(0);
    assert!(peak > 1 << 20, "24-bit audio came back near silent: peak {peak}");
    assert!(peak < 1 << 23, "24-bit value out of range: {peak}");
    println!("tone24.wav peak = {peak} (limit {})", 1 << 23);
}
