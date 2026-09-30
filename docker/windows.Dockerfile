# escape=`

# atome on real Windows, in a Windows container. Needs a Windows host with
# Docker in Windows-container mode — it cannot run on macOS or Linux, where
# docker/run.sh uses windows-cross.Dockerfile (Wine) instead.
#
#   docker build -t atome-test-windows -f docker\windows.Dockerfile docker
#   docker run --rm -v "${PWD}:C:\src:ro" atome-test-windows
#
# The MSVC toolchain, Rust, and ffmpeg (the converters, from gyan.dev's build).
# A Windows container has no sound card, so the device tests skip with a
# message; everything else, fixtures included, runs as it would on a desktop.

FROM mcr.microsoft.com/windows/servercore:ltsc2022

SHELL ["powershell", "-NoProfile", "-Command", "$ErrorActionPreference = 'Stop';"]

# The C++ build tools rustc links with.
RUN Invoke-WebRequest https://aka.ms/vs/17/release/vs_buildtools.exe -OutFile C:\vs_buildtools.exe; `
    Start-Process C:\vs_buildtools.exe -Wait -ArgumentList '--quiet', '--wait', '--norestart', '--nocache', `
        '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended'; `
    Remove-Item C:\vs_buildtools.exe

RUN Invoke-WebRequest https://win.rustup.rs/x86_64 -OutFile C:\rustup-init.exe; `
    C:\rustup-init.exe -y --profile minimal --default-toolchain stable; `
    Remove-Item C:\rustup-init.exe

RUN Invoke-WebRequest https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip -OutFile C:\ffmpeg.zip; `
    Expand-Archive C:\ffmpeg.zip C:\; `
    Move-Item C:\ffmpeg-*-essentials_build C:\ffmpeg; `
    Remove-Item C:\ffmpeg.zip

RUN setx /M PATH $($env:PATH + ';C:\Users\ContainerAdministrator\.cargo\bin;C:\ffmpeg\bin')

ENV CARGO_TARGET_DIR=C:\target CARGO_TERM_COLOR=never

WORKDIR C:\work
CMD ["powershell", "-NoProfile", "-File", "C:\\src\\docker\\test.ps1"]
