# Native Windows launcher; all Loom arguments are forwarded unchanged.
$ErrorActionPreference = 'Stop'
$loomRoot = Split-Path -Parent $PSScriptRoot
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error 'Cargo was not found. Install Rust from https://rustup.rs, then reopen the terminal.'
    exit 1
}
$loomCargoArgs = @('run', '--quiet', '--manifest-path', (Join-Path $loomRoot 'custom-tui/Cargo.toml'))
if ($env:CUSTOM_TUI_PROFILE -ne 'debug') { $loomCargoArgs += '--release' }
$loomCargoArgs += '--'
$loomCargoArgs += $args
& cargo @loomCargoArgs
exit $LASTEXITCODE
