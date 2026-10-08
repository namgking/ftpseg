#!/bin/sh
# Runs cargo with the workspace toolchain when one is present.
#
#   ./scripts/cargo.sh build --release
#   ./scripts/cargo.sh test
set -e

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/env.sh"

exec cargo "$@"
