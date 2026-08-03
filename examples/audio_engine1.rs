//! Random input to random output.
//!
//! The smallest engine there is: one capture device, one playback device, no
//! routing and no plugins. Both are picked at random from whatever this machine
//! has, so on a laptop with several interfaces two runs give different
//! pairings — the point being that nothing in the engine cares which.
//!
//! ```sh
//! make example1
//! ```

use atome::device::AtomeDevice;
use atome::output::SampleRate;
use atome::{input, output, AudioEngine};

mod common;

fn main() -> Result<(), cpal::Error> {
    let inputs = input::list_devices(None)?;
    let outputs = output::list_devices(None)?;

    let Some(input) = common::pick(&inputs) else {
        eprintln!("no input devices on this machine");
        return Ok(());
    };
    let Some(output) = common::pick(&outputs) else {
        eprintln!("no output devices on this machine");
        return Ok(());
    };

    println!("picked at random:");
    println!("  in   {}", common::name_of(input));
    println!("  out  {}", common::name_of(output));

    let engine = AudioEngine::<f32>::new(
        vec![AtomeDevice::input(input.clone(), common::host())],
        vec![AtomeDevice::output(output.clone(), common::host())],
        SampleRate::Hz48k,
        // One count per output device, and there is one output.
        vec![2],
        Some(512),
        vec![],
    )?;

    common::describe(&engine);

    // Nothing named a route, so the input feeds every output — here, the one.
    println!(
        "\nunrouted input reaches every output: {:?}",
        engine.inputs()[0].routes()
    );

    common::note_no_audio();
    Ok(())
}
