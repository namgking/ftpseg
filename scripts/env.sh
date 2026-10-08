# Puts the workspace-local Rust toolchain on PATH when one is present.
#
# On macOS and Linux a normal rustup installation is the expected setup, so this
# script only takes effect if .rust/cargo exists (as it does on the Windows
# development machine, where the toolchain is kept inside the workspace).
#
# Usage:  . ./scripts/env.sh
#         ./scripts/cargo.sh test

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

if [ -d "$ROOT/.rust/cargo/bin" ]; then
    RUSTUP_HOME="$ROOT/.rust/rustup"
    CARGO_HOME="$ROOT/.rust/cargo"
    CARGO_TARGET_DIR="$ROOT/target"
    TMPDIR="$ROOT/.tmp"
    export RUSTUP_HOME CARGO_HOME CARGO_TARGET_DIR TMPDIR
    mkdir -p "$TMPDIR"
    prefix="$CARGO_HOME/bin"

    # A complete MinGW-w64 (gcc, ld, as, dlltool) is what rustc links with; the
    # toolchain's own incomplete kit is deliberately left off the path.
    for mingw_bin in "$ROOT"/.rust/mingw/w64devkit/bin "$ROOT"/.rust/mingw/bin; do
        [ -x "$mingw_bin/as.exe" ] && prefix="$prefix:$mingw_bin"
    done

    PATH="$prefix:$PATH"
    export PATH
fi
