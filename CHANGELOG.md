# Changelog

Finished work is moved out of `planning/TODO.md` and listed under **Done**
below as it lands (see `.claude/CLAUDE.MD`).

`make release v=x.y.z` empties **Done** into a new `## Release x.y.z` section
underneath it, so **Done** always holds exactly what is unreleased.

**Done**

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
