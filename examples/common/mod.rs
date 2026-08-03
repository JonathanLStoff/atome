//! Shared helpers for the examples.
//!
//! Not part of the crate's API — this is a module the examples pull in with
//! `mod common;`, so it lives in `examples/common/` rather than `src/`.
//!
//! Every example compiles the whole module but uses only part of it, which is
//! what the allow is for: the alternative is one helper file per example.

#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use atome::output::{device_name, OutputType};
use atome::{AudioEngine, SampleType};

/// The host API to tag devices with on this platform.
///
/// The tag is descriptive — cpal has already decided which backend is in use —
/// so this only has to be right, not clever.
pub fn host() -> OutputType {
    if cfg!(target_os = "windows") {
        OutputType::WASAPI
    } else {
        OutputType::CoreAudio
    }
}

/// One item at random, or `None` if there are none.
///
/// The clock is the whole source of randomness: pulling in a random-number
/// generator to choose between three audio devices would be a dependency for
/// nothing.
pub fn pick<T>(items: &[T]) -> Option<&T> {
    if items.is_empty() {
        return None;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as usize)
        .unwrap_or(0);

    items.get(nanos % items.len())
}

/// Prints how an engine ended up wired.
pub fn describe<S: SampleType>(engine: &AudioEngine<S>) {
    println!("\nengine: {} Hz, buffer {:?}", engine.sample_rate().hz(), engine.buffer_size());

    if !engine.plugins().is_empty() {
        println!("  engine-wide plugins: {}", names(engine.plugins()));
    }

    println!("  outputs:");
    for (index, output) in engine.outputs().iter().enumerate() {
        print!(
            "    [{index}] {} — {} ch",
            output.device().name(),
            output.output().channels()
        );
        if output.device().plugins().is_empty() {
            println!();
        } else {
            println!(", plugins: {}", names(output.device().plugins()));
        }
    }

    println!("  inputs:");
    for (index, input) in engine.inputs().iter().enumerate() {
        let destinations: Vec<String> = input
            .routes()
            .iter()
            .map(|route| engine.outputs()[*route].device().name())
            .collect();

        print!(
            "    [{index}] {} — {} ch -> {}",
            input.device().name(),
            input.input().channels(),
            if destinations.is_empty() {
                "nothing".to_string()
            } else {
                destinations.join(", ")
            }
        );
        if input.device().plugins().is_empty() {
            println!();
        } else {
            println!(", plugins: {}", names(input.device().plugins()));
        }
    }
}

fn names(plugins: &[atome::Plugin]) -> String {
    plugins
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// Says plainly that nothing will be heard, and why.
///
/// Every engine example builds real streams over real devices, but carrying
/// captured audio from an input to its outputs is still unfinished — see
/// `planning/TODO.md` section 2.3. Printing this is better than leaving someone
/// to wonder whether their speakers are broken.
pub fn note_no_audio() {
    println!(
        "\nno audio will be heard: input-to-output forwarding is not implemented \
         yet (planning/TODO.md 2.3). This example shows the wiring."
    );
}

/// Names a device, for printing.
pub fn name_of(device: &cpal::Device) -> String {
    device_name(device)
}
