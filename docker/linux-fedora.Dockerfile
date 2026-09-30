# atome's Linux test image, Fedora. Built and run by docker/run.sh; see
# linux-debian.Dockerfile for what is in it and why.
#
# Fedora's own ffmpeg leaves out the encumbered encoders the fixtures need, so
# the converters come from RPM Fusion — only for making fixtures; atome itself
# never links ffmpeg.

FROM fedora:41

RUN dnf install -y \
        https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-41.noarch.rpm \
    && dnf install -y --allowerasing \
        ca-certificates curl git gcc gcc-c++ make pkgconf-pkg-config clang cmake \
        autoconf automake libtool \
        alsa-lib-devel alsa-utils \
        ffmpeg \
    && dnf clean all

COPY asound.conf /etc/asound.conf

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

ENV PATH=/root/.cargo/bin:$PATH \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "linux"]
