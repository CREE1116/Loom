# Install a verified Windows x86_64 release without Rust or a compiler.
# Save this file, then run: powershell -File .\get.ps1 [-Version v0.1.3] [-Prefix DIR]
param(
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\Loom'),
    [string]$Version = 'latest'
)
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    throw 'The prebuilt Windows release requires x86_64. Build other targets from source.'
}
$repo = 'CREE1116/Loom'
if ($Version -eq 'latest') {
    $Version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Invalid release version' }
$name = 'loom-windows-x86_64'
$asset = "$name.tar.gz"
$work = Join-Path ([IO.Path]::GetTempPath()) ('loom-install-' + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $url = "https://github.com/$repo/releases/download/$Version/$asset"
    $archive = Join-Path $work $asset
    Invoke-WebRequest -UseBasicParsing $url -OutFile $archive
    Invoke-WebRequest -UseBasicParsing "$url.sha256" -OutFile (Join-Path $work 'checksum')
    $fields = (Get-Content -Raw (Join-Path $work 'checksum')).Trim() -split '\s+'
    if ($fields.Count -ne 2 -or $fields[1] -ne $asset -or $fields[0] -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'Invalid checksum file'
    }
    if ((Get-FileHash -Algorithm SHA256 $archive).Hash -ne $fields[0]) { throw 'Checksum mismatch' }
    & tar -xzf $archive -C $work
    if ($LASTEXITCODE -ne 0) { throw 'Could not extract package' }
    $incoming = Join-Path $work "$name\bin\loom.exe"
    $reported = & $incoming --version
    if ($LASTEXITCODE -ne 0 -or $reported.Trim() -ne "loom-cli $($Version.Substring(1))") {
        throw 'Downloaded executable failed version verification'
    }
    $bin = Join-Path $Prefix 'bin'
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    $destination = Join-Path $bin 'loom.exe'
    $staged = Join-Path $bin ('.loom-' + [Guid]::NewGuid() + '.exe')
    Copy-Item -LiteralPath $incoming -Destination $staged
    try {
        if (Test-Path -LiteralPath $destination) {
            [IO.File]::Replace($staged, $destination, $null)
        } else {
            [IO.File]::Move($staged, $destination)
        }
    } finally { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $bin })) {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$bin", 'User')
    }
    Write-Host "Installed Loom $Version at $destination. Open a new terminal to run loom."
} finally { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
