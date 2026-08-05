# atome

[![Crates.io](https://img.shields.io/crates/v/atome.svg)](https://crates.io/crates/atome) [![docs.rs](https://docs.rs/atome/badge.svg)](https://docs.rs/atome/)

**A**udio **T**ranslucent **O**ptimized **M**acGyver **E**ngine

A real-time audio engine over [`cpal`](https://github.com/RustAudio/cpal). `atome`
handles the parts every audio application has to build anyway — device and host
enumeration, output streams with sample-accurate scheduling, capture streams,
multi-device routing, plugin chains, and audio file decoding — and leaves the
audio callback allocation-free while doing it.

It is generic over the sample type. `OutputClass<i16>` is `i16` from
`add_samples` all the way to the device; nothing is converted behind your back.

`atome` is synchronous and thread-based. There is no `async` API and no runtime
requirement — the mixer runs on its own thread and hands audio to the callback
over a lock-free ring buffer.

## Status

Early. The version is `0.8.0`, APIs are not stable, and breaking changes should
be expected until `1.0`.

**Working today:**

- Output streams (`OutputClass<S>`) with a mixer thread, indexed sample-accurate
  scheduling, and a stop that clears both the mixer and the committed ring buffer
- Capture streams (`InputClass<S>`)
- Format alignment: planar↔interleaved weaving, channel up/down mapping, linear
  resampling
- Device and host enumeration and lookup
- `AudioEngine<S>` over many devices, with routing resolved at construction and
  plugin chains at three levels
- Plugin hosting: internal (Rust functions), VST3, and AU v2/v3, all through one
  `Plugin::apply`
- Fourteen built-in effects (`plugins::atome`) with a shared parameter vocabulary
- Audio file import: container/encoding identification from magic bytes, then
  decoding through Symphonia, whole-file or block at a time

**Not there yet** — the full list is in [`planning/TODO.md`](planning/TODO.md):

- **The engine does not carry captured audio from an input to its outputs.**
  Routing is resolved and the streams are built, but the lock-free hand-off
  between them is unwritten. `examples/live_thru.rs` does that forwarding by
  hand, and is the shape it will take
- No sample/voice layer, no signal graph, no buses. `AudioEngine` mixes by
  scheduling into an output, not by walking a graph
- No audio export or encoding — decoding only
- No duplex operation, no drift correction between unsynced device clocks
- VST 2.4 is a module of documentation and nothing else: there is no maintained
  Rust crate to host through, so `Plugin::load` refuses the format
- No CI. `.github/workflows/publish.yml` covers releases only
- No benchmarks

## Installation

```toml
[dependencies]
atome = "0.8.0"
```

Every feature is off by default. Decoding audio files needs `import`, and
hosting plugins needs the feature for that format:

```toml
[dependencies]
atome = { version = "0.8.0", features = ["import", "vst3", "au3"] }
```

## Quick start

Decode a file and play it. This is `examples/play_file.rs`, shortened.

```rust
use atome::device::AtomeDevice;
use atome::output::{OutputType, SampleRate};
use atome::{import, output, AudioEngine};
use cpal::traits::StreamTrait;
use std::path::Path;

fn main() -> Result<(), cpal::Error> {
    // Identification reads the file's bytes, never its name.
    let decoded = import::decode(Path::new("assets/kick.wav"))?;

    let devices = output::list_devices(None)?;
    let device = devices.first().expect("no output devices");
    let rate = SampleRate::from_hz(decoded.sample_rate).unwrap_or(SampleRate::Hz48k);

    let mut engine = AudioEngine::<f32>::new(
        vec![],                                                    // inputs
        vec![AtomeDevice::output(device.clone(), OutputType::CoreAudio)],
        rate,
        vec![decoded.channels],                                    // one count per output
        None,                                                      // device picks the buffer size
        vec![],                                                    // engine-wide plugins
    )?;

    let output = engine.outputs_mut()[0].output_mut();

    // The file's sample type becomes the output's here; `align_samples` squares
    // up rate and channel count. Both are no-ops when they already agree.
    let samples = output.align_samples(
        &decoded.samples.to_vec::<f32>(),
        SampleRate::from_hz(decoded.sample_rate).unwrap_or(rate),
        decoded.channels,
        true, // already interleaved
    )?;

    let stream = output.build_stream()?;
    stream.play()?;
    output.add_samples(&samples, 0)?;

    std::thread::sleep(std::time::Duration::from_secs_f64(decoded.duration() + 0.5));
    Ok(())
}
```

`make example-play` runs the full version.

## Core concepts

### Sample type

`SampleType` is implemented for every type cpal can hand a stream callback —
`i8, i16, I24, i32, i64, u8, u16, U24, u32, u64, f32, f64`. It is a type
parameter rather than a runtime tag, so the callback is picked once when the
stream is built, and an `i16` pipeline never round-trips through `f32`.

### OutputClass

Owns one playback device's stream. `add_samples(&samples, index)` schedules
interleaved samples at an absolute sample index, summing with anything already
scheduled there — so two sources become one mix without either knowing about the
other. The call only hands a command to the mixer thread; no audio touches the
ring buffer on the calling thread, and the audio callback only pops from it.

`stop()` clears the mixer's queue *and* the samples already committed to the ring
buffer, so the tail is at most one buffer long rather than one buffer of audio
you have already lost control of.

### InputClass

The mirror, and deliberately much smaller: an input only has to hand over what
the device just gave it, so there is no queue and no mixer. You supply a
callback, and it runs **on the audio thread** — push the samples somewhere
lock-free and do the work elsewhere.

### AtomeDevice and AudioEngine

An `AtomeDevice` is a cpal device plus what `atome` needs on top of it: the host
it came through, its own plugin chain, and — for an input — which outputs it
feeds. Direction is a property of the device, so handing an output to the input
list fails at construction rather than when the wrong kind of stream is built.

```rust
use atome::device::AtomeDevice;
use atome::output::OutputType;

// A talkback mic that reaches the monitors and not the main mix.
let mic = AtomeDevice::default_input(OutputType::CoreAudio)
    .expect("no input device")
    .route_to(["Monitors"]);
```

An input that names no outputs feeds all of them. Names are resolved to indices
once, when the engine is built, so a name matching nothing is an error there
rather than silence at runtime.

`output_channels` is a list — `[2, 2, 5]` — because a stereo pair and a 5.1 rig
on one engine need different counts. A list of the wrong length is rejected, not
silently truncated.

### Plugins

The same `Plugin` attaches at three levels, and where it attaches is what decides
how much audio it hears:

| Attached to | Hears |
| --- | --- |
| An input's `AtomeDevice` | that input alone, before routing |
| `AudioEngine::new` directly | everything |
| An output's `AtomeDevice` | what that device plays |

| Format | Feature | Availability |
| --- | --- | --- |
| Internal (Rust functions) | — | always |
| VST 3 | `vst3` | all platforms |
| AU v2 | `au` | macOS / iOS only |
| AU v3 | `au3` | macOS / iOS only |
| VST 2.4 | `vst` | no backend — `load` refuses it |

The shortest plugin is a Rust function, needing no feature and nothing installed:

```rust
use atome::Plugin;

let quieter = Plugin::internal("-6 dB", |buffer: &mut [f32], _channels| {
    for sample in buffer {
        *sample *= 0.5;
    }
});
```

Hosted formats take the same route: describe with `Plugin::new`, call
`Plugin::load` once off the audio thread, then `Plugin::apply` per block.

### Built-in effects

`plugins::atome` has fourteen, all DSP written here and compiled in: `gain`,
`pan`, `width`, `filter`, `eq`, `compressor`, `limiter`, `gate`, `saturation`,
`tremolo`, `delay`, `chorus`, `flanger`, `reverb`.

```rust
// The leading `::` matters: this module is called `atome` inside a crate called
// `atome`, so importing it shadows the crate name. Or alias it —
// `use atome::plugins::atome as effects;`.
use ::atome::plugins::{atome, Params};

let compressor = atome::compressor(48_000)
    .with_params("threshold_db: -18, ratio: 4, attack_ms: 5")?;

// Or by name, which is what a chain described by a config file needs.
let delay = atome::create("delay", 48_000, &Params::parse("time_ms: 375, mix: 0.3")?)?;
```

Parameters are one vocabulary across every format — a built-in matches names
against its own fields, a VST3 or AU against the names the plugin reports, and
`set_params`, `set_param`, `set_params_str`, `params`, and `param_schema` read
the same either way. A set is all-or-nothing: every name and value is checked
before any is written.

Each constructor takes a sample rate, because almost everything here derives a
coefficient from one and `process` only gets a buffer and a channel count. Build
the effect for the rate of the stream it will sit in.

### Importing audio (`import` feature)

Identification is two steps and neither looks at the file name: recognise the
container by its magic bytes, then read that container's header for the encoding
inside. `.ogg` says nothing about Vorbis versus Opus, and `.m4a` covers both AAC
and ALAC.

```rust
use atome::import;
use std::path::Path;

let path = Path::new("assets/pad.flac");

let encoding = import::find_type(path)?;   // Encoding::Flac
let decoded = import::decode(path)?;       // whole file, in the file's own type
let mut blocks = import::stream::<f32>(path)?; // or a block at a time, as f32
```

`decode` returns `Decoded`, whose `samples` is a `Samples` enum carrying the
file's *own* sample type — a 16-bit WAV is `I16`, a 24-bit FLAC is `I24`. Match
on it to avoid a conversion, or call `to_vec::<S>()` when you need one type.

**Decodes today:** PCM (WAV, AIFF, CAF), MP3, AAC-LC, Vorbis, FLAC, ALAC, plus
HE-AAC v1/v2 with `import-he-aac` and Opus with `import-opus`. The MP4, Matroska,
and Ogg demuxers needed to reach them come with `import`.

**Identified but not decodable:** AC-3, E-AC-3, DTS, DTS-HD MA, Dolby TrueHD,
WMA, AMR-NB, AMR-WB. None has a published, maintained Rust decoder to point a
feature at, so these fail with a clear "no decoder" error rather than a misread
file.

## Feature flags

| Feature | Description |
| --- | --- |
| `asio` | ASIO output via cpal's ASIO backend. Windows in practice; needs the ASIO SDK and LLVM/Clang on `PATH` |
| `vst` | VST 2.4 module. Documentation only — no backend |
| `vst3` | VST 3 hosting via `truce-rack-vst3` |
| `au` | Audio Unit v2 hosting. macOS / iOS only |
| `au3` | Audio Unit v3 (App Extension) hosting. macOS / iOS only |
| `plugins` | Every plugin format at once |
| `import` | Audio file decoding — PCM, MP3, AAC-LC, Vorbis, FLAC, ALAC. Pure Rust |
| `import-he-aac` | HE-AAC v1/v2 via libfdk-aac. Needs a C toolchain; check its licence |
| `import-opus` | Opus via libopus, built from source. Needs a C compiler |
| `import-all` | Every format with a working decoder |

## Examples

`make help` lists them all. `example1`–`example4` build real streams and print
how they are wired without making sound; the `example-*` targets are audible.

| Target | What it shows |
| --- | --- |
| `make example1` | The smallest engine: one input, one output, no routing |
| `make example2` | An input routed to one specific output |
| `make example3` | Several outputs with different channel counts, partial routing |
| `make example4` | Plugins at all three levels, and what each one hears |
| `make example-play` | Decode a file and play it. **Makes sound** |
| `make example-effects` | File → plugin chain → device, block by block. **Makes sound** |
| `make example-thru` | Microphone → all three plugin levels → device. **Headphones** |
| `make example-mix` | A file and a microphone summed into one output. **Headphones** |

`example-thru` and `example-mix` open a microphone and play it back — through a
speaker that is a feedback loop.

## Platform support

Developed and run on macOS (CoreAudio) and Windows (WASAPI, ASIO). The
`OutputType` enum names only those hosts — CoreAudio, WASAPI, ASIO, DirectSound,
WDMKS, MME — so while cpal itself reaches ALSA and JACK, there is no variant for
them and Linux is untested.

## Contributing

Issues and PRs are welcome. Since this crate touches real-time audio code, please
include a brief note on how a change was tested (a sample project, a recording,
or a description of the signal chain) when contributing to the engine, mixer, or
plugin internals.

## License

Licensed under the [MIT License](LICENSE).
