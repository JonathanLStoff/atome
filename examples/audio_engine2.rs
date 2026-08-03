//! One input tied to one specific output.
//!
//! Two outputs exist; the input names only the second, so its audio reaches
//! that device and not the other. This is the case example 1 does not show —
//! there, with nothing named, the input fed everything.
//!
//! A talkback microphone is the usual reason: it has to reach the monitors
//! without also reaching the main mix.
//!
//! ```sh
//! make example2
//! ```

use atome::device::AtomeDevice;
use atome::output::SampleRate;
use atome::{input, output, AudioEngine};

mod common;

fn main() -> Result<(), cpal::Error> {
    let inputs = input::list_devices(None)?;
    let outputs = output::list_devices(None)?;

    let Some(input) = inputs.first() else {
        eprintln!("no input devices on this machine");
        return Ok(());
    };
    if outputs.len() < 2 {
        eprintln!(
            "needs two output devices to show routing; this machine has {}",
            outputs.len()
        );
        return Ok(());
    }

    // The device the input will be tied to. Routing is by name, so this is the
    // same string the engine matches against.
    let tied_to = common::name_of(&outputs[1]);
    println!("tying the input to {tied_to:?}, leaving {:?} alone", common::name_of(&outputs[0]));

    let engine = AudioEngine::<f32>::new(
        vec![AtomeDevice::input(input.clone(), common::host()).route_to([tied_to.clone()])],
        outputs
            .iter()
            .take(2)
            .map(|device| AtomeDevice::output(device.clone(), common::host()))
            .collect(),
        SampleRate::Hz48k,
        vec![2, 2],
        Some(512),
        vec![],
    )?;

    common::describe(&engine);

    let routes = engine.inputs()[0].routes();
    println!("\nroutes to indices {routes:?} — output 0 is not among them");

    // Routing is resolved to indices once, at construction, so a name that
    // matches nothing fails here rather than going silently nowhere later.
    let mistake = AudioEngine::<f32>::new(
        vec![AtomeDevice::input(input.clone(), common::host()).route_to(["Speakers That Are Not Here"])],
        vec![AtomeDevice::output(outputs[0].clone(), common::host())],
        SampleRate::Hz48k,
        vec![2],
        Some(512),
        vec![],
    );

    match mistake {
        Ok(_) => println!("\n(expected the bad route to be refused, but it was not)"),
        Err(error) => println!("\na route to a device that is not there is refused:\n  {error}"),
    }

    common::note_no_audio();
    Ok(())
}
