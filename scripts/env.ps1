# Puts the workspace-local Rust toolchain on PATH for this shell.
#
# The toolchain lives in .rust/ rather than the user profile so that the build
# is self-contained and reproducible; dot-source this file before calling cargo.
#
# Three environment problems are solved here, all of them Windows specific:
#
#  1. TMP/TEMP/TMPDIR are redirected into the workspace. The MinGW linker needs
#     a writable scratch directory to produce proc-macro DLLs, and the machine
#     wide %LOCALAPPDATA%\Temp is not writable for this account.
#  2. Rust's windows-gnu kit has no assembler, so its dlltool cannot build the
#     import libraries that raw-dylib links (tokio, clap, native-tls) need.
#     scripts/fetch-mingw.py drops a complete MinGW-w64 in .rust/mingw, which
#     supplies gcc, ld, as and dlltool as one consistent set. Putting it first
#     means rustup's incomplete kit is not used for linking at all.
#  3. That MinGW-w64 has no libgcc_eh.a, which rustc always asks for, so
#     fetch-mingw.py leaves an empty one behind. See the script for details.
$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot

$env:RUSTUP_HOME = Join-Path $Root '.rust\rustup'
$env:CARGO_HOME = Join-Path $Root '.rust\cargo'
$env:CARGO_TARGET_DIR = Join-Path $Root 'target'

$Temp = Join-Path $Root '.tmp'
New-Item -ItemType Directory -Force -Path $Temp | Out-Null
$env:TMP = $Temp
$env:TEMP = $Temp
$env:TMPDIR = $Temp

$searchPath = @((Join-Path $env:CARGO_HOME 'bin'))

$mingwRoot = Join-Path $Root '.rust\mingw'
if (Test-Path $mingwRoot) {
    $mingwBin = @(
        (Join-Path $mingwRoot 'w64devkit\bin'),
        (Join-Path $mingwRoot 'bin')
    ) | Where-Object { Test-Path (Join-Path $_ 'as.exe') } | Select-Object -First 1

    if (-not $mingwBin) {
        $mingwBin = Get-ChildItem -Path $mingwRoot -Recurse -Filter 'as.exe' -ErrorAction SilentlyContinue |
            Select-Object -First 1 |
            ForEach-Object { $_.DirectoryName }
    }
    if ($mingwBin) {
        $searchPath += $mingwBin
    }
}

$env:PATH = ($searchPath -join ';') + ';' + $env:PATH
