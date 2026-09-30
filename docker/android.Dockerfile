# atome for Android, cross-built with the NDK. Built and run by docker/run.sh.
#
# Build only: running the tests needs an emulator or a device, which is past
# what a container does well. What this proves is that the default toolkit, the
# decoders, and the FLAC writer compile and link for aarch64-linux-android —
# which nothing else here would notice breaking, since cpal's Android path is
# `cfg`-split.

# The NDK ships x86-64 Linux binaries only, so this is an amd64 container even
# on an arm64 host.
FROM --platform=linux/amd64 debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git unzip build-essential pkg-config python3 \
    && rm -rf /var/lib/apt/lists/*

ARG NDK=r27c
RUN curl -sSfL -o /tmp/ndk.zip https://dl.google.com/android/repository/android-ndk-${NDK}-linux.zip \
    && unzip -q /tmp/ndk.zip -d /opt \
    && mv /opt/android-ndk-${NDK} /opt/ndk \
    && rm /tmp/ndk.zip

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
    && /root/.cargo/bin/rustup target add aarch64-linux-android

# The NDK's own clang for API level 26, the oldest with AAudio — which is
# what cpal plays through on Android.
ENV PATH=/root/.cargo/bin:$PATH \
    ANDROID_NDK_HOME=/opt/ndk \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never \
    CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=/opt/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang \
    CC_aarch64_linux_android=/opt/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang \
    CXX_aarch64_linux_android=/opt/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang++ \
    AR_aarch64_linux_android=/opt/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "android"]
