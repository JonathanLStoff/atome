# atome's Linux test image, Debian family: `BASE=debian:bookworm` or
# `BASE=ubuntu:24.04`. Built and run by docker/run.sh.
#
# Everything the default toolkit and its feature matrix need from the system,
# and the converters that make the test fixtures:
#
#   - a Rust toolchain, and a C toolchain for the libopus and libfdk-aac
#     features, which build those libraries from source
#   - ALSA's headers for cpal, and a null ALSA device as the default, so the
#     device tests open a real (silent) output and input instead of skipping
#   - ffmpeg with MP3, Vorbis, Opus, AAC, ALAC, FLAC, and x264 — the converters
#     tests/test_data/make_fixtures.sh uses, so every decode test has its file
#
# The source is mounted read-only at /src and copied, so fixtures made here
# never land in the working tree.

ARG BASE=debian:bookworm
FROM ${BASE}

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config clang cmake \
        autoconf automake libtool \
        libasound2-dev alsa-utils \
        ffmpeg \
    && rm -rf /var/lib/apt/lists/*

COPY asound.conf /etc/asound.conf

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

ENV PATH=/root/.cargo/bin:$PATH \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "linux"]
