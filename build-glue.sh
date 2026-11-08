#!/usr/bin/env bash
# Two-pass release build of a binary with serde glue for its coroutines.
#   usage: build-glue.sh [package crate_name main_file]   (default: the poc binary)
#   pass 1: empty glue table; durable-driver writes glue from the layouts.
#   pass 2: glue included; durable-driver regenerates it and fails on mismatch.
set -euo pipefail
cd "$(dirname "$0")"
export RUSTC_WRAPPER=
cargo build -q -p durable-driver
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
mkdir -p "$TD/durable"
PKG=${1:-durable-poc} CRATE=${2:-durable_poc} MAIN=${3:-src/main.rs}
G="$TD/durable/glue-$CRATE.rs"; [ "$CRATE" = durable_poc ] && G="$TD/durable/glue.rs"
L="$TD/durable/layout-$CRATE.txt"; [ "$CRATE" = durable_poc ] && L="$TD/durable/layout.txt"
export RUSTC_WORKSPACE_WRAPPER="$TD/debug/durable-driver" DURABLE_CRATES=$CRATE
touch "$MAIN" # make sure the driver sees the bin in pass 1
DURABLE_GLUE_OUT="$G" DURABLE_DUMP="$L" cargo build -q --release -p "$PKG" --bin "$PKG"
DURABLE_GLUE_SRC="$G" DURABLE_GLUE_CHECK="$G" cargo build -q --release -p "$PKG" --bin "$PKG"
echo "glue: $G"
