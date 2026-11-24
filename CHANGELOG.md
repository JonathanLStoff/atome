# Changelog

Finished work is moved out of `planning/TODO.md` and listed under **Done**
below as it lands (see `.claude/CLAUDE.MD`).

`make release v=x.y.z` empties **Done** into a new `## Release x.y.z` section
underneath it, so **Done** always holds exactly what is unreleased.

**Done**

## Release 0.8.17

- **FLAC export never writes a frame larger than its samples stored plain.** flacenc
  0.5's fixed predictors could pick a Rice parameter far too small for clipped 24-bit
  audio (a loud MP3 decoded), writing residuals mostly in unary: a 20-minute MP3 became
  a 4.5 GB FLAC and needed 3.5 GB of memory, enough for iOS to end the app. A block
  that comes out larger than plain is encoded again without the fixed predictors, then
  plain. The same MP3 now makes a 106 MB FLAC in 9 MB of memory.

## Release 0.8.16

- **A VST3 gets a buffer for every audio bus it declares.** The host passed one input
  and one output bus whatever the plugin had, so a plugin with a sidechain (iZotope
  VocalSynth 2) or with no input at all (an instrument: Massive X) read past them and
  crashed on its first block. Sidechains now get silence, extra outputs are dropped,
  and an instrument gets no input bus.
- **`scan::vst3_bundle(path)`** lists the plugins of one VST3 bundle, and a bundle
  without `moduleinfo.json` now says which are instruments (its sub-categories,
  through `IPluginFactory2`).

## Release 0.8.15

- **Per-channel levels on a voice: pan in the mixer.** `Scheduler::set_levels(voice,
  &levels, over)` weighs each output channel of a voice by its own level, ramped,
  on top of `set_gain`. Done at commit like the gain, so a pan move is heard within
  a buffer however far ahead the voice is scheduled.
- **Buses.** `Scheduler::new_bus()` gives a bus id; `send(voice, Some(bus))` mixes
  a voice into it instead of the output. A bus sums its voices apart, runs its
  effects (`set_bus_effects`), then its own `set_gain` and `set_levels`, and goes
  into the output. `cancel(bus)` removes it; its voices go straight to the output.
  One level: a bus does not go into another bus. A stop keeps a bus's level and pan.
- **VST3 host.** The host the plugin talks to: its context, a component handler
  for edits made in the plugin's own window, the frame its view sits in, and the
  messages between processor and controller. Parameter edits wait for the next
  processed block.

## Release 0.8.14

- **An effect on a whole output.** `OutputClass::set_master_effect(Some(plugin))`
  (or `MixerHandle::set_master(plugin, channels)`) runs one effect on
  everything the output plays, as the mixer commits it, in the output's own
  sample type: a limiter to protect the speakers. `None` takes it off; heard
  from the next buffer. While one is set the mixer commits whole frames only.

## Release 0.8.13

- **Reverb quality.** `reverb` takes `quality` (default 1): under 0.5 it runs
  four combs into two all-passes instead of eight into four, about half the
  work, with each comb taking more of the input so the tail is as loud. For
  phones, where the reverb is the one heavy effect.
- **A dead mixer is seen.** `MixerHandle::is_alive()` says whether the mixer
  thread still runs (one that panicked or returned does not; the stream pads
  with silence, so the clock alone cannot tell), and `MixerHandle::passes()`
  counts its passes, so a watchdog that sees it stand still has a stuck one.

## Release 0.8.12

- **Plugin windows and state.** `Plugin::editor()` hands out a hosted
  plugin's own window (`PluginEditor`, re-exported with `WindowHandle`) to
  open in a native parent, main thread only. `Plugin::state()` /
  `set_state()` save and restore a hosted plugin's whole state as opaque
  bytes, for a host to keep with a project.
