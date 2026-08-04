//! Plugins at all three levels, and what each one hears.
//!
//! The same `Plugin` type attaches in three places, and where it attaches is
//! the whole distinction:
//!
//! | Attached to | Hears |
//! |---|---|
//! | an input's device | that input alone, before routing |
//! | the engine | everything |
//! | an output's device | what that device plays, after mixing |
//!
//! The plugins here are [internal ones](atome::plugins::internal) — Rust
//! functions compiled in — because that needs no plugin installed on the
//! machine and no Cargo feature. Each one multiplies the buffer by a distinct
//! gain, so the number that comes out the far end says which chains ran and in
//! what order, which is the point worth settling first: it decides whether a
//! compressor on an input is heard once or once per destination.
//!
//! ```sh
//! make example4
//! ```

use atome::device::AtomeDevice;
use atome::output::SampleRate;
use atome::{input, output, AudioEngine, Plugin};

mod common;

/// A plugin that scales the buffer, named so its position can be seen.
///
/// Distinct gains rather than one shared gain: multiplied together they give a
/// different answer for every subset of the chain, so the output identifies
/// which plugins ran rather than merely how many.
fn plugin(name: &str, gain: f32) -> Plugin {
    Plugin::internal(name, move |buffer: &mut [f32], _channels| {
        for sample in buffer {
            *sample *= gain;
        }
    })
}

fn main() -> Result<(), cpal::Error> {
    let inputs = input::list_devices(None)?;
    let outputs = output::list_devices(None)?;

    if inputs.is_empty() || outputs.len() < 2 {
        eprintln!(
            "needs one input and two outputs; this machine has {} and {}",
            inputs.len(),
            outputs.len()
        );
        return Ok(());
    }

    let mut engine = AudioEngine::<f32>::new(
        vec![AtomeDevice::input(inputs[0].clone(), common::host())
            .with_plugin(plugin("gate (input only)", 0.5))],
        vec![
            AtomeDevice::output(outputs[0].clone(), common::host())
                .with_plugin(plugin("room EQ (output 0 only)", 0.25)),
            AtomeDevice::output(outputs[1].clone(), common::host()),
        ],
        SampleRate::Hz48k,
        vec![2, 2],
        Some(512),
        // Handed to the engine directly, so it hears everything.
        vec![plugin("limiter (everything)", 0.1)],
    )?;

    common::describe(&engine);

    // Route by route, from the same starting sample each time, so the two
    // numbers can be compared against each other.
    let mut buffer = vec![1.0f32; 512];
    engine.apply_plugins(0, 0, &mut buffer, 2)?;
    println!("\ninput 0 -> output 0: gate -> limiter -> room EQ");
    println!("  1.0 * 0.5 * 0.1 * 0.25 = {}", buffer[0]);

    let mut buffer = vec![1.0f32; 512];
    engine.apply_plugins(0, 1, &mut buffer, 2)?;
    println!("\ninput 0 -> output 1: gate -> limiter (output 1 has none of its own)");
    println!("  1.0 * 0.5 * 0.1        = {}", buffer[0]);

    // Each level can also be run on its own, which is how the engine will use
    // them: the input's chain runs once, before the audio is copied to each
    // destination, so a plugin on an input is heard once however many outputs
    // that input reaches.
    let mut buffer = vec![1.0f32; 512];
    engine.apply_input_plugins(0, &mut buffer)?;
    engine.apply_engine_plugins(&mut buffer, 2)?;
    println!("\nrun a level at a time, up to the split:");
    println!("  after gate and limiter = {}", buffer[0]);

    let mut to_output_0 = buffer.clone();
    engine.apply_output_plugins(0, &mut to_output_0)?;
    println!("  then output 0's own    = {}", to_output_0[0]);
    println!("  then output 1's own    = {} (it has none)", buffer[0]);

    common::note_no_audio();
    Ok(())
}
