# Builds the SoundCraft desktop app on Windows with ASIO audio devices enabled.
#
# ASIO is an opt-in cargo feature (`asio`) because building it downloads Steinberg's ASIO SDK,
# which has its own licence, and needs LLVM's libclang for the bindings. This script checks for
# libclang, points the build at it and builds the release app with the feature on.
#
# Usage (PowerShell, from the repository root):
#   .\scripts\build-windows-asio.ps1            # release build
#   .\scripts\build-windows-asio.ps1 -Run       # build, then start the app
#
# Needs: Rust (rustup), the Visual Studio C++ build tools and LLVM (`winget install LLVM.LLVM`).
param([switch]$Run)
# No `$ErrorActionPreference = 'Stop'`: Windows PowerShell 5.1 turns a native command's stderr into
# error records when output is redirected, and cargo writes its progress there, so that setting
# would abort the build at its first line. Failures are checked through $LASTEXITCODE instead.

$candidates = @($env:LIBCLANG_PATH, 'C:\Program Files\LLVM\bin') |
    Where-Object { $_ -and (Test-Path (Join-Path $_ 'libclang.dll')) }
if (-not $candidates) {
    Write-Host "libclang.dll not found. Install LLVM (winget install LLVM.LLVM) or set LIBCLANG_PATH to its bin folder."
    exit 1
}
$env:LIBCLANG_PATH = @($candidates)[0]
Write-Host "Using libclang from $env:LIBCLANG_PATH"

cargo build --release -p soundcraft --features asio
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "Built target\release\soundcraft.exe with ASIO. Choose it under Setup > Playback Engine > Audio system."
if ($Run) { & .\target\release\soundcraft.exe }