- **`plugins::scan`.** `scan::vst3(folders)` and `scan::au()` list installed
  plugins as `Found { name, vendor, format, id, path, kind, instrument }`,
  with `Failed { path, reason }` for bundles that would not read, and
  `Found::plugin()` builds the descriptor. A VST3's vendor and kind come from
  its `moduleinfo.json` sub-categories without loading it; otherwise the kind
  is guessed from the name (`kind_from_name`), one of `scan::KINDS`.
  `default_vst3_folders()` gives the OS's usual folders. `vst3` now pulls in
  `serde_json`.
- **AU loads by id.** An AU lives in the registry, so the descriptor's `path`
  is now its `type:sub:mfr` id (as `Found::id` gives it); before, every AU
  load scanned a path and failed.
- **VST3 picks the class by name** in a bundle that holds several, the first
  one otherwise.
- **VST3 host carried in-tree** (`plugins::vst3_host`, from `truce-rack-vst3`
  1.1.5, kept for good). Three fixes: a dropped VST3 no longer crashes (the
  library was unloaded before the plugin's COM objects were released); Windows
  calls `InitDll` / `ExitDll`, which Native Instruments and other plugins need;
  and a bundle path loads (`scan_bundle`), where before only a folder of bundles
  did, so `Plugin::load` and `scan::vst3` found nothing for a `.vst3` path or a
  bundle without `moduleinfo.json`. `vst3` now depends on `vst3` and
  `libloading` directly instead of `truce-rack-vst3`.
- **`plugins::remote`: a hosted plugin in a helper process**, so one that
  crashes takes down only its helper. `Remote::spawn` starts a program that
  calls `remote::host_main` (a host relaunches itself with an argument) and
  loads the plugin there; `process`, `reset`, `state` / `set_state`,
  `latency` and `open_editor` / `close_editor` (the window in the host's
  parent window; the helper pumps Windows messages) go over its standard input
  and output, about 80 µs a 1024-frame block. A dead helper fails every call
  and reports `is_dead`.
- **`Plugin::reset` reaches hosted plugins:** a VST3 or AU is deactivated and
  activated again at the same layout, clearing its tails between uses.
- **Three built-ins:** `polarity` (invert per side), `phaser` (swept all-pass
  notches with feedback) and `de_esser` (high-pass keyed compression,
  split-band or wideband).

## Release 0.8.11

- **AU hosting links on iOS.** `au` and `au3` pulled in `truce-rack-au` 1.1.5,
  which declares `objc2-app-kit` (for its Cocoa editor) on every Apple target,
  so an iOS app built with either feature failed to link: `ld: framework
  'AppKit' not found`. The two truce-rack AU crates (`truce-rack-au`,
  `truce-rack-au3`, MIT OR Apache-2.0) are now carried in-tree as
  `plugins::au_host` and `plugins::au3_scan`, with their own dependencies, and
  the editor and `objc2-app-kit` are compiled for macOS only. On iOS a loaded
  AU reports no editor; scanning, loading, rendering, MIDI, parameters,
  presets and state are upstream's code unchanged (one let chain rewritten for
  edition 2021). macOS behaves as before. Checked by linking an iOS executable
  with `au3`: 0.8.10 fails with that error, this links (AudioToolbox, no
  AppKit). Upstream's 2 tests come along in `plugins/au_host.rs`

## Release 0.8.10

