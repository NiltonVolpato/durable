#!/usr/bin/env bash
# Build poc/examples/lint_fail.rs with durable-driver; the lint must reject it.
set -uo pipefail
cd "$(dirname "$0")/.."
export RUSTC_WRAPPER=
cargo build -q -p durable-driver || exit 1
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
touch poc/examples/lint_fail.rs
RUSTC_WORKSPACE_WRAPPER="$TD/debug/durable-driver" DURABLE_CRATES=lint_fail \
    cargo build -q -p durable-poc --example lint_fail 2>&1 | grep -v "^warning\|never used\|^  *|$\|= note: .#\[warn"
