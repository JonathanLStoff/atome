# atome — Roadmap to a Full Audio Framework

What stands between the current crate and a complete framework for building audio
applications *and* shippable plugins. GUI/UI work is deliberately excluded.

Ordered roughly by what unblocks what. Nothing below is started unless marked.

---

## Where things stand

**Done:**

- Output stream over cpal, generic over sample type (`OutputClass<S>`), so an
  `i16` pipeline stays `i16` end to end
- Mixer thread with indexed, sample-accurate scheduling and lock-free handoff to
  the audio callback
- Stop that clears both the mixer and the committed ring buffer
- Format alignment: planar→interleaved weaving, channel up/down mapping, linear
  resampling
- Device and host enumeration/lookup
- Feature-gated module scaffolding for ASIO, VST2, VST3, AU

- Audio file import: container/encoding identification from magic bytes, and
  decoding via Symphonia in the file's own sample type
- Capture stream (`InputClass<S>`), mirroring the output
- `AudioEngine` over many devices, with routing and three levels of plugin chain

- Plugin hosting: internal (Rust functions), VST3, and AU v2/v3, all through
  one `Plugin::apply`

**Scaffolding only (no implementation):** `src/plugins/vst.rs` — VST2 has no
backend crate to host through, and `Plugin::load` refuses the format rather
than pretending otherwise

---

## 0. Foundation

The crate builds. What is left here is documentation and CI.

- [x] `Cargo.toml`: `vst` no longer names an undeclared dependency — the crate
      builds
- [x] `src/lib.rs`: the prototype `AudioEngine` is gone, replaced by one built
      on `InputClass`/`OutputClass`
- [x] `src/output/mod.rs`: `add_samples_time` compiles — but see its doc
      comment, "from now" still measures from the start of the stream
- [x] `src/output/utils.rs`: `list_device_names` and `find_device` pass `None`
- [ ] `README.md` documents an API that does not exist (`Engine`, `Sample`,
      `Voice`, `Bus`, format features). Reconcile — it currently reads as a
      promise, not a description
- [ ] CI that actually builds and tests (badge points at a workflow with no
      file; `.github/workflows/publish.yml` covers releases only)
- [x] Test suite: `tests/` covers import, decoding, input, and engine wiring
- [x] `examples/`, with a `Makefile` target per example
- [ ] `benches/` still does not exist

---

## 1. Audio input and duplex

- [x] Input capture stream (`src/input/`) — mirror of `OutputClass`
- [ ] Duplex operation: input and output on one device, shared clock
- [ ] Cross-device duplex with drift correction between unsynced clocks
- [ ] Input monitoring path with latency reporting
- [ ] Loopback / system-audio capture where the platform offers it

## 2. Engine: many devices, routing, and plugin chains

`AudioEngine` owns every stream and decides what reaches which device. One
input may feed every output, or only the outputs it names; plugins attach at
three levels and each has a different reach.

### 2.1 Devices

- [x] `AtomeDevice` in `src/device.rs`: a cpal device plus what atome needs on
      top of it — the host it came through, its own plugin chain, and, for an
      input, where its audio is routed
- [x] Builder methods so a device can be described in one expression
- [x] Direction is a property of the device, so an output cannot be handed in
      where an input belongs
- [ ] Resolve a device by name at construction, so a config file can name one

### 2.2 Engine construction

- [x] `AudioEngine::new(inputs, outputs, sample_rate, output_channels,
      buffer_size, plugins)`
- [x] One channel count per output device — `[2, 2, 5]` — since a stereo pair
      and a 5.1 rig on the same engine need different counts
- [x] Reject a channel list that does not match the output list, rather than
      silently pairing them off
- [x] Build an `OutputClass<S>` per output device
- [x] Build an `InputClass<S>` per input device
- [ ] Start and stop every stream together
- [ ] Report per-device failures without taking the whole engine down

### 2.3 Routing

