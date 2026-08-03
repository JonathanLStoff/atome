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
//! `Plugin::apply` is a stub — no format backend exists yet — so this shows the
//! ordering rather than any processing. That ordering is worth settling first:
//! it is what decides whether a compressor on an input is heard once or once
//! per destination.
//!
//! ```sh
//! make example4
//! ```

use std::path::PathBuf;

use atome::device::AtomeDevice;
use atome::output::SampleRate;
use atome::{input, output, AudioEngine, Plugin};

mod common;

/// A plugin that does nothing, named so its position can be seen.
fn plugin(name: &str) -> Plugin {
    Plugin {
        name: name.to_string(),
        path: PathBuf::from("/not/a/real/plugin"),
        buffer_size: 512,
        sample_rate: 48_000,
        channels: 2,
        params: String::new(),
    }
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
            .with_plugin(plugin("gate (input only)"))],
        vec![
            AtomeDevice::output(outputs[0].clone(), common::host())
                .with_plugin(plugin("room EQ (output 0 only)")),
            AtomeDevice::output(outputs[1].clone(), common::host()),
        ],
        SampleRate::Hz48k,
        vec![2, 2],
        Some(512),
        // Handed to the engine directly, so it hears everything.
        vec![plugin("limiter (everything)")],
    )?;

    common::describe(&engine);

    // A buffer to run the chains over. The stubs pass it through untouched,
    // which is exactly what a plugin with no backend should do.
    let mut buffer = vec![0.25f32; 512];

    println!("\napplying the chain for input 0 -> output 0:");
    engine.apply_plugins(0, 0, &mut buffer, 2)?;
    println!("  gate -> limiter -> room EQ");

    println!("\napplying the chain for input 0 -> output 1:");
    engine.apply_plugins(0, 1, &mut buffer, 2)?;
    println!("  gate -> limiter          (output 1 has no plugins of its own)");

    let untouched = buffer.iter().all(|sample| *sample == 0.25);
    println!("\nbuffer unchanged by the stubs: {untouched}");

    // Each level can also be run on its own, which is how the engine will use
    // them: the input's chain runs once, before the audio is copied to each
    // destination.
    engine.apply_input_plugins(0, &mut buffer)?;
    engine.apply_engine_plugins(&mut buffer, 2)?;
    engine.apply_output_plugins(1, &mut buffer)?;

    common::note_no_audio();
    Ok(())
}
