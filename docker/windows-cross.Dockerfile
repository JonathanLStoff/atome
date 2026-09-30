# atome on Windows, from a Linux container: cross-compiled with MinGW for
# x86_64-pc-windows-gnu, and the tests run under Wine. Built and run by
# docker/run.sh, always as linux/amd64 — Wine runs x86-64 Windows binaries only
# on an x86-64 host, emulated on Apple Silicon.
#
# Wine is Windows' API, not Windows: what this proves is that the Windows build
# compiles, links, and passes everything that does not need a real sound card
# (the device tests skip under Wine). windows.Dockerfile, on a Windows host,
# is the real thing. ASIO is left out — it needs Steinberg's SDK.

FROM --platform=linux/amd64 debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive

RUN dpkg --add-architecture i386 && apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config cmake \
        gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 \
        wine wine64 \
        ffmpeg \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
    && /root/.cargo/bin/rustup target add x86_64-pc-windows-gnu

ENV PATH=/root/.cargo/bin:$PATH \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine \
    WINEDEBUG=-all \
    WINEPREFIX=/wine

RUN wineboot --init && wineserver --wait

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "windows"]
