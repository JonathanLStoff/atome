#!/bin/sh
# atome's test matrix, for one kind of system. Every image's entry point, and
# run directly on a Mac for the systems no container can be:
#
#   sh docker/test.sh linux     # Debian, Ubuntu, Fedora, Alpine images
#   sh docker/test.sh windows   # windows-cross image: MinGW build, tests under Wine
#   sh docker/test.sh android   # android image: NDK cross-build
#   sh docker/test.sh macos     # a Mac, natively — plus the iOS cross-build
#
# The converters come first: tests/test_data/make_fixtures.sh has ffmpeg make
# every fixture the decode tests want, so none of them skips for lack of a file.
# Every step runs even when an earlier one fails, and the summary at the end
# says which did what. Exits non-zero if any step failed.
#
# Always works on a copy of the source — /src in a container, the checkout on a
# Mac — so the fixtures never reach the working tree. On a Mac the copy shares
# the checkout's target directory, so nothing is rebuilt that need not be.

set -u

profile="${1:-linux}"

if [ -d /src ]; then
    source=/src
else
    source="$(cd "$(dirname "$0")/.." && pwd)"
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$source/target}"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tar -C "$source" --exclude=./target --exclude=./.git -cf - . | tar -C "$work" -xf -
cd "$work"

results=""
failed=0

step() {
    name="$1"
    shift
    echo ""
    echo "==> $name: $*"
    if "$@"; then
        results="$results
  PASS  $name"
    else
        results="$results
  FAIL  $name"
        failed=1
    fi
}

skip() {
    echo ""
    echo "==> $1: skipped — $2"
    results="$results
  SKIP  $1 ($2)"
}

converters() {
    ffmpeg -hide_banner -version | head -1 && sh tests/test_data/make_fixtures.sh
}

case "$profile" in
    linux)
        step converters converters
        step default cargo test
        # The pure-Rust decoders and the FLAC writer, against every fixture.
        step import-export cargo test --features import,export
        # Opus and HE-AAC through their C libraries, built from source.
        step import-all cargo test --features import-all,export
        step examples cargo check --features import,export --all-targets
        step plugins cargo check --features vst,vst3
        ;;

    windows)
        target=x86_64-pc-windows-gnu
        step converters converters
        step build cargo build --target "$target" --features import,export
        # Under Wine: there is no sound card, so the device tests skip.
        step default cargo test --target "$target"
        step import-export cargo test --target "$target" --features import,export
        ;;

    android)
        target=aarch64-linux-android
        step default cargo build --target "$target"
        step import-export cargo build --target "$target" --features import,export
        ;;

    macos)
        step converters converters
        step default cargo test
        step import-export cargo test --features import,export
        step import-all cargo test --features import-all,export
        step examples cargo check --features import,export --all-targets
        step plugins cargo check --features plugins
        if rustup target list --installed 2>/dev/null | grep -q aarch64-apple-ios; then
            step ios cargo build --target aarch64-apple-ios --features import,export
        else
            skip ios "rustup target add aarch64-apple-ios to include it"
        fi
        ;;

    *)
        echo "unknown profile '$profile': linux, windows, android, or macos" >&2
        exit 2
        ;;
esac

echo ""
echo "atome on $profile$( [ -f /etc/os-release ] && . /etc/os-release && printf ' (%s)' "$PRETTY_NAME"):$results"

exit "$failed"
