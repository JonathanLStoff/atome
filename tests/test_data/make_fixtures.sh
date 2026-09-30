#!/bin/sh
# Makes every fixture the decode tests look for, beside this script. Anything
# the tests find missing they skip, so without these most of tests/decode.rs and
# tests/native_type.rs quietly does nothing — the docker/ images run this first
# for exactly that reason.
#
#   sh tests/test_data/make_fixtures.sh
#
# Needs ffmpeg with libmp3lame, libvorbis, libopus, and libx264, which the
# ffmpeg packages of Debian, Ubuntu, Alpine, RPM Fusion, Homebrew, and gyan.dev
# all have. Only test415hz.mp3 is committed; everything here is regenerated.

set -eu

cd "$(dirname "$0")"

make() {
    output="$1"
    shift
    ffmpeg -hide_banner -loglevel error -y "$@" "$output"
}

# The library encoder where this ffmpeg has it, else ffmpeg's own — which for
# Vorbis and Opus is marked experimental but writes a valid stream. Homebrew's
# ffmpeg, for one, has no libvorbis.
encoder() {
    library="$1" native="$2"
    if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " $library "; then
        echo "$library"
    else
        echo "$native -strict experimental"
    fi
}

vorbis="$(encoder libvorbis vorbis)"
opus="$(encoder libopus opus)"

# One second of 440 Hz, stereo, 48 kHz: the source of everything else.
make tone.wav -f lavfi -i "sine=frequency=440:duration=1:sample_rate=48000" -ac 2

# PCM in every width the native-type tests check.
make tone8.wav -i tone.wav -c:a pcm_u8
make tone24.wav -i tone.wav -c:a pcm_s24le
make tone32.wav -i tone.wav -c:a pcm_s32le
make tone32f.wav -i tone.wav -c:a pcm_f32le
make tone.aiff -i tone.wav -c:a pcm_s16be
make tone.caf -i tone.wav -c:a pcm_s16le

# Lossless.
make tone.flac -i tone.wav -c:a flac
make tone24.flac -i tone.wav -c:a flac -sample_fmt s32
make tone_alac.m4a -i tone.wav -c:a alac

# Lossy, in each container the decoders are meant to reach.
make tone.mp3 -i tone.wav -c:a libmp3lame
# shellcheck disable=SC2086
make tone.ogg -i tone.wav -c:a $vorbis
make tone_aac.m4a -i tone.wav -c:a aac
make tone.aac -i tone.wav -c:a aac -f adts
# shellcheck disable=SC2086
make tone.opus -i tone.wav -c:a $opus
# shellcheck disable=SC2086
make tone_opus.mkv -i tone.wav -c:a $opus

# A video with an AAC soundtrack: what export::to_flac is for.
make tone_video.mp4 -f lavfi -i "color=c=black:s=160x120:d=1" -i tone.wav \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest

echo "fixtures in $(pwd)"