- [x] An input with no routing goes to every output
- [x] An input with routing goes only to the outputs it names
- [x] Resolve names to output indices once, at construction, and fail on a name
      that matches nothing
- [ ] Carry captured audio from the input callback to its outputs — a lock-free
      hand-off per route, drained off the audio thread
- [ ] Convert between an input's channel count and each output's on the way
- [ ] Gain per route

### 2.4 Plugin chains

Three levels, and the distinction is the whole point: the same `Plugin` reaches
different audio depending on where it is attached.

- [x] Engine-wide: plugins handed to `new` directly, applied to everything
- [x] Per input: plugins on an input's `AtomeDevice`, applied to that input's
      audio alone, before routing
- [x] Per output: plugins on an output's `AtomeDevice`, applied to what that
      device plays and nothing else
- [x] Stub the apply step at all three levels, so the ordering is settled
      before any host backend exists
- [x] Real processing — internal, VST3, and AU all process for real; see
      [section 11](#11-plugin-hosting)
- [ ] Bypass and wet/dry per plugin
- [ ] Latency reporting from a chain, feeding delay compensation

## 3. Audio file I/O

The README advertises this; none of it exists.

### 3.1 Infrastructure

- [ ] Split **container** (demuxer) from **codec** (decoder) — the same codec
      appears in several containers, and conflating them means writing AAC
      support three times
- [ ] `Demuxer` trait: probe, enumerate tracks, read packets for one track
- [ ] `Decoder` trait: packet in, PCM frames out, with codec-private init data
- [ ] Registry keyed on magic bytes, falling back to extension — extensions lie,
      especially `.ogg` and `.m4a`
- [ ] Uniform `AudioSource`: sample rate, channel layout, duration, seekability
- [ ] Feed decoded output through the existing `align_samples` path
- [ ] Feature-gate each format (`wav`, `flac`, `mp3`, `ogg`, `aac`, `all-formats`)
- [ ] Streaming reads for files too large to hold in memory
- [ ] Async decode off the audio thread with a prefetch buffer
- [ ] Memory-mapped reads for instant seek on large files
- [ ] Sample-accurate seek, including in formats with no index
- [ ] Gapless playback: honour encoder delay/padding
- [ ] Fuzz the parsers — decoders are the crate's untrusted-input surface

### 3.2 Uncompressed / lossless

- [ ] **WAV** (RIFF) — PCM u8, i16, i24, i32, f32, f64; `WAVE_FORMAT_EXTENSIBLE`
      for >2ch and channel masks; A-law/µ-law; MS/IMA ADPCM
- [ ] **RF64 / Wave64 (`.w64`)** — the >4 GB variants; long recordings hit this
- [ ] **BWF** — WAV plus the `bext` broadcast chunk (timecode, origination)
- [ ] **AIFF / AIFF-C (`.aif`, `.aiff`, `.aifc`)** — big-endian PCM; AIFF-C adds
      compression and is what a lot of Mac-origin material is
- [ ] **CAF (`.caf`)** — Apple's container; PCM, ALAC, IMA4, AAC. No 4 GB limit
- [ ] **FLAC (`.flac`)** — native stream container
- [ ] **FLAC-in-Ogg (`.oga`)** — same codec, different container
- [ ] **ALAC** — Apple Lossless, normally inside MP4/M4A or CAF, rarely raw
- [ ] **AU / SND (`.au`)** — legacy Sun/NeXT, µ-law and PCM
- [ ] **WavPack (`.wv`)**, **APE (`.ape`)**, **TTA**, **TAK** — niche lossless,
      low priority

### 3.3 Lossy

- [ ] **MP3 (`.mp3`)** — MPEG-1/2/2.5 Layer III; CBR, VBR (Xing/VBRI/LAME
      headers), free-format; ID3v1/v2 skipping; encoder delay + padding
- [ ] **MP1 / MP2 (`.mp2`)** — still standard in broadcast
- [ ] **AAC** — LC, HE-AAC v1 (SBR), HE-AAC v2 (SBR+PS), plus:
  - [ ] ADTS framing (`.aac`) — raw stream, self-describing frames
  - [ ] ADIF — single global header, rare
  - [ ] In MP4/M4A — no framing, config lives in the container's
        `AudioSpecificConfig`
- [ ] **Ogg Vorbis (`.ogg`, `.oga`)** — the three setup headers (ident, comment,
      setup) must be parsed before any audio packet decodes
- [ ] **Opus (`.opus`)** — Ogg Opus; `OpusHead`/`OpusTags`; pre-skip; always
      decodes at 48 kHz regardless of the original rate
- [ ] **Speex (`.spx`)** — deprecated but present in old Ogg files
- [ ] **WMA / WMA Pro / WMA Lossless (`.wma`)** — ASF container
- [ ] **AC-3 / E-AC-3 (`.ac3`, `.eac3`)** — mostly arrives via video containers
- [ ] **DTS (`.dts`, `.dtshd`)** — same
- [ ] **AMR-NB / AMR-WB (`.amr`, `.awb`)** — voice recordings, phone-origin
- [ ] **MPC (`.mpc`)** — low priority

### 3.4 Special

- [ ] **DSD (`.dsf`, `.dff`)** — 1-bit bitstream, not PCM; needs its own
      conversion path, and `SampleType` has no home for it today
- [ ] **Tracker modules (`.mod`, `.xm`, `.it`, `.s3m`)** — these are sequences,
      not audio; only worth it if the engine ever wants a built-in tracker
- [ ] **M3U / PLS / CUE** — playlists and cue sheets, not audio, but needed for
      splitting a single-file album

### 3.5 Encoding / writing

- [ ] WAV (all PCM widths) and RF64 for long captures
- [ ] FLAC encoding for lossless bounce
- [ ] AIFF and CAF writing
- [ ] Opus and Vorbis encoding for compressed export
- [ ] MP3 encoding — check the encoder's licence before shipping it
- [ ] Metadata writing on export: ID3, Vorbis comments, `bext`, loop points, cues

---

## 4. Audio extraction from video containers

Pulling the audio track out of a video file is a **demuxing** problem, not a
video problem. The container interleaves separately-coded tracks; the audio
packets are complete and independent, so the video packets never have to be
parsed as video — only skipped by the byte lengths their headers declare. No
frame decoding, no codec for the video, no image buffers anywhere.

### 4.1 Containers and the audio codecs found inside them

| Container | Extensions | Audio codecs it carries |
| --- | --- | --- |
| ISO BMFF / MP4 | `.mp4`, `.m4v`, `.m4a`, `.m4b` | AAC (LC/HE v1/v2) — overwhelmingly the common case; ALAC; MP3; AC-3, E-AC-3; DTS; FLAC; Opus; PCM (rare) |
| QuickTime | `.mov`, `.qt` | AAC; ALAC; PCM (i16/i24/f32, common in camera and edit-master files); MP3; AC-3; µ-law/A-law; IMA4 |
| Matroska | `.mkv`, `.mka` | Opus; Vorbis; AAC; FLAC; PCM; MP3; AC-3, E-AC-3; DTS, DTS-HD; TrueHD; ALAC — the most permissive of the lot |
| WebM | `.webm` | Vorbis and Opus only (the spec allows nothing else) |
| MPEG-TS | `.ts`, `.m2ts`, `.mts` | AAC (ADTS); AC-3, E-AC-3; MP1/MP2/MP3; DTS; LPCM — broadcast, camera, and HLS segments |
| MPEG-PS | `.mpg`, `.mpeg`, `.vob`, `.evo` | MP2; AC-3; DTS; LPCM — DVD-era |
| AVI (RIFF) | `.avi` | MP3; PCM; AC-3; MP2; occasionally AAC |
| ASF | `.wmv`, `.asf` | WMA, WMA Pro; occasionally MP3 |
| Ogg | `.ogv` | Vorbis; Opus; Speex; FLAC |
| FLV / F4V | `.flv`, `.f4v` | AAC; MP3; Nellymoser; Speex |
| 3GPP / 3GPP2 | `.3gp`, `.3g2` | AAC-LC, HE-AAC; AMR-NB, AMR-WB — phone-origin |
| MXF | `.mxf` | PCM / AES3, usually multiple mono tracks — broadcast masters |
| Fragmented / adaptive | `.m3u8` + segments, DASH `.mpd` | Whatever the segments hold (fMP4 or TS): AAC, AC-3, E-AC-3, Opus |

Everything in that right-hand column is already on the section 3 list — which is
the point of splitting demuxers from decoders. Adding MP4 and Matroska demuxing
gets audio out of most video files without one new decoder.

### 4.2 Steps to extract audio without touching video frames

- [ ] **1. Probe the container.** Identify by magic bytes (`ftyp` at offset 4 for
      ISO BMFF, `0x1A45DFA3` for Matroska, `RIFF….AVI` , `0x47` sync bytes on a
      188-byte stride for MPEG-TS). Do not trust the extension
- [ ] **2. Read the header/index only.** MP4: parse the `moov` atom's track
      boxes. Matroska: the `Tracks` element and `Cues`. TS: the PAT/PMT tables.
      This is metadata, kilobytes, nowhere near the media payload
- [ ] **3. Enumerate tracks and pick the audio one.** A file can have several —
      commentary, other languages, stereo alongside 5.1. Expose them all and
      select on codec, channel count, language tag, and the default/forced flags
      rather than assuming track 1
- [ ] **4. Reject encrypted tracks early.** DRM-protected files (`encv`/`enca`
      sample entries, Widevine/FairPlay) cannot be extracted; fail with a clear
      error instead of emitting noise
- [ ] **5. Pull the codec-private init data.** The audio decoder cannot start
      without it: `AudioSpecificConfig` (`esds`/`mp4a`) for AAC, `OpusHead` for
      Opus, `STREAMINFO` for FLAC, the magic cookie for ALAC, the three setup
      packets for Vorbis. In MP4 and Matroska this sits in the header, not in the
      packet stream
- [ ] **6. Build a packet index for that track alone.** MP4: walk the sample
      tables (`stts`, `stsc`, `stsz`, `stco`/`co64`) to get every audio packet's
      file offset and size. Matroska: use `Cues` plus cluster scanning. TS: filter
      on the audio PID. The result is a list of byte ranges — nothing else in the
      file needs reading
- [ ] **7. Read only those byte ranges.** Audio and video are interleaved, so
      seek from one audio packet to the next and skip the video payload entirely.
      Where a scan is unavoidable (TS, fragmented MP4), read packet headers, check
      the track ID/PID, and skip the declared length without interpreting the
      bytes. **This is the step that keeps video frames out of the process**
- [ ] **8. Re-frame the elementary stream if the codec needs it.** MP4 stores raw
      AAC access units with no framing — either hand them to the decoder with the
      config from step 5 or synthesise ADTS headers. Matroska strips some header
      bytes it expects to be reconstructed (`ContentCompression` header stripping)
- [ ] **9. Convert timestamps.** Each container has its own timescale; convert
      PTS/DTS to sample positions. Handle MP4 edit lists (`elst`) for start
      offsets and gaps, and container-level track offsets — ignoring these puts
      the audio out of sync with picture
- [ ] **10. Trim encoder delay and padding.** AAC carries ~1024–2112 priming
      samples, Opus a `pre_skip` count, MP3 a LAME delay/padding pair. Dropping
      these is what makes extraction sample-exact rather than approximately right
- [ ] **11. Decode packets, or don't.** Two output modes worth having:
  - [ ] *Decode* to PCM for playback and processing
  - [ ] *Passthrough/remux* — write the compressed stream straight into a
        matching container (AAC → `.m4a`, Opus → `.opus`) with no re-encode, no
        quality loss, and near-instant extraction
- [ ] **12. Normalise the output.** Rate, channel count, and layout go through
      the existing `align_samples` path so extracted audio looks like every other
      source to the engine
- [ ] **13. Stream, don't slurp.** A two-hour file's audio track is still
      hundreds of MB decoded. Yield blocks as they are demuxed
- [ ] **14. Handle the awkward layouts.** `moov` at the end of the file (not
      "faststart") means the index is only reachable by seeking to the end;
      fragmented MP4 (`moof`) has no global index at all and must be walked
      fragment by fragment; truncated files should give back whatever decoded
      cleanly rather than an error
- [ ] **15. Multi-track and multi-channel output.** MXF and broadcast files carry
      discrete mono tracks that belong together; offer both "extract one track"
      and "extract and combine into one layout"

### 4.3 Scope note

Where this stops: no video decoding, no frame handling, no muxing video back
out, no transcoding pipeline. The video packets exist only as byte ranges to be
skipped. If picture is ever needed, that is a different crate's job.

## 5. Sample playback and voices

The layer that turns a mixer into an engine.

- [ ] `Sample` type: decoded audio + rate + channel layout, cheap to clone/share
- [ ] `Voice`: one playing instance, independently controlled
- [ ] Per-voice gain, pan, mute/solo
- [ ] Pitch/playback-rate change with interpolation
- [ ] Loop points, crossfaded looping, one-shot vs sustain
- [ ] ADSR envelopes per voice
- [ ] Voice pool with stealing policy and a polyphony cap
- [ ] Start/stop at a sample-accurate future time
- [ ] Fade in/out on start/stop to avoid clicks
- [ ] Reverse playback, start offset

## 6. Signal graph and routing

- [ ] `Node` trait: process a block in place, declare channel counts and latency
- [ ] Graph container with topological sort and cycle detection
- [ ] Atomic graph swap — build off-thread, install between blocks, free off-thread
- [ ] Buses, sends/returns, sidechain inputs
- [ ] Per-node bypass and wet/dry
- [ ] Automatic plugin delay compensation across the graph
- [ ] Channel layouts and speaker arrangements (mono → 5.1/7.1/Atmos beds)
- [ ] Block-size adaptation for nodes wanting a fixed internal block
- [ ] Graph serialization (save/restore a patch)

## 7. Parameters and automation

- [ ] Parameter type with range, skew/curve, step, unit, and display formatting
- [ ] Atomic, RT-safe read from the audio thread; no locks
- [ ] Parameter smoothing (linear/exponential ramps) to kill zipper noise
- [ ] Normalized (0..1) ↔ real-value conversion for host automation
- [ ] Automation curves and sample-accurate parameter changes within a block
- [ ] Change listeners/notification for the control side
- [ ] Undo/redo history over parameter state
- [ ] Parameter groups and hierarchy

## 8. State, presets, serialization

- [ ] Serialize/deserialize full engine or plugin state
- [ ] Preset format, plus factory-vs-user preset directories
- [ ] Versioned state with migration on load
- [ ] A tree-shaped, observable value store as the backing model
- [ ] Import/export of common preset interchange formats

## 9. DSP toolbox

Currently nothing beyond `mix` and linear interpolation.

- [ ] Biquad filters (LP/HP/BP/notch/shelf/peak) with coefficient smoothing
- [ ] State-variable and ladder filters
- [ ] FIR/IIR abstractions and a design helper
- [ ] FFT wrapper, window functions, overlap-add/save framework
- [ ] Fast convolution and a partitioned convolver for long IRs
- [ ] Oscillators, band-limited (PolyBLEP/wavetable), plus LFOs and noise
- [ ] Envelope followers, ADSR, gate/trigger
- [ ] Dynamics primitives: compressor, limiter, gate, expander
- [ ] Delay lines with fractional interpolation, all-pass, comb
- [ ] Reverb (algorithmic and convolution)
- [ ] Waveshaping/saturation with oversampling
- [ ] Oversampling framework (up/downsample around a nonlinear block)
- [ ] Band-limited resampling to replace the current linear interpolation
      (sinc/polyphase; today's version aliases when downsampling)
- [ ] Time-stretch and pitch-shift, tempo-independent
- [ ] dB↔linear, pan laws, gain smoothing, interpolation, denormal helpers
- [ ] Fixed-size SIMD-friendly buffer types

## 10. Built-in effects (`plugins::internal`)

- [ ] Gain / trim / phase invert
- [ ] EQ (parametric, graphic)
- [ ] Compressor / limiter / gate
- [ ] Delay, chorus, flanger, phaser
- [ ] Reverb
- [ ] Distortion / saturation
- [ ] Stereo width, mid/side encode/decode
- [ ] Metering taps that don't alter the signal

## 11. Plugin hosting

Internal, VST3, and AU (v2 and v3) load and process. Everything below the
first block is still open.

- [x] Internal plugins: a Rust function mapped in at load, applied per block,
      with a name registry so a chain can be described by configuration
- [x] Common hosting path so formats are interchangeable at the call site —
      `plugins::host::Hosted`, over `truce-rack-core`'s traits
- [x] VST3 hosting: scan, instantiate, activate, process
- [x] AU hosting (macOS/iOS), v2 and v3
- [ ] Plugin scanning as a first-class API: filesystem walk, per-platform
      standard paths, cached index. Loading scans one path today, which is
      enough to load a known plugin and not enough to browse
- [ ] Out-of-process scanning so a bad plugin can't take down the scan
- [ ] Parameter and bus handling — `params` on the descriptor is still an
      unparsed `String` and reaches no plugin
- [ ] Re-activation when the channel count or block size changes, rather than
      the current error
- [ ] VST2 hosting — no backend crate exists and the SDK licence is unresolved;
      `PluginFormat::Vst` refuses on every build until both change
- [ ] CLAP hosting — modern, permissively licensed, no SDK obligations
- [ ] LV2 hosting (Linux reach)
- [ ] Plugin state save/restore and preset switching
- [ ] Parameter enumeration, automation, and host↔plugin notification
- [ ] Latency reporting from plugins into the graph's compensation. Internal
      plugins declare theirs; the hosted formats report zero because
      `truce-rack-core` exposes no latency on its traits
- [ ] Bus/channel-layout negotiation
- [ ] Sandboxed/bridged hosting (crash isolation, 32↔64-bit bridging)
- [ ] Editor lifecycle hooks — the window is the GUI layer's problem, but
      open/close/resize plumbing is not

## 12. Plugin authoring

Distinct from hosting, and the harder half: one implementation of an audio
processor, exported to every format.

- [ ] `AudioProcessor` trait: prepare, process, release, state, parameters
- [ ] VST3 wrapper — export a processor as a `.vst3`
- [ ] AU wrapper — export as an `.component` / AUv3 app extension
- [ ] CLAP wrapper
- [ ] Standalone application wrapper (device picker, no host required)
- [ ] Parameter declaration that generates each format's parameter model
- [ ] Host-provided transport, tempo, and time-signature plumbing
- [ ] Bus layout declaration and negotiation
- [ ] Bundle generation: correct directory layout, `Info.plist`, resources
- [ ] Code signing and notarization helpers (macOS)
- [ ] Installer generation per platform
- [ ] Validator/self-test pass before shipping a build

## 13. Transport, timing, sync

- [ ] Transport: play/stop/record, position in samples/seconds/bars+beats
- [ ] Tempo map and time signature, including changes mid-timeline
- [ ] Musical-time ↔ sample-time conversion
- [ ] Sample-accurate event scheduling queue with a lookahead window
- [ ] Host transport sync when running as a plugin
- [ ] Latency/roundtrip measurement and reporting
- [ ] Ableton Link or equivalent network tempo sync

## 14. Real-time safety

The architecture is already lock-free at the callback; this is about proving and
keeping it.

- [ ] Denormal handling (FTZ/DAZ) on the audio thread
- [ ] Thread priority elevation / workgroup joining for the audio thread
- [ ] RT-safe allocator or pre-allocated pools for anything the RT path touches
- [ ] Allocation-detection harness that fails tests on an RT-thread allocation
- [ ] Documented RT-safety contract per public function
- [ ] Bounded-time guarantees for every callback-side operation
- [ ] Xrun/underrun counters exposed for monitoring

## 15. Device and stream management

- [ ] Device hot-plug detection and graceful migration
- [ ] Handle sample-rate or buffer-size changes at runtime
- [ ] Recover from device disconnect without tearing down engine state
- [ ] Persist and restore a device configuration by identity, not index
- [ ] Buffer-size and sample-rate negotiation with fallback ladder
- [ ] Multi-device aggregation with drift correction
- [ ] Exclusive/low-latency modes per host (WASAPI exclusive, etc.)
- [ ] Finish the ASIO path beyond host lookup: channel naming, control panel

## 16. Metering and analysis

- [ ] Peak and RMS meters with configurable ballistics
- [ ] True-peak (oversampled) detection
- [ ] LUFS loudness (momentary/short-term/integrated) and dynamic range
- [ ] Correlation/phase metering
- [ ] Spectrum analysis feed for visualisation
- [ ] Waveform/peak-file generation for large files
- [ ] Lock-free metering handoff to the control thread

## 17. Control protocols and interop

- [ ] OSC send/receive
- [ ] Audio-over-network transport
- [ ] Control-surface abstraction (HUI/Mackie or a native mapping layer)
- [ ] Remote control API for headless/embedded operation

## 18. Concurrency and API surface

- [ ] Actually deliver the async control API the README describes, or drop the
      claim — there is no async runtime integration today
- [ ] Runtime-agnostic async (Tokio/async-std/smol) behind features
- [ ] Background worker pool for decode, analysis, and file I/O
- [ ] A cloneable, `Send + Sync` engine handle usable from any task
- [ ] Command/acknowledgement protocol with backpressure that never blocks the
      audio thread

## 19. Quality and release engineering

- [ ] Unit tests across mixer, alignment, graph, and DSP
- [ ] Integration tests with a null/offline device for deterministic runs
- [ ] Null tests for DSP correctness against reference output
- [ ] Benchmarks with regression thresholds on the RT path
- [ ] Fuzzing for file decoders and MIDI/SysEx parsers
- [ ] Cross-platform CI (macOS/Windows/Linux) across the feature matrix
- [ ] `#![deny(missing_docs)]` and a doc pass on public API
- [ ] Examples that compile and run: playback, capture, effect chain, plugin host
- [ ] Semantic versioning discipline and a changelog

---

## Suggested order

1. **Section 0** — nothing is verifiable until the crate builds
2. **Sections 3 + 5** — file loading and voices make it usable for real work
3. **Section 4** — cheap once 3 exists: the demuxers are new, the decoders are
   the same ones. Do it right after, while the container/codec split is fresh
4. **Sections 6 + 7** — graph and parameters are the spine everything else hangs
   on
5. **Section 2** — MIDI, without which instrument work is impossible
6. **Section 9** — DSP toolbox, needed by both hosting and authoring
7. **Section 11, then 12** — hosting first (smaller, validates the processor
   abstraction), then authoring
8. **Sections 14 + 19 continuously**, not as a phase

## Deliberately not on this list

- GUI/UI, per the brief
- General-purpose platform utilities (strings, JSON/XML, filesystem abstraction,
  networking, cryptography, image handling) that large C++ frameworks bundle
  because the C++ standard library and ecosystem left gaps. Rust's crate
  ecosystem already covers these, and pulling them in would make the crate worse,
  not more complete.
