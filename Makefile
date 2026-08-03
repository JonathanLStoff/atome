# Build and run atome's examples.
#
#   make            list every target
#   make example1   build and run one, with the features it needs
#   make examples   run every example that needs no audio hardware
#
# Each example gets its own target rather than one parameterised rule, because
# each has its own requirements: a feature to enable, a tool to have installed,
# a fixture to be present. A shared rule would have to check the union of all
# of them for every example.

CARGO ?= cargo
# Passed through to the playback example: make example-play FILE=~/song.flac
FILE ?=
# The version to cut: make release v=0.9.0
v ?=

# Anything that reads or writes audio devices is release-built by default:
# a debug-built decoder is slow enough to underrun on a small buffer.
PROFILE ?= --release

.DEFAULT_GOAL := help
.PHONY: help examples example1 example2 example3 example4 example-play \
        build build-all test check doc clean release \
        require-cargo require-manifest require-import require-native require-fixture \
        require-version

# --- checks ---------------------------------------------------------------
# Each is a guard the example targets depend on. They fail with a sentence
# saying what to do, rather than letting cargo fail with something longer.

require-cargo:
	@command -v $(CARGO) >/dev/null 2>&1 || { \
		echo "error: $(CARGO) not found. Install Rust from https://rustup.rs"; \
		exit 1; }

# The manifest has to parse before any feature check means anything.
require-manifest: require-cargo
	@$(CARGO) metadata --no-deps --format-version 1 >/dev/null 2>&1 || { \
		echo "error: Cargo.toml does not parse. Run '$(CARGO) metadata' to see why."; \
		exit 1; }

# The `import` feature must exist and be spelled the way these targets spell it.
require-import: require-manifest
	@$(CARGO) metadata --no-deps --format-version 1 \
		| grep -q '"import"' || { \
		echo "error: no 'import' feature in Cargo.toml — the decoders live behind it."; \
		exit 1; }

# `import-he-aac` and `import-opus` build libfdk-aac and libopus from source.
require-native: require-manifest
	@command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1 || { \
		echo "error: no C compiler. import-he-aac and import-opus build C libraries from source."; \
		exit 1; }
	@command -v cmake >/dev/null 2>&1 || { \
		echo "error: cmake not found, and libfdk-aac needs it. Try 'brew install cmake'."; \
		exit 1; }

require-fixture:
	@test -n "$(FILE)" -o -f tests/test_data/test415hz.mp3 || { \
		echo "error: tests/test_data/test415hz.mp3 is missing."; \
		echo "       Pass one instead: make example-play FILE=/path/to/audio.flac"; \
		exit 1; }

# `v` is the whole input to `make release`, so it is checked before anything is
# rewritten — a half-applied version bump is worse than a refusal.
require-version:
	@test -n "$(v)" || { \
		echo "error: no version. Usage: make release v=0.9.0"; \
		exit 1; }
	@printf '%s' '$(v)' \
		| grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$$' || { \
		echo "error: '$(v)' is not a semver version (x.y.z)"; \
		exit 1; }
	@test -f CHANGELOG.md || { \
		echo "error: no CHANGELOG.md to cut a release section in."; \
		exit 1; }
	@grep -Fqx '## Release $(v)' CHANGELOG.md && { \
		echo "error: CHANGELOG.md already has a 'Release $(v)' section."; \
		exit 1; } || true
	@# There has to be something to release, and it has to be checked here
	@# rather than mid-recipe: by then the version bump has already landed and
	@# a failure would leave the tree half-released.
	@awk ' \
		state == "pending" && /^## Release / { state = "tail" } \
		state == "pending" && NF { found = 1 } \
		state == "" && /^\*\*Done\*\*[[:space:]]*$$/ { state = "pending" } \
		END { if (state == "") exit 2; if (!found) exit 3 } \
	' CHANGELOG.md; \
	case $$? in \
		0) ;; \
		2) echo "error: CHANGELOG.md has no '**Done**' line to move items out of."; exit 1 ;; \
		3) echo "error: nothing under '**Done**' in CHANGELOG.md — no release to cut."; exit 1 ;; \
		*) echo "error: could not read CHANGELOG.md"; exit 1 ;; \
	esac

# --- examples -------------------------------------------------------------

## example1: random input to random output, no routing, no plugins
example1: require-manifest
	@echo "==> audio_engine1: random input -> random output"
	$(CARGO) run $(PROFILE) --example audio_engine1

## example2: one input tied to one specific output
example2: require-manifest
	@echo "==> audio_engine2: input tied to a named output"
	$(CARGO) run $(PROFILE) --example audio_engine2

## example3: several outputs with different channel counts, partial routing
example3: require-manifest
	@echo "==> audio_engine3: [2, 5] channel outputs, one input routed, one not"
	$(CARGO) run $(PROFILE) --example audio_engine3

## example4: plugins at all three levels
example4: require-manifest
	@echo "==> audio_engine4: plugin chains on input, engine, and output"
	$(CARGO) run $(PROFILE) --example audio_engine4

