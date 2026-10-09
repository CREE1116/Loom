# Native Windows launcher; all Loom arguments are forwarded unchanged.
$ErrorActionPreference = 'Stop'
$loomRoot = Split-Path -Parent $PSScriptRoot
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error 'Cargo was not found. Install Rust from https://rustup.rs, then reopen the terminal.'
    exit 1
}
$loomNativeCommand = $args.Count -gt 0 -and $args[0] -in @('run','resume','approve','status')
$loomManifest = if ($loomNativeCommand) { 'loom-core/Cargo.toml' } else { 'loom-tui/Cargo.toml' }
$loomCargoArgs = @('run', '--quiet', '--manifest-path', (Join-Path $loomRoot $loomManifest))
if ($loomNativeCommand) { $loomCargoArgs += @('--bin','loom') }
elseif ($env:LOOM_PROFILE -ne 'debug' -and $env:CUSTOM_TUI_PROFILE -ne 'debug') { $loomCargoArgs += '--release' }
$loomCargoArgs += '--'
$loomCargoArgs += $args
& cargo @loomCargoArgs
exit $LASTEXITCODE
