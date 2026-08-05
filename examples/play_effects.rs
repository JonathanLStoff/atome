//! A file, through a plugin chain, out to a device. **Makes sound.**
//!
//! ```text
//! import::decode -> align to the device -> plugin chain -> output -> speakers
//! ```
//!
//! The whole path, end to end, and audible. [`play_file`](../play_file.rs)
//! plays a file untouched; this one puts a chain of [atome's own
//! effects](atome::plugins::atome) between the decoder and the device so you
//! can hear it do something.
//!
//! # Two ways to build a chain
//!
//! By calling the constructors, which is what you want when the chain is known
//! when the code is written:
//!
//! ```text
//! atome::compressor(48_000).with_params("ratio: 4")?
//! ```
//!
//! Or by name and parameter string, which is what a chain read from a
//! configuration file looks like — `CHAIN` below is that form, and the two
//! produce the same plugins.
//!
//! # Why the chain runs block by block
//!
//! The decoded file is in memory all at once, so the plugins *could* be handed
//! the whole thing in one call. They are given 1024 frames at a time instead,
//! because that is the shape a live stream has — and it is what shows that a
//! stateful plugin keeps its state across block boundaries. A filter whose
//! history reset every block would click 40 times a second, and processing in
//! one go would hide that.
//!
//! Needs the `import` feature, which is what pulls in the decoders.
//!
//! ```sh
//! make example-effects
//! make example-effects FILE=~/Music/something.flac
//! ```

use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use atome::device::AtomeDevice;
use atome::output::SampleRate;
use atome::plugins::{atome as effects, Params};
use atome::{import, output, AudioEngine, Plugin};
use cpal::traits::StreamTrait;

mod common;

/// The sample type the output runs in.
type Sample = f32;

/// Frames per block. Not the device's buffer size — the point is only that the
/// chain sees the audio in pieces, the way it will in a live stream.
const BLOCK_FRAMES: usize = 1024;

/// The chain, as a configuration file would carry it: a name and a parameter
/// string per effect.
///
/// Nothing here is Rust-specific — the same two columns could be rows in a
/// TOML file or a database, which is the point of building plugins by name.
const CHAIN: &[(&str, &str)] = &[
    // Roll the top off, so the tremolo has something dark to chew on.
    ("filter", "cutoff_hz: 1800, resonance: 1.4"),
    // Even out what is left before modulating it, or the loud parts modulate
    // further than the quiet ones and it sounds uneven.
    ("compressor", "threshold_db: -24, ratio: 4, attack_ms: 5, release_ms: 80"),
    ("tremolo", "rate_hz: 4, depth: 0.7"),
    // A short slap, wide enough to hear but not so long it smears the tremolo.
    ("delay", "time_ms: 180, feedback: 0.3, mix: 0.25"),
    // Last, so nothing after it can push the level back over.
    ("limiter", "ceiling_db: -3, release_ms: 40"),
];

/// Builds the chain above for a sample rate.
fn chain(sample_rate: u32) -> Result<Vec<Plugin>, Box<dyn std::error::Error>> {
    CHAIN
        .iter()
        .map(|(kind, params)| {
            let params = Params::parse(params)?;
            Ok(effects::create(kind, sample_rate, &params)?)
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("test_data")
            .join("test415hz.mp3")
    });

    if !path.exists() {
        eprintln!("no such file: {}", path.display());
        return Ok(());
    }

    let decoded = import::decode(&path)?;
    println!("file:        {}", path.display());
    println!("encoding:    {:?}", import::find_type(&path)?);
    println!("sample rate: {} Hz", decoded.sample_rate);
    println!("channels:    {}", decoded.channels);
    println!("length:      {:.2} s", decoded.duration());

    let outputs = output::list_devices(None)?;
    let Some(device) = outputs.first() else {
        eprintln!("no output devices on this machine");
        return Ok(());
    };

    let rate = SampleRate::from_hz(decoded.sample_rate).unwrap_or(SampleRate::Hz48k);

    // The chain hangs off the output device, so it hears exactly what that
    // device is about to play — after any rate and channel alignment, and
    // after everything else in the engine.
    let mut engine = AudioEngine::<Sample>::new(
        vec![],
        vec![AtomeDevice::output(device.clone(), common::host())
            .with_plugins(chain(rate.hz() as u32)?)],
        rate,
        vec![decoded.channels],
        None,
        vec![],
    )?;

    common::describe(&engine);

    println!("\nchain:");
    for plugin in engine.outputs()[0].device().plugins() {
        println!("  {:<12} {}", plugin.name, plugin.params());
    }

    // Align first, process second. The output's chain is meant to hear what
    // the device will play, and that is the aligned audio — a plugin told it
    // has two channels should be handed two.
    let output = engine.outputs_mut()[0].output_mut();
    let mut samples = output.align_samples(
        &decoded.samples.to_vec::<Sample>(),
        SampleRate::from_hz(decoded.sample_rate).unwrap_or(rate),
        decoded.channels,
        true,
    )?;

    let channels = output.channels().max(1) as usize;
    let sample_rate = output.sample_rate() as f64;

    println!("\nrunning the chain over {} blocks", samples.len().div_ceil(BLOCK_FRAMES * channels));
    for block in samples.chunks_mut(BLOCK_FRAMES * channels) {
        engine.apply_output_plugins(0, block)?;
    }

    let output = engine.outputs_mut()[0].output_mut();
    let stream = output.build_stream()?;
    stream.play()?;
    output.add_samples(&samples, 0)?;

    let seconds = samples.len() as f64 / sample_rate / channels as f64;
    println!("playing {seconds:.2} s through {}", common::name_of(device));

    thread::sleep(Duration::from_secs_f64(seconds) + Duration::from_millis(500));

    engine.outputs_mut()[0].output_mut().stop();
    println!("done");

    Ok(())
}
