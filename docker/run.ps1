# run.sh for a Windows host without a POSIX shell. Same OSes, same rules:
#
#   powershell -File docker\run.ps1                   # everything this host can run
#   powershell -File docker\run.ps1 debian alpine     # just these
#
# With Docker in Linux-container mode (Docker Desktop's default) the Linux
# images run and `windows` is skipped; switched to Windows-container mode,
# `windows` runs and the Linux images are skipped. `macos` is always skipped
# here — no container can be macOS.

$ErrorActionPreference = 'Continue'
Set-Location (Split-Path $PSScriptRoot)

$crate = 'atome'
$allOses = 'debian', 'ubuntu', 'fedora', 'alpine', 'windows-cross', 'android', 'macos', 'windows'
$oses = if ($args.Count -gt 0) { $args } else { $allOses }

$summary = @()
$failed = $false
$dockerOs = (docker version --format '{{.Server.Os}}' 2>$null)
$native = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'linux/arm64' } else { 'linux/amd64' }
$source = (Get-Location).Path

function Pass($what) { $script:summary += "  PASS  $what" }
function Fail($what) { $script:summary += "  FAIL  $what"; $script:failed = $true }
function Skip($what) { $script:summary += "  SKIP  $what" }

# Whether an x86-64 container runs here. Always on x86-64 Windows; on ARM64
# Windows, Docker Desktop emulates it. Asked once.
$script:amd64 = $null
function CanRunAmd64 {
    if ($null -eq $script:amd64) {
        if ($native -eq 'linux/amd64') { $script:amd64 = $true }
        else {
            docker run --rm --platform linux/amd64 alpine:3.20 true 2>$null | Out-Null
            $script:amd64 = ($LASTEXITCODE -eq 0)
        }
    }
    return $script:amd64
}

function InDocker($os, $dockerfile, $platform, $testProfile, [string[]]$buildArgs) {
    if ($dockerOs -ne 'linux') {
        if (-not $dockerOs) { Fail "$os (Docker is not running)" }
        else { Skip "$os (Docker is in $dockerOs-container mode; Linux images need Linux-container mode)" }
        return
    }

    if ($platform -eq 'linux/amd64' -and -not (CanRunAmd64)) {
        Skip "$os (x86-64 only, and this $native host cannot emulate it)"
        return
    }

    $image = "$crate-test-$os"
    Write-Host "`n######## ${os}: building $image"
    docker build --platform $platform -t $image -f "docker/$dockerfile" @buildArgs docker
    if ($LASTEXITCODE -ne 0) { Fail "$os (image would not build)"; return }

    Write-Host "######## ${os}: testing"
    docker run --rm --platform $platform `
        -v "${source}:/src:ro" `
        -v "$crate-cargo-registry:/root/.cargo/registry" `
        -v "$crate-target-${os}:/target" `
        $image sh /src/docker/test.sh $testProfile
    if ($LASTEXITCODE -eq 0) { Pass $os } else { Fail $os }
}

foreach ($os in $oses) {
    switch ($os) {
        'debian' { InDocker 'debian' 'linux-debian.Dockerfile' $native 'linux' @('--build-arg', 'BASE=debian:bookworm') }
        'ubuntu' { InDocker 'ubuntu' 'linux-debian.Dockerfile' $native 'linux' @('--build-arg', 'BASE=ubuntu:24.04') }
        'fedora' { InDocker 'fedora' 'linux-fedora.Dockerfile' $native 'linux' @() }
        'alpine' { InDocker 'alpine' 'linux-alpine.Dockerfile' $native 'linux' @() }
        'windows-cross' { InDocker 'windows-cross' 'windows-cross.Dockerfile' 'linux/amd64' 'windows' @() }
        'android' { InDocker 'android' 'android.Dockerfile' 'linux/amd64' 'android' @() }
        'macos' { Skip 'macos (native only — runs on a Mac; no container can be macOS)' }
        'windows' {
            if ($dockerOs -ne 'windows') {
                Skip 'windows (native only — switch Docker to Windows-container mode; windows-cross covers it meanwhile)'
            } else {
                docker build -t "$crate-test-windows" -f docker/windows.Dockerfile docker
                if ($LASTEXITCODE -eq 0) {
                    docker run --rm -v "${source}:C:\src:ro" "$crate-test-windows"
                }
                if ($LASTEXITCODE -eq 0) { Pass 'windows' } else { Fail 'windows' }
            }
        }
        default { Fail "$os (unknown — $($allOses -join ' '))" }
    }
}

Write-Host "`n$crate across operating systems, from Windows:"
$summary | ForEach-Object { Write-Host $_ }

if ($failed) { exit 1 }
