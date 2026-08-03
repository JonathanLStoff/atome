//! Capture-side checks.
//!
//! `capture_from_a_real_device` needs a microphone and is `#[ignore]`d; the
//! rest only need the API to behave.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use atome::input::{self, InputClass, InputType};
use atome::output::SampleRate;
use cpal::traits::StreamTrait;

fn host_type() -> InputType {
    if cfg!(target_os = "windows") { InputType::WASAPI } else { InputType::CoreAudio }
}

#[test]
fn lists_input_devices_separately_from_outputs() {
    let inputs = input::list_devices(None).expect("list input devices");
    let outputs = atome::output::list_devices(None).expect("list output devices");

    println!("inputs:  {:?}", inputs.iter().map(atome::output::device_name).collect::<Vec<_>>());
    println!("outputs: {:?}", outputs.iter().map(atome::output::device_name).collect::<Vec<_>>());

    // Nothing to assert about the contents — a CI box may have neither — but
    // the two must be answered by different enumerations, not the same one.
    assert_eq!(inputs.len(), input::list_device_names().expect("names").len());
}

#[test]
fn reports_what_it_was_configured_with() {
    if input::default_device().is_none() {
        eprintln!("skipping: no input device");
        return;
    }

    let input = InputClass::<f32>::new(None, host_type(), SampleRate::Hz48k, Some(512), |_| {});

    assert_eq!(input.sample_rate(), 48_000);
    assert_eq!(input.buffer_size(), Some(512));
    assert_eq!(input.in_type(), host_type());
    assert_eq!(input.sample_format(), cpal::SampleFormat::F32);
    assert!(input.channels() >= 1, "a capture device must have a channel");
    assert!(input.stream().is_none(), "nothing is built until asked");

    println!("{} @ {} ch", input.name(), input.channels());
}

#[test]
fn the_callback_is_handed_over_only_once() {
    if input::default_device().is_none() {
        eprintln!("skipping: no input device");
        return;
    }

    let mut input = InputClass::<f32>::new(None, host_type(), SampleRate::Hz48k, None, |_| {});

    if input.build_stream().is_err() {
        eprintln!("skipping: device would not open");
        return;
    }

    // The callback was moved into the stream, so a second build has nothing to
    // give it and must say so rather than building a silent stream.
    let second = input.build_stream();
    assert!(second.is_err(), "building twice should fail");

    // ...until a new callback is supplied.
    input.close();
    input.set_callback(|_| {});
    assert!(input.build_stream().is_ok(), "should rebuild after set_callback");
}

/// Captures for a moment and checks the callback actually ran.
#[test]
#[ignore = "manual: needs a microphone, and records from it"]
fn capture_from_a_real_device() {
    let calls = Arc::new(AtomicUsize::new(0));
    let samples = Arc::new(AtomicUsize::new(0));

    let (calls_cb, samples_cb) = (Arc::clone(&calls), Arc::clone(&samples));
    let mut input = InputClass::<f32>::new(
        None,
        host_type(),
        SampleRate::Hz48k,
        None,
        move |buffer: &[f32]| {
            calls_cb.fetch_add(1, Ordering::Relaxed);
            samples_cb.fetch_add(buffer.len(), Ordering::Relaxed);
        },
    );

    println!("capturing from {} ({} ch)", input.name(), input.channels());

    let stream = input.build_stream().expect("build the input stream");
    stream.play().expect("start capturing");

    std::thread::sleep(std::time::Duration::from_millis(500));
    input.close();

    let calls = calls.load(Ordering::Relaxed);
    let samples = samples.load(Ordering::Relaxed);
    println!("{calls} callbacks, {samples} samples");

    assert!(calls > 0, "the callback never ran");
    assert!(samples > 0, "no samples were captured");
}
