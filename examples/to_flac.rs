//! Decodes the audio in a file — a song, or the soundtrack of a video — and
//! writes it out as FLAC. Makes no sound.
//!
//! Needs the `import` and `export` features.
//!
//! ```sh
//! make example-to-flac FILE=clip.mp4                  # writes clip.flac beside it
//! cargo run --features import,export --example to_flac -- clip.mp4 out.flac
//! ```

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use atome::export::to_flac;
use atome::import;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);

    let Some(input) = args.next().map(PathBuf::from) else {
        eprintln!("usage: to_flac <input> [output.flac]");
        std::process::exit(2);
    };
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| input.with_extension("flac"));

    println!(
        "{}: {:?} audio in {:?}",
        input.display(),
        import::find_type(&input)?,
        import::find_container(&input)?
    );

    let started = Instant::now();
    let summary = to_flac(&input, &output)?;

    println!(
        "{}: {:.2} s of {} Hz, {} channel{}, {}-bit — {} bytes, written in {:.0} ms",
        output.display(),
        summary.duration(),
        summary.sample_rate,
        summary.channels,
        if summary.channels == 1 { "" } else { "s" },
        summary.bits_per_sample,
        summary.bytes,
        started.elapsed().as_secs_f64() * 1000.0
    );

    Ok(())
}