## example-play: decode a file and play it. Makes sound. Needs the import feature
example-play: require-import require-fixture
	@echo "==> play_file: decode and play (this one you can hear)"
	$(CARGO) run $(PROFILE) --features import --example play_file -- $(FILE)

## example-play-all: as above, with the C-backed decoders (Opus, HE-AAC)
example-play-all: require-import require-native require-fixture
	@echo "==> play_file with every decoder (builds libopus and libfdk-aac)"
	$(CARGO) run $(PROFILE) --features import-all --example play_file -- $(FILE)

## examples: every example that needs no particular hardware and makes no sound
examples: example1 example2 example3 example4

# --- build and test -------------------------------------------------------

## build: compile the library with no features
build: require-manifest
	$(CARGO) build $(PROFILE)

## build-all: compile the library and every example, across the feature sets
build-all: require-import
	$(CARGO) build $(PROFILE) --examples
	$(CARGO) build $(PROFILE) --features import --examples

## test: run the test suite with decoding enabled
test: require-import
	$(CARGO) test --features import

## check: compile-check every feature combination that does not need C tooling
check: require-import
	$(CARGO) check
	$(CARGO) check --features import
	$(CARGO) check --all-targets --features import

## doc: build the documentation
doc: require-manifest
	$(CARGO) doc --no-deps --features import

## clean: remove build artefacts
clean:
	$(CARGO) clean

# --- release --------------------------------------------------------------
# Prepares a release in the working tree and stops there. Nothing here commits,
# tags, or pushes: `.github/workflows/publish.yml` publishes off a `rel-<v>`
# branch, so pushing is the one irreversible step and stays a deliberate act.

## release: set the version everywhere and cut a CHANGELOG section (make release v=0.9.0)
release: require-version require-manifest
	@# Only the [package] version. A blanket substitution would also rewrite
	@# the dependency versions further down the manifest.
	@awk -v ver='$(v)' ' \
		/^\[/ { in_pkg = ($$0 == "[package]") } \
		in_pkg && !done && /^version[[:space:]]*=/ { \
			print "version = \"" ver "\""; done = 1; next } \
		{ print } \
	' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
	@grep -Fqx 'version = "$(v)"' Cargo.toml || { \
		echo "error: Cargo.toml still does not say $(v) — is there a [package] version line?"; \
		exit 1; }
	@# The README's install snippets are the other place the version is
	@# written by hand, in both the bare and the with-features form.
	@sed -i.bak -E \
		-e 's/^atome = "[^"]*"/atome = "$(v)"/' \
		-e 's/^atome = \{ version = "[^"]*"/atome = { version = "$(v)"/' \
		README.md && rm -f README.md.bak
	@# Cargo.lock records the crate's own version too; resolving rewrites it.
	@$(CARGO) metadata --format-version 1 >/dev/null
	@# Everything under **Done** becomes the new release section, leaving
	@# **Done** empty for the next cycle. Exit codes rather than messages,
	@# because awk's stdout is the file being written.
	@awk -v ver='$(v)' ' \
		BEGIN { state = "head" } \
		state == "head" { \
			print; \
			if ($$0 ~ /^\*\*Done\*\*[[:space:]]*$$/) state = "pending"; \
			next } \
		state == "pending" && /^## Release / { state = "tail" } \
		state == "pending" { pending = pending $$0 "\n"; next } \
		{ tail = tail $$0 "\n" } \
		END { \
			if (state == "head") exit 2; \
			sub(/^\n+/, "", pending); sub(/\n+$$/, "", pending); \
			if (pending == "") exit 3; \
			print ""; print "## Release " ver; print ""; print pending; \
			if (tail != "") { sub(/\n+$$/, "", tail); print ""; print tail } \
		} \
	' CHANGELOG.md > CHANGELOG.md.tmp; \
	code=$$?; \
	if [ $$code -ne 0 ]; then rm -f CHANGELOG.md.tmp; fi; \
	case $$code in \
		0) mv CHANGELOG.md.tmp CHANGELOG.md ;; \
		2) echo "error: CHANGELOG.md has no '**Done**' line to move items out of."; exit 1 ;; \
		3) echo "error: nothing under '**Done**' in CHANGELOG.md — no release to cut."; exit 1 ;; \
		*) exit $$code ;; \
	esac
	@echo "==> $(v): Cargo.toml, Cargo.lock, README.md, CHANGELOG.md"
	@echo ""
	@echo "  Review the diff, then:"
	@echo "    git checkout -b rel-$(v) && git commit -am 'release $(v)' && git push -u origin rel-$(v)"
	@echo ""
	@echo "  The push publishes to crates.io and opens the GitHub release."

# --- help -----------------------------------------------------------------

help:
	@echo "atome examples"
	@echo ""
	@grep -E '^## ' $(MAKEFILE_LIST) \
		| sed -e 's/^## //' -e 's/:/:|/' \
		| awk -F'|' '{ printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 }'
	@echo ""
	@echo "  Only 'example-play' makes any sound. The engine examples build real"
	@echo "  streams and print how they are wired; carrying captured audio from an"
	@echo "  input to its outputs is not implemented yet (planning/TODO.md 2.3)."
