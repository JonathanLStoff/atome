//! A microphone, through the engine's plugin chains, out to a device.
//! **Makes sound — wear headphones.**
//!
//! ```text
//! capture -> input chain -> channel align -> engine chain -> output chain -> speakers
//! ```
//!
//! # Read this before running it
//!
//! This monitors a microphone through a speaker. Without headphones that is a
//! feedback loop, and it will find the room's resonant frequency faster than
//! you can reach the keyboard. The chain below ends in a large attenuation for
//! that reason; it is not enough to make open speakers safe.
//!
//! # What the engine does not do yet, and what this does instead
//!
//! Carrying captured audio to an input's routed outputs is unfinished —
//! `planning/TODO.md` section 2.3 — so [`AudioEngine::new`] leaves every input
//! callback as a placeholder. This example supplies its own: the forwarding
//! here is the example's, not the engine's. Everything *else* is the engine's,
//! including all three levels of plugin chain.
//!
//! # Why the plugins do not run in the capture callback
//!
//! The callback copies its block and sends it. That is all it does. The chain
//! runs on the main thread, which is where the engine's own
//! `apply_*_plugins` methods have to be called from anyway — they take
//! `&mut AudioEngine`.
//!
//! That split is worth keeping even once forwarding lands. An internal plugin
//! allocates a scratch buffer per block, a hosted one may take a lock deep
//! inside a vendor's code, and neither belongs on the audio thread. The
//! `to_vec` in the callback is itself an allocation and the one thing here that
//! a real implementation would replace with a pre-allocated pool.
//!
//! # Why the chains are called one at a time
//!
//! [`AudioEngine::apply_plugins`] runs all three levels in one call, at one
//! channel count. A microphone is usually mono and a speaker pair is not, so
//! the channel alignment has to happen *between* the input's chain and the
//! output's — the input's plugins hear one channel, the output's hear two.
//! Calling the three levels separately is what makes room for it, and is the
//! clearest illustration of why they are three levels rather than one list.
//!
//! # Scheduling
//!
//! `add_samples` takes an absolute index from the start of the stream, and the
//! mixer's play cursor is not visible from outside — so this cannot ask where
//! playback has got to. It estimates instead: the first block goes a fixed
//! margin ahead of where the wall clock says the cursor should be, and every
//! block after it is appended contiguously.
//!
//! That is open-loop, and it drifts if the capture and playback clocks do not
//! agree. Over a run this short the drift is inaudible; over an hour it would
//! not be. Exposing the cursor through `MixerHandle` is what would close the
//! loop, and is noted against `add_samples_time` in the source.
//!
//! ```sh
//! make example-thru
//! make example-thru SECONDS=30
//! ```

use std::env;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atome::device::AtomeDevice;
use atome::{input, output, AudioEngine, Plugin};
use cpal::traits::StreamTrait;

mod common;

/// The sample type both streams run in.
type Sample = f32;

/// How far ahead of the estimated play cursor the first block is scheduled.
///
/// Everything after it is appended, so this is the whole of the added latency
/// and also the whole of the slack: too little and the first blocks land behind
/// the cursor and are dropped as late, too much and you hear yourself a beat
/// after you speak.
const PRE_ROLL: Duration = Duration::from_millis(120);

/// How long to run for, unless the command line says otherwise.
const DEFAULT_SECONDS: u64 = 15;

/// A fixed gain.
fn gain(db: f32) -> Plugin {
    let scale = 10.0_f32.powf(db / 20.0);

    Plugin::internal(format!("gain {db:+} dB"), move |buffer: &mut [f32], _| {
        for sample in buffer {
            *sample *= scale;
        }
    })
}

