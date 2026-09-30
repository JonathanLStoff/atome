# atome's test matrix on native Windows — windows.Dockerfile's entry point, and
# runnable by hand on any Windows machine with Rust and ffmpeg:
#
#   powershell -File docker\test.ps1
#
# The same steps as test.sh's Linux profile. Works on a copy of the source, so
# the fixtures it makes never reach the working tree.

$ErrorActionPreference = 'Continue'

$source = if (Test-Path C:\src) { 'C:\src' } else { Split-Path $PSScriptRoot }
$work = Join-Path $env:TEMP "atome-test-$PID"
robocopy $source $work /E /XD target .git /NFL /NDL /NJH /NJS | Out-Null
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $source 'target' }
Set-Location $work

$results = @()
$failed = $false

function Step($name, [scriptblock]$command) {
    Write-Host "==> $name"
    & $command
    if ($LASTEXITCODE -eq 0) {
        $script:results += "PASS  $name"
    } else {
        $script:results += "FAIL  $name"
        $script:failed = $true
    }
}

Step 'converters' { powershell -NoProfile -File tests\test_data\make_fixtures.ps1 }
Step 'default' { cargo test }
Step 'import-export' { cargo test --features import,export }
Step 'import-all' { cargo test --features import-all,export }
Step 'plugins' { cargo check --features vst,vst3 --all-targets }

Write-Host ''
Write-Host 'atome on windows:'
$results | ForEach-Object { Write-Host "  $_" }

Set-Location $source
Remove-Item -Recurse -Force $work

if ($failed) { exit 1 }
