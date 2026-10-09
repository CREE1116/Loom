# Build Loom from this checkout and install loom.exe.
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 [-Prefix DIR] [-Cuda]
# Requires Rust (https://rustup.rs), git, CMake and Visual Studio Build Tools (C++).
# User data (models, providers, settings) lives in %USERPROFILE%\.loom and is never touched.
param(
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\Loom'),
    [switch]$Cuda
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
foreach ($tool in 'cargo', 'cmake') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Error "$tool was not found. Install Rust from https://rustup.rs and CMake from https://cmake.org, then reopen the terminal."
        exit 1
    }
}
$cargoArgs = @('build', '--release', '--locked', '--manifest-path', (Join-Path $root 'loom-tui\Cargo.toml'), '--bin', 'loom')
if ($Cuda) { $cargoArgs += @('--features', 'cuda') }
Write-Host 'Building Loom (release)...'
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$bin = Join-Path $Prefix 'bin'
New-Item -ItemType Directory -Force -Path $bin | Out-Null
Copy-Item -Force (Join-Path $root 'loom-tui\target\release\loom.exe') (Join-Path $bin 'loom.exe')
& (Join-Path $bin 'loom.exe') --version | Out-Null
Write-Host "Installed $bin\loom.exe"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not ($userPath -split ';' | Where-Object { $_ -eq $bin })) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$bin", 'User')
    Write-Host "Added $bin to your user PATH; open a new terminal to use 'loom'."
}
Write-Host 'Start in a project:  loom --cwd C:\path\to\project   (try without a model: loom --demo)'
