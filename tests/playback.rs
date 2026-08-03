//! Plays a file through a real output device, end to end.
//!
//! This is a manual check, not something CI can judge: the only thing that says
//! it worked is hearing a 415 Hz tone. It is `#[ignore]`d so an ordinary
//! `cargo test` skips it, and it reads from stdin, so it needs
//! `--nocapture` and a single thread:
//!
//! ```sh
//! cargo test --features import --test playback -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `play_file` picks a host and a device, asking only where there is more than
//! one of either. `report_decode` does everything except open the stream, so
//! the decode path can be checked on a machine with no audio hardware:
//!
//! ```sh
//! cargo test --features import --test playback report_decode -- --ignored --nocapture
//! ```

#![cfg(feature = "import")]

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use atome::import::{self, Decoded};
use atome::output::{
    device_name, list_devices, list_hosts, OutputClass, OutputType, SampleRate, SampleType,
};
use cpal::traits::StreamTrait;
use cpal::{Device, Host};

/// The sample type the whole chain runs in. `f32` is what the file decodes to
/// most naturally and what every device accepts; changing this one line moves
/// decoding, mixing, and playback together, which is the point of the engine
/// being generic over it.
type Sample = f32;

/// Extra time to wait past the end of the audio, covering the buffering
/// between `add_samples` and the speaker.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

fn test_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test_data")
        .join("test415hz.mp3")
}

/// Decodes the file and prints what came back.
///
/// The four things an output needs are exactly what this reports: sample rate,
/// channel count, sample format, and the samples themselves.
fn decode() -> Decoded {
    let path = test_file();
    println!("file:          {}", path.display());

    let encoding = import::find_type(&path).expect("identify the file");
    println!("encoding:      {encoding:?}");

    let decoded = import::decode(&path).expect("decode the file");

    println!("sample rate:   {} Hz", decoded.sample_rate);
    println!("channels:      {}", decoded.channels);
    println!("sample type:   {:?}", decoded.sample_format());
    println!(
        "samples:       {} ({} frames, {:.2} s)",
        decoded.samples.len(),
        decoded.frames(),
        decoded.duration()
    );

    decoded
}

/// Reads a line, returning `None` at end of input.
fn prompt(question: &str) -> Option<String> {
    print!("{question}");
    io::stdout().flush().ok()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok()?;

    Some(line.trim().to_owned())
}

/// Asks which of `options` to use, by number.
///
/// Only asks when there is a decision to make: one option is chosen silently,
/// and an empty answer takes the first.
fn choose<T>(what: &str, options: Vec<T>, describe: impl Fn(&T) -> String) -> T {
    assert!(!options.is_empty(), "no {what} available");

    if options.len() == 1 {
        let only = options.into_iter().next().expect("checked non-empty");
        println!("{what}: {} (only one)", describe(&only));
        return only;
    }

    println!("\navailable {what}s:");
    for (index, option) in options.iter().enumerate() {
        println!("  {index}: {}", describe(option));
    }

    let answer = prompt(&format!("choose a {what} [0]: ")).unwrap_or_default();
    let chosen = answer.parse::<usize>().unwrap_or(0).min(options.len() - 1);

    let picked = options.into_iter().nth(chosen).expect("index clamped above");
    println!("using: {}", describe(&picked));

    picked
}

/// Maps a host to the [`OutputType`] tag an [`OutputClass`] is built with.
///
/// cpal names hosts as strings, and this enum is atome's own vocabulary, so
/// something has to translate. An unrecognised host falls back to the platform
/// default rather than failing — the tag is descriptive, and cpal has already
/// decided which backend is actually in use.
fn output_type(host: &Host) -> OutputType {
    match host.id().name().to_lowercase().as_str() {
        name if name.contains("asio") => OutputType::ASIO,
        name if name.contains("wasapi") => OutputType::WASAPI,
        name if name.contains("directsound") => OutputType::DirectSound,
        name if name.contains("wdm") => OutputType::WDMKS,
        name if name.contains("mme") => OutputType::MME,
        _ if cfg!(target_os = "windows") => OutputType::WASAPI,
        _ => OutputType::CoreAudio,
    }
}

/// Decodes and reports, without touching audio hardware.
#[test]
#[ignore = "manual: prints what the decoder produced"]
fn report_decode() {
    let decoded = decode();

    assert!(!decoded.samples.is_empty(), "decoded no audio");

    // The file decides the type, not this test. Playback needs one specific
    // type, so it converts — and says so when a conversion is happening.
    if decoded.sample_format() == Sample::format() {
        println!("\nfile is already {:?}; no conversion needed", Sample::format());
    } else {
        println!(
            "\nfile is {:?}; playback converts to {:?}",
            decoded.sample_format(),
            Sample::format()
        );
    }

    match SampleRate::from_hz(decoded.sample_rate) {
        Some(rate) => println!("\noutput would open at {} Hz directly", rate.hz()),
        None => println!(
            "\n{} Hz is not an output rate; align_samples would resample it",
            decoded.sample_rate
        ),
    }
}

/// Decodes the file and plays it through a chosen device.
#[test]
#[ignore = "manual: plays audio through a real device"]
fn play_file() {
    let decoded = decode();

    let host = choose("host", list_hosts(), |host| host.id().name().to_owned());

    // `Host` is not `Clone`, and listing devices consumes it, so the tag is
    // read off before it goes.
    let out_type = output_type(&host);
    let devices = list_devices(Some(host)).expect("list output devices");
    let device = choose("output device", devices, |device: &Device| {
        device_name(device)
    });

    // Open at the file's own rate where the enum has it, so nothing is
    // resampled. Where it does not, fall back and let `align_samples` convert
    // — which is the more interesting path to exercise anyway.
    let rate = SampleRate::from_hz(decoded.sample_rate).unwrap_or(SampleRate::Hz48k);
    if rate.hz() != decoded.sample_rate {
        println!(
            "\nresampling {} Hz -> {} Hz",
            decoded.sample_rate,
            rate.hz()
        );
    }

    let mut output =
        OutputClass::<Sample>::new(Some(device), out_type, decoded.channels, rate, None);

    println!(
        "\noutput:        {} @ {} Hz, {} ch, {:?}",
        output.name(),
        output.sample_rate(),
        output.channels(),
        output.sample_format()
    );

    // Match the file to the output. A no-op when the two already agree, which
    // is why it is called unconditionally rather than guarded.
    let samples = output
        .align_samples(
            &decoded.samples.to_vec::<Sample>(),
            SampleRate::from_hz(decoded.sample_rate).unwrap_or(rate),
            decoded.channels,
            true,
        )
        .expect("align the samples to the output");

    let stream = output.build_stream().expect("build the output stream");
    stream.play().expect("start the stream");

    output.add_samples(&samples, 0).expect("schedule the audio");

    let seconds = samples.len() as f64
        / output.sample_rate() as f64
        / output.channels().max(1) as f64;
    println!("\nplaying {seconds:.2} s — listen for a 415 Hz tone");

    thread::sleep(Duration::from_secs_f64(seconds) + DRAIN_GRACE);

    output.stop();
    println!("done");
}
