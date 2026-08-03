//! Decodes a file and plays it through an engine output. **Makes sound.**
//!
//! The only example here that you can hear. The others build streams and show
//! how they are wired; this one goes the whole way, because playback does not
//! depend on the input-to-output forwarding that is still unfinished — the
//! audio comes from a file rather than a capture device.
//!
//! Needs the `import` feature, which is what pulls in the decoders.
//!
//! ```sh
//! make example-play
//! make example-play FILE=~/Music/something.flac
//! ```

use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use atome::device::AtomeDevice;
use atome::import;
use atome::output::SampleRate;
use atome::{output, AudioEngine};
use cpal::traits::StreamTrait;

mod common;

/// The sample type the output runs in. A file decodes to its own type, so this
/// is the one place a conversion is unavoidable: the device needs one type,
/// fixed when the code is written.
type Sample = f32;

fn main() -> Result<(), cpal::Error> {
    let path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("test_data")
                .join("test415hz.mp3")
        });

    if !path.exists() {
        eprintln!("no such file: {}", path.display());
        return Ok(());
    }

    // Identification reads the file's bytes, never its name.
    let encoding = import::find_type(&path)?;
    let decoded = import::decode(&path)?;

    println!("file:        {}", path.display());
    println!("encoding:    {encoding:?}");
    println!("sample rate: {} Hz", decoded.sample_rate);
    println!("channels:    {}", decoded.channels);
    println!("sample type: {:?}", decoded.sample_format());
    println!("length:      {:.2} s", decoded.duration());

    let outputs = output::list_devices(None)?;
    let Some(device) = outputs.first() else {
        eprintln!("no output devices on this machine");
        return Ok(());
    };

    // Open at the file's own rate where it is one an output can take, so
    // nothing is resampled.
    let rate = SampleRate::from_hz(decoded.sample_rate).unwrap_or(SampleRate::Hz48k);
    if rate.hz() != decoded.sample_rate {
        println!("\nresampling {} Hz -> {} Hz", decoded.sample_rate, rate.hz());
    }

    let mut engine = AudioEngine::<Sample>::new(
        vec![],
        vec![AtomeDevice::output(device.clone(), common::host())],
        rate,
        vec![decoded.channels],
        None,
        vec![],
    )?;

    common::describe(&engine);

    let output = engine.outputs_mut()[0].output_mut();

    // The file's type becomes the output's here, and `align_samples` squares up
    // rate and channel count. Both are no-ops when they already agree.
    let samples = output.align_samples(
        &decoded.samples.to_vec::<Sample>(),
        SampleRate::from_hz(decoded.sample_rate).unwrap_or(rate),
        decoded.channels,
        true,
    )?;

    let stream = output.build_stream()?;
    stream.play()?;

    output.add_samples(&samples, 0)?;

    let seconds =
        samples.len() as f64 / output.sample_rate() as f64 / output.channels().max(1) as f64;
    println!("\nplaying {seconds:.2} s through {}", common::name_of(device));

    thread::sleep(Duration::from_secs_f64(seconds) + Duration::from_millis(500));

    output.stop();
    println!("done");

    Ok(())
}
