//! A file and a microphone, each through its own plugins, into one output.
//! **Makes sound — wear headphones.**
//!
//! ```text
//! import::decode -> file chain  ---.
//!                                   >-- mixer -> output -> speakers
//! capture -> input chain ----------'
//! ```
//!
//! [`play_effects`](../play_effects.rs) shows a file reaching a device and
//! [`live_thru`](../live_thru.rs) shows a microphone reaching one. This shows
//! both reaching the *same* device at the same time, which is the part neither
//! of them exercises: two independent producers calling
//! [`add_samples`](atome::OutputClass::add_samples) over overlapping index
//! ranges, and the mixer summing them.
//!
//! # Read this before running it
//!
//! It monitors a microphone through a speaker. Wear headphones — see
//! [`live_thru`](../live_thru.rs), which explains the feedback risk at length.
//!
//! # Why there is no chain on the output device
//!
//! An output's chain is meant to hear what that device is about to play, which
//! for two sources means hearing them *mixed*. The summing happens in the
//! mixer, on the far side of `add_samples`, so from out here there is no point
//! at which the mixed signal exists to be processed — the last thing this
//! example can reach is each source separately.
//!
//! Applying the output chain to both sources instead would be a different
//! thing wearing the same name. For a gain it makes no audible difference; for
//! anything non-linear — a compressor, the soft clip in `live_thru` — it very
//! much does, because two signals clipped apart and summed is not two signals
//! summed and clipped. So this example does not pretend: each source gets its
//! own chain, and the post-mix level is left empty until the engine owns the
//! mixing itself (`planning/TODO.md` section 2.3).
//!
//! Needs the `import` feature.
//!
//! ```sh
//! make example-mix
//! make example-mix FILE=~/Music/something.flac
//! ```

use std::env;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atome::device::AtomeDevice;
use atome::{import, input, output, AudioEngine, Plugin};
use cpal::traits::StreamTrait;

mod common;

/// The sample type every stream here runs in.
type Sample = f32;

/// How far ahead of the estimated play cursor the first samples are scheduled.
///
/// Both sources use it, and both measure it from the same instant, which is
/// what makes them land on top of each other rather than one after the other.
const PRE_ROLL: Duration = Duration::from_millis(150);

/// A fixed gain.
fn gain(name: &str, db: f32) -> Plugin {
    let scale = 10.0_f32.powf(db / 20.0);

    Plugin::internal(format!("{name} {db:+} dB"), move |buffer: &mut [f32], _| {
        for sample in buffer {
            *sample *= scale;
        }
    })
}

/// Keeps the first channel and silences the rest.
///
/// Puts the file hard left so the two sources are told apart by ear rather
/// than by trusting the printout.
fn hard_left() -> Plugin {
    Plugin::internal("hard left", |buffer: &mut [f32], channels| {
        for frame in buffer.chunks_mut(channels as usize) {
            for sample in frame.iter_mut().skip(1) {
                *sample = 0.0;
            }
        }
    })
}

/// Keeps the last channel and silences the rest.
fn hard_right() -> Plugin {
    Plugin::internal("hard right", |buffer: &mut [f32], channels| {
        let channels = channels as usize;
        for frame in buffer.chunks_mut(channels) {
            for sample in frame.iter_mut().take(channels.saturating_sub(1)) {
                *sample = 0.0;
            }
        }
    })
}

