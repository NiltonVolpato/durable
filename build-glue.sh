#!/usr/bin/env bash
# Two-pass release build of the poc binary with serde glue for its coroutines.
#   pass 1: empty glue table; durable-driver writes glue from the layouts.
#   pass 2: glue included; durable-driver regenerates it and fails on mismatch.
set -euo pipefail
cd "$(dirname "$0")"
export RUSTC_WRAPPER=
cargo build -q -p durable-driver
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
mkdir -p "$TD/durable"
export RUSTC_WORKSPACE_WRAPPER="$TD/debug/durable-driver" DURABLE_CRATES=durable_poc
touch src/main.rs # make sure the driver sees the bin in pass 1
DURABLE_GLUE_OUT="$TD/durable/glue.rs" DURABLE_DUMP="$TD/durable/layout.txt" \
    cargo build -q --release --bin durable-poc
DURABLE_GLUE_SRC="$TD/durable/glue.rs" DURABLE_GLUE_CHECK="$TD/durable/glue.rs" \
    cargo build -q --release --bin durable-poc
echo "glue: $TD/durable/glue.rs"