/// Soft clipping — a limiter's shape without a limiter's state.
///
/// `tanh` is bounded by ±1, so this cannot produce a sample that will clip the
/// device however loud the input gets. Stateless on purpose: this chain is the
/// one nearest the audio path, and a plugin with no state is a plugin with no
/// lock and nothing to reset.
fn soft_clip(drive: f32) -> Plugin {
    Plugin::internal(format!("soft clip x{drive}"), move |buffer: &mut [f32], _| {
        for sample in buffer {
            *sample = (*sample * drive).tanh();
        }
    })
}

fn main() -> Result<(), cpal::Error> {
    let seconds = env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECONDS);

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

    // One rate for both streams, chosen from what the two devices agree on.
    let rate = common::shared_rate(source, sink);

    // One plugin chain per level, so the printout below says which audio each
    // one heard rather than merely that they ran.
    let mut engine = AudioEngine::<Sample>::new(
        vec![AtomeDevice::input(source.clone(), common::host())
            .with_plugin(soft_clip(1.5))],
        vec![AtomeDevice::output(sink.clone(), common::host()).with_plugin(gain(-24.0))],
        rate,
        vec![2],
        Some(512),
        vec![gain(-6.0)],
    )?;

    common::describe(&engine);

    let in_channels = engine.inputs()[0].input().channels();
    let out_channels = engine.outputs()[0].output().channels().max(1);

    // The capture callback's only job. A bounded queue would be the right
    // shape for a real one — an unbounded channel turns a stalled consumer
    // into unbounded memory growth rather than into dropped audio, and dropped
    // audio is the better failure.
    let (blocks, captured) = mpsc::channel::<Vec<Sample>>();

    {
        let input = engine.inputs_mut()[0].input_mut();
        input.set_callback(move |block: &[Sample]| {
            // A send failure means the main loop has finished and dropped the
            // receiver. Nothing to report from in here.
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

    println!(
        "\nmonitoring {} -> {} for {seconds} s",
        common::name_of(source),
        common::name_of(sink)
    );
    println!("wear headphones — this is a microphone feeding a speaker\n");

    let deadline = started + Duration::from_secs(seconds);
    let mut index: Option<usize> = None;
    let mut forwarded = 0usize;
    let mut blocks_sent = 0usize;
    let mut refused = 0usize;

    while Instant::now() < deadline {
        // A timeout rather than a blocking receive, so the run ends on time
        // even if capture has stopped delivering.
        let Ok(mut block) = captured.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };

        // The input's own chain, at the input's own channel count — before
        // anything has been mixed or mapped.
        engine.apply_input_plugins(0, &mut block)?;

        // Mono microphone to a stereo pair. The rate is already the engine's
        // on both sides, so this maps channels and nothing else.
        let mut block = engine.outputs()[0]
            .output()
            .align_samples(&block, rate, in_channels, true)?;

        // Then the two chains that belong to the destination side, at the
        // destination's channel count.
        engine.apply_engine_plugins(&mut block, out_channels)?;
        engine.apply_output_plugins(0, &mut block)?;

        // First block: start a margin ahead of wherever the cursor has got to
        // by now. Every one after it is appended.
        let at = *index.get_or_insert_with(|| {
            let ahead = started.elapsed() + PRE_ROLL;
            let frames = (ahead.as_secs_f64() * rate.hz() as f64) as usize;
            frames * out_channels as usize
        });

        match engine.outputs_mut()[0].output_mut().add_samples(&block, at) {
            Ok(next) => {
                index = Some(next);
                forwarded += block.len();
                blocks_sent += 1;
            }
            // The mixer's command queue is full, which means it is not
            // draining. Dropping the block keeps the loop in step; keeping it
            // would only make the next one later still.
            Err(_) => refused += 1,
        }
    }

    engine.outputs_mut()[0].output_mut().stop();

    let played = forwarded as f64 / rate.hz() as f64 / out_channels as f64;
    println!("forwarded {played:.2} s of audio in {blocks_sent} blocks");
    if refused > 0 {
        println!("dropped {refused} blocks: the mixer queue was full");
    }
    println!("done");

    Ok(())
}
