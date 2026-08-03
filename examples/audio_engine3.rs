//! Several outputs with different channel counts, and partial routing.
//!
//! This is why `output_channels` is a list rather than one number: a stereo
//! pair and a surround rig on the same engine need different counts, and
//! pairing them off by position is checked rather than assumed.
//!
//! Two inputs, wired differently — one to everything, one to a single
//! destination — so the two routing behaviours appear side by side.
//!
//! ```sh
//! make example3
//! ```

use atome::device::AtomeDevice;
use atome::output::SampleRate;
use atome::{input, output, AudioEngine};

mod common;

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

    let first_output = common::name_of(&outputs[0]);

    // Two counts for two devices: stereo, then five channels. A real 5.1 rig
    // would have to be one; here it only has to be asked for.
    let channels = vec![2u16, 5];
    println!("output channel counts: {channels:?}");

    let engine = AudioEngine::<f32>::new(
        vec![
            // Feeds everything, because it names nothing.
            AtomeDevice::input(inputs[0].clone(), common::host()),
            // Feeds only the first output.
            AtomeDevice::input(
                inputs.get(1).unwrap_or(&inputs[0]).clone(),
                common::host(),
            )
            .route_to([first_output.clone()]),
        ],
        outputs
            .iter()
            .take(2)
            .map(|device| AtomeDevice::output(device.clone(), common::host()))
            .collect(),
        SampleRate::Hz48k,
        channels,
        Some(512),
        vec![],
    )?;

    common::describe(&engine);

    println!("\ninput 0 routes to {:?} (all of them)", engine.inputs()[0].routes());
    println!("input 1 routes to {:?} (only {first_output:?})", engine.inputs()[1].routes());

    // The check that makes the list worth having: a count list of the wrong
    // length is refused rather than silently pairing off what it can.
    let mismatch = AudioEngine::<f32>::new(
        vec![],
        outputs
            .iter()
            .take(2)
            .map(|device| AtomeDevice::output(device.clone(), common::host()))
            .collect(),
        SampleRate::Hz48k,
        vec![2],
        Some(512),
        vec![],
    );

    match mismatch {
        Ok(_) => println!("\n(expected the short channel list to be refused, but it was not)"),
        Err(error) => println!("\ntwo outputs and one channel count is refused:\n  {error}"),
    }

    common::note_no_audio();
    Ok(())
}
