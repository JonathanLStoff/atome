# atome's Linux test image, Alpine: musl rather than glibc. Built and run by
# docker/run.sh; see linux-debian.Dockerfile for what is in it and why.

FROM alpine:3.20

RUN apk add --no-cache \
        ca-certificates curl git bash tar build-base pkgconf clang cmake \
        autoconf automake libtool \
        alsa-lib-dev alsa-utils \
        ffmpeg

COPY asound.conf /etc/asound.conf

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

# cpal finds ALSA by dlopen-free linking, but the C libraries the codec
# features build are shared objects; a static musl binary would not load them.
ENV PATH=/root/.cargo/bin:$PATH \
    RUSTFLAGS="-C target-feature=-crt-static" \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "linux"]
