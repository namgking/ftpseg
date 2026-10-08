# Installs everything needed to build ftpseg on Windows without Visual Studio.
#
# Everything lands inside the workspace, so nothing on the system is touched:
#
#   .rust\rustup  .rust\cargo   the Rust toolchain (x86_64-pc-windows-gnu host)
#   .rust\mingw                 a portable MinGW-w64 (for the `as` assembler)
#
# After this finishes, build with:  .\scripts\cargo.ps1 build --release
$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
$RustupHome = Join-Path $Root '.rust\rustup'
$CargoHome = Join-Path $Root '.rust\cargo'
$CargoExe = Join-Path $CargoHome 'bin\cargo.exe'

if (Test-Path $CargoExe) {
    Write-Host 'Rust toolchain already present.'
} else {
    Write-Host 'Downloading rustup-init ...'
    New-Item -ItemType Directory -Force -Path $RustupHome, $CargoHome | Out-Null
    $init = Join-Path $Root '.rust\rustup-init.exe'
    Invoke-WebRequest `
        -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe' `
        -OutFile $init

    $env:RUSTUP_HOME = $RustupHome
    $env:CARGO_HOME = $CargoHome
    Write-Host 'Installing the toolchain (this takes a few minutes) ...'
    & $init -y --no-modify-path --default-host x86_64-pc-windows-gnu --profile minimal
    if ($LASTEXITCODE -ne 0) { throw "rustup-init failed with exit code $LASTEXITCODE" }
}

# The Rust windows-gnu kit ships gcc, ld and dlltool but no assembler. dlltool
# needs `as` to build the import libraries that raw-dylib links (tokio, clap,
# native-tls) require, so a real MinGW-w64 has to be present.
$mingwAs = Join-Path $Root '.rust\mingw\w64devkit\bin\as.exe'
if (Test-Path $mingwAs) {
    Write-Host 'MinGW-w64 already present.'
} else {
    Write-Host 'Fetching a portable MinGW-w64 (about 64 MiB) ...'
    $python = (Get-Command python -ErrorAction SilentlyContinue) ??
              (Get-Command python3 -ErrorAction SilentlyContinue)
    if (-not $python) {
        throw 'Python 3 is required by scripts/fetch-mingw.py to unpack MinGW-w64.'
    }
    & $python.Source (Join-Path $PSScriptRoot 'fetch-mingw.py')
    if ($LASTEXITCODE -ne 0) { throw "fetch-mingw.py failed with exit code $LASTEXITCODE" }
}

Write-Host ''
Write-Host 'Toolchain ready. Build with:'
Write-Host '  .\scripts\cargo.ps1 build --release'
