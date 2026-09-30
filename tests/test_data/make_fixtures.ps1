# make_fixtures.sh for Windows without a POSIX shell: the same fixtures, made
# with the same ffmpeg commands, beside this script.
#
#   powershell -File tests\test_data\make_fixtures.ps1

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Make($output) {
    & ffmpeg -hide_banner -loglevel error -y @args $output
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg could not make $output" }
}

# The library encoder where this ffmpeg has it, else ffmpeg's own.
function Encoder($library, $native) {
    if ((& ffmpeg -hide_banner -encoders 2>$null) -match " $library ") { return @($library) }
    return @($native, '-strict', 'experimental')
}

$vorbis = Encoder 'libvorbis' 'vorbis'
$opus = Encoder 'libopus' 'opus'

Make tone.wav -f lavfi -i 'sine=frequency=440:duration=1:sample_rate=48000' -ac 2

Make tone8.wav -i tone.wav -c:a pcm_u8
Make tone24.wav -i tone.wav -c:a pcm_s24le
Make tone32.wav -i tone.wav -c:a pcm_s32le
Make tone32f.wav -i tone.wav -c:a pcm_f32le
Make tone.aiff -i tone.wav -c:a pcm_s16be
Make tone.caf -i tone.wav -c:a pcm_s16le

Make tone.flac -i tone.wav -c:a flac
Make tone24.flac -i tone.wav -c:a flac -sample_fmt s32
Make tone_alac.m4a -i tone.wav -c:a alac

Make tone.mp3 -i tone.wav -c:a libmp3lame
Make tone.ogg -i tone.wav -c:a @vorbis
Make tone_aac.m4a -i tone.wav -c:a aac
Make tone.aac -i tone.wav -c:a aac -f adts
Make tone.opus -i tone.wav -c:a @opus
Make tone_opus.mkv -i tone.wav -c:a @opus

Make tone_video.mp4 -f lavfi -i 'color=c=black:s=160x120:d=1' -i tone.wav `
    -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest

Write-Host "fixtures in $PSScriptRoot"
