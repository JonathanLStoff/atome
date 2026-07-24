# atome

[![Actions Status](https://github.com/JonathanLStoff/atome/workflows/atome/badge.svg)](https://github.com/JonathanLStoff/atome/actions) [![Crates.io](https://img.shields.io/crates/v/atome.svg)](https://crates.io/crates/atome) [![docs.rs](https://docs.rs/atome/badge.svg)](https://docs.rs/atome/)

**A**udio **T**ranslucent **O**ptimized **M**acGyver **E**ngine

An async, `cpal`-based audio engine for Rust. `atome` gives you a small, composable API for building real-time audio applications — sample playback, mixing, routing, and DSP graphs — without wrestling with platform audio callbacks, device enumeration, or buffer management yourself.

If you've used large C++ audio frameworks and wished for something with the same shape but written in idiomatic, async-first Rust, `atome` is aimed at that gap.

## Why atome?

Building audio software in Rust usually means dropping down to [`cpal`](https://github.com/RustAudio/cpal) directly: managing devices, sample formats, ring buffers, and the real-time constraints of the audio callback yourself. That's the right layer for `cpal` to operate at — but it means every project re-invents the same scaffolding.

`atome` sits on top of `cpal` and provides:

- **A sample engine** — load, decode, and play back audio files or in-memory buffers with per-voice gain, pan, pitch, and looping.
- **An async control API** — start playback, adjust parameters, and route signals from any async context (Tokio, async-std, etc.) without touching the real-time thread yourself.
- **A signal graph** — connect sources, effects, and buses into a processing graph that's recomputed safely off the audio thread.
- **Safe real-time boundaries** — commands cross from your async world into the audio callback via lock-free channels, so you get the ergonomics of `async`/`await` without allocating or blocking inside the callback.
- **Device-agnostic setup** — sensible defaults for device and stream selection via `cpal`, with escape hatches when you need explicit control.

## Status

`atome` is early-stage and under active development. APIs are not yet stable. Expect breaking changes between minor versions until `1.0`.

## Installation

```toml
[dependencies]
atome = "0.1"
```

`atome` only decodes WAV files by default. To load other audio file formats, enable the matching feature:

```toml
[dependencies]
atome = { version = "0.1", features = ["flac", "mp3", "ogg"] }
```

Or enable every supported format at once:

```toml
[dependencies]
atome = { version = "0.1", features = ["all-formats"] }
```

## Quick start

```rust
use atome::{Engine, Sample};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Spin up the engine on the default output device.
    let engine = Engine::start_default().await?;

    // Load a sample into memory.
    let kick = Sample::from_file("assets/kick.wav")?;

    // Play it back on a voice with a bit of gain and pan.
    let voice = engine
        .play(&kick)
        .gain(0.8)
        .pan(-0.2)
        .looping(false)
        .await?;

    // Voices can be controlled live from any async task.
    voice.set_gain(0.5).await?;
    voice.stop().await?;

    engine.shutdown().await?;
    Ok(())
}
```

## Core concepts

### Engine

The `Engine` owns the `cpal` stream and audio thread. It's created once per output device and exposes an async handle you can clone and share across tasks. All mutation happens through message passing — nothing blocks the audio callback.

### Samples & Voices

A `Sample` is decoded audio data (loaded from disk or supplied as raw PCM). Calling `engine.play(&sample)` spawns a `Voice` — a single playing instance with its own gain, pan, pitch, and lifecycle, independent of other voices playing the same sample.

### Graph & Buses

Voices route into `Bus`es, and buses can host effects and route into other buses, forming a processing graph similar to a mixing console. Graph changes are applied atomically between audio buffers.

### Async boundary

All control-plane calls (`play`, `set_gain`, `connect`, etc.) are `async fn`s that send a command over a lock-free queue to the real-time thread and await acknowledgement. The audio callback itself never allocates, locks, or awaits — it only drains the command queue and renders audio.

### Loading audio files

`Sample::from_file` picks a decoder based on the file extension. Only the decoders for enabled features are compiled in, so calling it for a format whose feature isn't active fails at runtime with an unsupported-format error rather than a compile error — make sure the feature is enabled for every format you plan to load:

```rust
use atome::Sample;

// Always available.
let click = Sample::from_file("assets/click.wav")?;

// Requires the `flac` feature.
let pad = Sample::from_file("assets/pad.flac")?;

// Requires the `mp3` feature.
let loop_take = Sample::from_file("assets/loop.mp3")?;

// Requires the `ogg` feature.
let amb = Sample::from_file("assets/ambience.ogg")?;
```

## Feature flags

| Feature | Description |
| --- | --- |
| `wav` | WAV file decoding (enabled by default) |
| `flac` | FLAC file decoding |
| `mp3` | MP3 file decoding |
| `ogg` | Ogg Vorbis file decoding |
| `aac` | AAC file decoding |
| `all-formats` | Enables every decoder feature above |
| `resampling` | On-the-fly sample-rate conversion for mismatched sources |

## Platform support

`atome` inherits its platform reach from `cpal`, and is tested on:

- macOS (CoreAudio)
- Windows (WASAPI)
- Linux (ALSA / JACK)

## Contributing

Issues and PRs are welcome. Since this crate touches real-time audio code, please include a brief note on how a change was tested (a sample project, a recording, or a description of the signal chain) when contributing to the engine or graph internals.

## License

Licensed under the [MIT License](LICENSE).