fn main() -> Result<(), cpal::Error> {
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

    let inputs = input::list_devices(None)?;
    let outputs = output::list_devices(None)?;

    let (Some(source), Some(sink)) = (inputs.first(), outputs.first()) else {
        eprintln!(
            "needs one input and one output; this machine has {} and {}",
            inputs.len(),
            outputs.len()
        );
        return Ok(());
    };

    let decoded = import::decode(&path)?;
    println!("file:        {}", path.display());
    println!("encoding:    {:?}", import::find_type(&path)?);
    println!("sample rate: {} Hz", decoded.sample_rate);
    println!("channels:    {}", decoded.channels);
    println!("length:      {:.2} s", decoded.duration());

    let rate = common::shared_rate(source, sink);

    // The microphone's chain hangs off its device, so the engine applies it.
    // The file has no device to hang a chain off — it is not a capture stream —
    // so its chain is held here and applied by hand, which is the shape of
    // every source that is not a device.
    let mut engine = AudioEngine::<Sample>::new(
        vec![AtomeDevice::input(source.clone(), common::host())
            .with_plugin(hard_right())
            .with_plugin(gain("mic", -20.0))],
        vec![AtomeDevice::output(sink.clone(), common::host())],
        rate,
        vec![2],
        Some(512),
        vec![],
    )?;

    let mut file_chain = vec![hard_left(), gain("file", -9.0)];

    common::describe(&engine);
    println!(
        "  file chain: {}",
        file_chain
            .iter()
            .map(|plugin| plugin.name.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    );

    let in_channels = engine.inputs()[0].input().channels();
    let out_channels = engine.outputs()[0].output().channels().max(1);

    // --- the file, prepared up front -------------------------------------

    let output = engine.outputs()[0].output();
    let mut file = output.align_samples(
        &decoded.samples.to_vec::<Sample>(),
        atome::output::SampleRate::from_hz(decoded.sample_rate).unwrap_or(rate),
        decoded.channels,
        true,
    )?;

    for block in file.chunks_mut(1024 * out_channels as usize) {
        for plugin in &mut file_chain {
            plugin.apply(block, out_channels)?;
        }
    }

    // --- the microphone, block by block ----------------------------------

    let (blocks, captured) = mpsc::channel::<Vec<Sample>>();

    {
        let input = engine.inputs_mut()[0].input_mut();
        input.set_callback(move |block: &[Sample]| {
            let _ = blocks.send(block.to_vec());
        });

        match input.build_stream() {
            Ok(stream) => stream.play()?,
            Err(error) => {
                eprintln!("cannot capture from {}: {error}", common::name_of(source));
                return Ok(());
            }
        }
    }

    let started = {
        let output = engine.outputs_mut()[0].output_mut();
        let stream = output.build_stream()?;
        stream.play()?;
        Instant::now()
    };

    // One instant, two schedules. The file goes down in a single command at
    // the pre-roll; the live blocks are appended from the same index, so they
    // land on top of it and the mixer sums the two.
    let start = {
        let ahead = started.elapsed() + PRE_ROLL;
        (ahead.as_secs_f64() * rate.hz() as f64) as usize * out_channels as usize
    };

    engine.outputs_mut()[0].output_mut().add_samples(&file, start)?;

    let seconds = file.len() as f64 / rate.hz() as f64 / out_channels as f64;
    println!(
        "\nplaying {seconds:.2} s of {} (left) under {} (right)",
        path.file_name().unwrap_or_default().to_string_lossy(),
        common::name_of(source)
    );
    println!("wear headphones — this is a microphone feeding a speaker\n");

    let deadline = started + Duration::from_secs_f64(seconds) + PRE_ROLL;
    let mut index = start;
    let mut refused = 0usize;

    while Instant::now() < deadline {
        let Ok(mut block) = captured.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };

        // The input's own chain, at the input's own channel count.
        engine.apply_input_plugins(0, &mut block)?;

        // Then up to the output's channel count, so it sums with the file.
        let block = engine.outputs()[0]
            .output()
            .align_samples(&block, rate, in_channels, true)?;

        match engine.outputs_mut()[0].output_mut().add_samples(&block, index) {
            Ok(next) => index = next,
            Err(_) => refused += 1,
        }
    }

    engine.outputs_mut()[0].output_mut().stop();

    if refused > 0 {
        println!("dropped {refused} live blocks: the mixer queue was full");
    }
    println!("done");

    Ok(())
}
