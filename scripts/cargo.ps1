# Runs cargo with the workspace-local toolchain.
#
#   .\scripts\cargo.ps1 build --release
#   .\scripts\cargo.ps1 test
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

& cargo @CargoArgs
exit $LASTEXITCODE
