//! Engine construction: device validation, routing resolution, plugin reach.
//!
//! No audio is played. Everything here is about what the engine accepts and how
//! it wires what it accepted together.

use std::path::PathBuf;

use atome::device::{AtomeDevice, Direction};
use atome::output::{OutputType, SampleRate};
use atome::plugins::PluginFormat;
use atome::{AudioEngine, Plugin};

fn host() -> OutputType {
    if cfg!(target_os = "windows") { OutputType::WASAPI } else { OutputType::CoreAudio }
}

/// A plugin descriptor that is never loaded.
///
/// These tests are about the engine's wiring, not about hosting: an unloaded
/// plugin passes audio through, which is all a reach test needs it to do.
fn plugin(name: &str) -> Plugin {
    Plugin::new(
        name.to_string(),
        PathBuf::from("/nonexistent"),
        512,
        48_000,
        2,
        String::new(),
        PluginFormat::Internal,
    )
}

/// Two outputs and one input, or `None` where the machine has no devices.
fn devices() -> Option<(AtomeDevice, Vec<AtomeDevice>)> {
    let input = AtomeDevice::default_input(host())?;
    let outputs = atome::output::list_devices(None).ok()?;
    if outputs.len() < 2 {
        return None;
    }

    Some((
        input,
        outputs
            .into_iter()
            .take(2)
            .map(|device| AtomeDevice::output(device, host()))
            .collect(),
    ))
}

fn engine(
    inputs: Vec<AtomeDevice>,
    outputs: Vec<AtomeDevice>,
    channels: Vec<u16>,
    plugins: Vec<Plugin>,
) -> Result<AudioEngine<f32>, cpal::Error> {
    AudioEngine::new(inputs, outputs, SampleRate::Hz48k, channels, Some(512), plugins)
}

#[test]
fn a_channel_count_per_output_is_required() {
    let Some((input, outputs)) = devices() else {
        eprintln!("skipping: needs an input and two outputs");
        return;
    };

    // Two outputs, one channel count: pairing them off by position would give
    // the second device whatever the first got.
    let error = engine(vec![input], outputs, vec![2], vec![]).unwrap_err();
    let message = error.message().unwrap_or_default();
    assert!(message.contains("2 output devices but 1 channel counts"), "{message}");
}

#[test]
fn per_output_channel_counts_are_kept_apart() {
    let Some((input, outputs)) = devices() else { return; };

    let engine = engine(vec![input], outputs, vec![2, 5], vec![]).expect("build");

    assert_eq!(engine.outputs()[0].output().channels(), 2);
    assert_eq!(engine.outputs()[1].output().channels(), 5);
}

#[test]
fn a_device_facing_the_wrong_way_is_refused() {
    let Some((input, outputs)) = devices() else { return; };

    // An output listed as an input.
    let wrong = engine(vec![outputs[0].clone()], outputs.clone(), vec![2, 2], vec![]);
    let message = wrong.unwrap_err().message().unwrap_or_default().to_string();
    assert!(message.contains("listed as an input"), "{message}");

    // And an input listed as an output.
    let wrong = engine(vec![input.clone()], vec![input], vec![2], vec![]);
    let message = wrong.unwrap_err().message().unwrap_or_default().to_string();
    assert!(message.contains("listed as an output"), "{message}");
}

#[test]
fn an_unrouted_input_feeds_every_output() {
    let Some((input, outputs)) = devices() else { return; };

    let engine = engine(vec![input], outputs, vec![2, 2], vec![]).expect("build");

    assert_eq!(engine.inputs()[0].routes(), &[0, 1]);
}

#[test]
fn a_routed_input_feeds_only_what_it_names() {
    let Some((input, outputs)) = devices() else { return; };

    let second = outputs[1].name();
    let input = input.route_to([second]);

    let engine = engine(vec![input], outputs, vec![2, 2], vec![]).expect("build");

    assert_eq!(engine.inputs()[0].routes(), &[1], "should reach only the named output");
}

#[test]
fn routing_to_a_name_that_is_not_there_fails_at_construction() {
    let Some((input, outputs)) = devices() else { return; };

    let input = input.route_to(["Nonexistent Monitors"]);
    let error = engine(vec![input], outputs, vec![2, 2], vec![]).unwrap_err();

    let message = error.message().unwrap_or_default();
    assert!(message.contains("Nonexistent Monitors"), "{message}");
    assert!(message.contains("not one of the outputs"), "{message}");
}

#[test]
fn plugins_attach_at_the_level_they_were_given() {
    let Some((input, outputs)) = devices() else { return; };

    let input = input.with_plugin(plugin("on the input"));
    let outputs = vec![
        outputs[0].clone().with_plugin(plugin("on output 0")),
        outputs[1].clone(),
    ];

    let engine = engine(vec![input], outputs, vec![2, 2], vec![plugin("engine wide")])
        .expect("build");

    assert_eq!(engine.plugins().len(), 1, "engine-wide chain");
    assert_eq!(engine.inputs()[0].device().plugins().len(), 1, "input chain");
    assert_eq!(engine.outputs()[0].device().plugins().len(), 1, "output 0 chain");
    assert_eq!(engine.outputs()[1].device().plugins().len(), 0, "output 1 has none");

    // The reach is the point: an output's plugin is not the engine's, and vice
    // versa.
    assert_eq!(engine.plugins()[0].name, "engine wide");
    assert_eq!(engine.outputs()[0].device().plugins()[0].name, "on output 0");
}

#[test]
fn every_chain_is_reached_and_indices_are_checked() {
    let Some((input, outputs)) = devices() else { return; };

    let mut engine = engine(vec![input], outputs, vec![2, 2], vec![plugin("engine")])
        .expect("build");

    let mut buffer = vec![0.25f32; 256];

    // The full chain runs.
    engine.apply_plugins(0, 1, &mut buffer, 2).expect("apply all three levels");

    // A stub must pass audio through untouched, not zero it.
    assert!(buffer.iter().all(|s| *s == 0.25), "the stub altered the buffer");

    // Both ends are actually visited: a bad index at either end is reported.
    let message = engine
        .apply_plugins(9, 0, &mut buffer, 2)
        .unwrap_err()
        .message()
        .unwrap_or_default()
        .to_string();
    assert!(message.contains("no input at index 9"), "{message}");

    let message = engine
        .apply_plugins(0, 9, &mut buffer, 2)
        .unwrap_err()
        .message()
        .unwrap_or_default()
        .to_string();
    assert!(message.contains("no output at index 9"), "{message}");
}

#[test]
fn direction_is_carried_on_the_device() {
    let Some((input, outputs)) = devices() else { return; };

    assert_eq!(input.direction(), Direction::Input);
    assert_eq!(outputs[0].direction(), Direction::Output);
}
