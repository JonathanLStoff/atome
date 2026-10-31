//! Seeking a stream, and sections of one, checked to the sample on a FLAC file
//! whose every frame says where it is.
//!
//! ```text
//! cargo test --features import,export --test seek
//! ```

#![cfg(all(feature = "import", feature = "export"))]

use std::path::PathBuf;

use atome::export::FlacWriter;
use atome::import::{self, AudioStream, Section};

const FRAMES: u64 = 40_000;
const RATE: u32 = 48_000;

/// A scratch file that removes itself.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// What frame `n` holds: its own number on the left, folded into 16 bits, and
/// the negative on the right.
fn frame(n: u64) -> [i16; 2] {
    let left = (n % 30_000) as i16 - 15_000;
    [left, -left]
}

/// A 16-bit stereo FLAC of `FRAMES` frames, each saying which it is.
fn ramp(name: &str) -> Scratch {
    let file = Scratch(std::env::temp_dir().join(format!("atome-{}-{name}.flac", std::process::id())));
    let samples: Vec<i32> = (0..FRAMES).flat_map(|n| frame(n).map(i32::from)).collect();
    let mut writer = FlacWriter::create(&file.0, RATE, 2, 16).unwrap();
    writer.write(&samples).unwrap();
    writer.finish().unwrap();
    file
}

/// The next `count` frames out of `stream`.
fn frames(stream: &mut dyn AudioStream<i16>, count: usize) -> Vec<[i16; 2]> {
    let mut got = Vec::new();
    // An odd size, so reads straddle the decoder's blocks.
    let mut block = vec![0_i16; 2 * 777];
    while got.len() < count {
        let read = stream.read(&mut block).unwrap();
        if read == 0 {
            break;
        }
        got.extend(block[..read].chunks(2).map(|pair| [pair[0], pair[1]]));
    }
    got.truncate(count);
    got
}

#[test]
fn a_seek_lands_on_the_frame_asked_for() {
    let file = ramp("seek");
    let mut stream = import::stream::<i16>(&file.0).unwrap();

    for target in [12_345, 0, 4_095, 4_096, 39_990, 1] {
        assert_eq!(stream.seek(target).unwrap(), target);
        let expected: Vec<[i16; 2]> = (target..(target + 50).min(FRAMES)).map(frame).collect();
        assert_eq!(frames(stream.as_mut(), 50), expected, "after a seek to {target}");
    }
}

#[test]
fn a_seek_past_the_end_is_the_end() {
    let file = ramp("past");
    let mut stream = import::stream::<i16>(&file.0).unwrap();
    assert_eq!(stream.seek(FRAMES + 10).unwrap(), FRAMES);
    assert!(frames(stream.as_mut(), 10).is_empty());
}

#[test]
fn a_looping_section_of_a_file_runs_round_with_no_gap() {
    let file = ramp("loop");
    let stream = import::stream::<i16>(&file.0).unwrap();
    let mut section = Section::new(stream, 1_000, Some(3_500), true).unwrap();

    let got = frames(&mut section, 2_500 * 3 + 100);
    let expected: Vec<[i16; 2]> = (0..got.len() as u64).map(|at| frame(1_000 + at % 2_500)).collect();
    assert_eq!(got, expected);
    assert_eq!(section.passes(), 3);
}

#[test]
fn a_section_from_a_reader_starts_at_its_in_point() {
    let file = ramp("reader");
    let reader = std::fs::File::open(&file.0).unwrap();
    let stream = import::stream_reader::<i16>(Box::new(reader), "ramp").unwrap();
    let mut section = Section::new(stream, 20_000, None, false).unwrap();
    assert_eq!(section.total_frames(), Some(FRAMES - 20_000));

    let got = frames(&mut section, FRAMES as usize);
    assert_eq!(got.len() as u64, FRAMES - 20_000);
    assert_eq!(got[0], frame(20_000));
    assert_eq!(*got.last().unwrap(), frame(FRAMES - 1));
}

/// Symphonia's FLAC reader, seeking back onto a frame boundary, used to hand
/// out what it had read ahead after the first packet; every packet after a
/// seek must follow on from the one before.
#[test]
fn after_a_seek_back_every_packet_follows_on() {
    let file = ramp("back");
    for (first, then) in [(4_095, 4_096), (9_000, 4_096), (30_000, 0), (12_345, 8_192)] {
        let mut stream = import::stream::<i16>(&file.0).unwrap();
        stream.seek(first).unwrap();
        frames(stream.as_mut(), 50);

        stream.seek(then).unwrap();
        let expected: Vec<[i16; 2]> = (then..then + 10_000).map(frame).collect();
        assert_eq!(frames(stream.as_mut(), 10_000), expected, "{first}, then back to {then}");
    }
}
