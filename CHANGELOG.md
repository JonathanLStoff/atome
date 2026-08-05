# Changelog

Finished work is moved out of `planning/TODO.md` and listed under **Done**
below as it lands (see `.claude/CLAUDE.MD`).

`make release v=x.y.z` empties **Done** into a new `## Release x.y.z` section
underneath it, so **Done** always holds exactly what is unreleased.

**Done**

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