- **Linux and Android have a host.** `OutputType` had Windows' APIs and
  CoreAudio only, so an application choosing a type by platform had none to
  give a device on Linux or Android and could not build an engine there. It
  gains `ALSA` and `AAudio`, `OutputType::native()` (the platform's usual API)
  and `OutputType::from_host_name` (from cpal's name for a host). The type is
  a label: which devices exist is still decided by the `cpal::Host` they were
  listed from. Compile-checked for Android, iOS and Windows
- **A voice's level can be changed while it plays.** `Scheduler::set_gain(voice,
  gain, over)` brings a voice to a linear level over a time (zero: at once),
  from wherever it is, and holds it for what the voice schedules afterwards.
  `Scheduler::fade_out(voice, over)` ramps to silence and then takes the voice
  back, as `cancel` does: a stop that does not click. Both act in the mixer on
  samples the device has not been handed yet, so they are heard within a
  buffer however far ahead the voice is scheduled. Every channel of a frame
  gets one gain; a voice at one costs nothing. 4 tests in `output/mixer.rs`
- **`Scheduler::aligner()`: `align` with nothing allocated per block.** An
  `Aligner` keeps its buffers between calls, and hands a block that is already
  at the output's rate and channel count straight back, not copied.
  `Aligner::matches` says when that is. 2 tests in `output/scheduler.rs`
- **Streams seek, to the sample.** `AudioStream::seek(frame)` moves a stream
  so the next `read` starts exactly at `frame` (per channel, from the start;
  past the end is the end) and says where it is. Symphonia streams ask the
  container for the packet at or before it, ask again from further back if
  it lands late, then decode forward and drop what comes first. Each seek
  opens the container again from the top: Symphonia 0.6's FLAC reader keeps
  the fragments it read ahead across a seek that lands exactly on a frame
  (every seek back to the start does), and after the first packet the audio
  jumped back to where it had been. ADTS rewinds and decodes forward. Other
  streams refuse with `UnsupportedOperation`, the trait's default. 6 tests
- **`import::Section`: in and out points, and gapless loops.** A stream from
  one frame to another (`None`: its end), once or round and round. Looping,
  the end of a pass seeks back to the start inside the same `read`, so
  whatever schedules the samples sees one unbroken run: no gap at the seam.
  `passes()` counts the turns; a looping section never ends. 4 tests

- **Sounds scheduled after a quiet stretch play on time.** The mixer used to
  stop moving its cursor whenever it had nothing to play, while the
  `PlayClock` kept counting. Anything scheduled off the clock after more than
  about 11 s of quiet (`MAX_SCHEDULE_AHEAD`, stereo 48 kHz) was silently
  dropped, and anything sooner started late by however long the output had
  been idle. This silenced OrbitX cues and could affect vtome soundtracks.
  `Mixer::flush` now pads with silence so the ring is always full and the
  cursor stays exactly one buffer ahead of the clock. The clock now advances
  by what the callback takes out of the ring (played, or dropped by a stop)
  instead of by the device's buffer length, so a stop or an underrun no longer
  makes the two drift apart. 1 test
- **Streaming from a reader (0.8.5).** `import::stream_reader::<S>(reader,
  name)` streams audio from any `Box<dyn ReadSeek>` (`Read + Seek + Send +
  Sync`: a `Cursor`, an entry of a pfac bundle) through the same Symphonia
  backend `stream` uses, the container found from the bytes; `name` is only
  what errors say. `open_parts` now opens the file and hands it to
  `open_source`, which the reader path shares. Needs `import`
- **FLAC export takes a writer, not only a path.** `FlacWriter` is generic over
  `W: Write + Seek` (default `File`, so existing code is unchanged);
  `FlacWriter::new(writer, ..)`, `write_flac_into`, and `to_flac_into` write
  into a `Cursor`, an open file, or an entry of a `pfac` bundle. `Seek` is
  required because the header's sample count and MD5 are only known at the end;
  a sink that cannot seek would get a wrong header, so it is not accepted. The
  header is patched at the writer's *starting* position, so a writer that
  already holds bytes keeps them. `FlacWriter::finish_into_inner` hands the
  writer back, flushed and at the end of the stream. A writer given by the
  caller is not synced — what flushing it means is its owner's to say — where
  `create` still syncs its own file. `FlacSummary::bytes` is the stream's size,
  not the writer's. The path functions and the writer ones produce identical
  bytes (tested). 6 tests
- **AAC through the operating system only — no bundled AAC decoder**
  (decided 2026-09-26, done 2026-10-03, planning §3.3). Symphonia's `aac`
  feature and `symphonia-adapter-fdk-aac` are gone; `cargo tree -i` finds
  neither. `import::aac` decodes LC and HE-AAC v1/v2 with AudioToolbox
  (`AudioConverter`, the cookie's richest format so SBR and PS are decoded),
  Media Foundation's AAC MFT on Windows, and MediaCodec on Android; Linux
  refuses AAC by name. Symphonia still demuxes MP4 and Matroska and hands AAC
  packets to the OS decoder registered in its place; bare ADTS, whose reader
  came with Symphonia's AAC crate, is split by atome. `aac_in_mp4`,
  `aac_in_adts`, and the video-soundtrack-to-FLAC test pass through
  AudioToolbox; Windows and Android are compiled and type-checked, not yet
  run. `import-he-aac` remains as an alias of `import` so existing builds
  resolve
- **`Scheduler`: an output's audio scheduled from any thread**, with voices.
  `OutputClass::scheduler()` is cheap to clone, `Send` and `Sync`; samples
  tagged with a voice are held apart until committed, so `cancel(voice)` takes
  back whatever has not reached the device. What vtome plays a film's
  soundtrack through
- Android builds again: the device-id fallback names cpal's AAudio host
- **`PlayClock`: an output's play position, as heard, from any thread**
  (planning §13). The cpal callback counts every buffer it hands over —
  silence included, so it is the stream's own timeline, the mixer's index
  over channels — and records cpal's callback-to-playback latency.
  `position()` subtracts the latency, moves smoothly between callbacks but
  never more than a buffer past the last one, and never runs backwards;
  `is_running()` goes false on `close` or when callbacks stop. Lock-free on
  the audio thread. `OutputClass::clock()`; vtome's `Audio::atome` follows it
- The Docker matrix's Mac leg builds into `target/docker-macos`, so it can no
  longer leave stale test binaries in the checkout's `target/`
- **FLAC writing** (`export` feature, pure Rust via `flacenc`). `FlacWriter`
  encodes one block at a time straight to disk and writes the real STREAMINFO
  — sample count, MD5, frame sizes — over a placeholder at `finish`, so a
  two-hour file is never held in memory. `write_flac` writes a `Decoded` at the
  depth its samples need (8, 16, or 24 bits; 32-bit and float sources at 24).
  `to_flac` decodes anything `import` reads — the audio track of a video
  included — straight into FLAC. Lossless round trips at 16 and 24 bits compare
  equal sample for sample, the short last block included; an H.264+AAC MP4's
  soundtrack came out as a FLAC that ffmpeg decodes cleanly and whose header
  MD5 matches the decoded audio. `make example-to-flac FILE=clip.mp4`
- `README.md` rewritten to describe the crate that exists. The old one promised
  an async `Engine`/`Sample`/`Voice`/`Bus` API and a set of `wav`/`flac`/`mp3`
  format features, none of which were ever written; the new one covers
  `AudioEngine`, `AtomeDevice`, `OutputClass`/`InputClass`, the three plugin
  levels, the built-in effects, and `import`, with the real feature names and a
  status section that says plainly what is missing — chiefly that the engine
  does not yet carry captured audio from an input to its outputs. Every snippet
  in it compiles. The CI badge is gone: it pointed at a workflow with no file
- `planning/TODO.md` section 3 no longer says audio file I/O does not exist —
  decoding has worked through Symphonia since 0.8.0; what is left there is the
  demuxer/decoder split, the formats Symphonia misses, and encoding
- Internal plugins (`plugins::internal`): a Rust function mapped in at load and
  run by `Plugin::apply`. `Plugin::internal` takes the function directly;
  `internal::register` names one so a chain can be described by configuration
  and resolved by `Plugin::load`. Declares its own latency, and needs no Cargo
  feature
- VST3 hosting behind the `vst3` feature, via `truce-rack-vst3`: scan,
  instantiate, activate, and process
- AU v2 (`au`) and AU v3 (`au3`) hosting on macOS/iOS, via `truce-rack-au` and
  `truce-rack-au3`. Both load into the same `AuPlugin`, since v3 differs in
  discovery rather than in rendering
- One hosting path shared by every format (`plugins::host::Hosted`), over
  `truce-rack-core`'s traits: bus-layout selection, activation, and the
  interleaved↔planar conversion each block needs, with the scratch allocated at
  activation rather than per block. Buffers longer than the activated block size
  are split rather than re-activating mid-stream
- `PluginFormat` carries every variant in every build. A format the build cannot
  host is refused by `Plugin::load` with a message naming the feature that would
  fix it, instead of the variant being absent from the enum
- `Plugin::apply` works for any `SampleType`, converting to `f32` and back, so an
  `i16` pipeline can carry plugins
- Fixed: the crate did not compile at all. `Cargo.toml` named `truce_rack_au3`,
  which is not a package on crates.io, and `plugins::mod` referred to
  `truce_rack_core` without depending on it and to a private `load_from`
- The AU crates are now declared under `[target.'cfg(target_vendor = "apple")']`,
  so a Linux or Windows build with `--features plugins` resolves neither them nor
  their objc2 tree
- `make release v=x.y.z` sets the version in `Cargo.toml`, `Cargo.lock`, and the
  README's install snippets, and moves everything under **Done** here into a new
  `## Release x.y.z` section
- `publish.yml` tags the published commit `v<version>` and opens a GitHub release
  for it after the crates.io upload
- Three whole-pipeline examples, all audible: `play_effects` (decode → plugin
  chain → device, block by block so stateful plugins are seen to keep their
  state), `live_thru` (microphone → all three plugin levels → device, live), and
  `mix_sources` (a file and a microphone summed by the mixer into one output).
  `make example-effects`, `make example-thru`, `make example-mix`
- `examples/common::shared_rate` picks a sample rate two devices agree on.
  Opening a device at a rate it does not support is not a hard failure in cpal —
  it reports through the stream's error callback and runs at its own rate, which
  is a confusing thing to find by ear
- `examples/audio_engine4` runs real plugins instead of pass-through stubs, and
  prints the gain products so the chain order is visible
- `plugins::atome`: fourteen built-in effects — gain, pan, width, filter, eq,
  compressor, limiter, gate, saturation, tremolo, delay, chorus, flanger, and
  reverb. Each is a Rust `Effect` with named parameters, built directly
  (`atome::compressor(48_000)`) or by name (`atome::create("delay", rate,
  &params)`) so a chain can be described by configuration
- `plugins::params`: one parameter vocabulary for every format. `Params` is a
  name/value map parsed from a flat JSON object — braces, quotes, and a
  trailing comma all optional — and `Plugin::set_params`, `set_param`,
  `set_params_str`, `with_params`, `params`, and `param_schema` speak it
  whatever the plugin is. A built-in matches names against its own fields; a
  VST3 or AU matches against the parameter names the plugin reports
- Parameter sets are all-or-nothing: every name and value is checked before any
  is written, so a set naming one parameter the plugin lacks changes none
- `Plugin::new`'s `params` string is no longer inert — `load` parses and applies
  it, and a bad value fails the load rather than being ignored
- `Plugin::load` builds a built-in by name when nothing is registered under it,
  so `Plugin::new("compressor", …, "ratio: 4", Internal)` works with no code
- `Effect`, behind `InternalPlugin::from_effect`, for writing an effect that
  carries parameters. Cloning one gives a fresh instance with the same
  parameters and no state — two devices with "the same" compressor should not
  share an envelope follower
- `Plugin::reset` drops a built-in's accumulated state, leaving its parameters
- `examples/play_effects` builds its chain from a name-and-parameter-string
  table rather than hand-rolled closures, and prints each plugin's parameters
