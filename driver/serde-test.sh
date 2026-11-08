#!/usr/bin/env bash
# Run tests/serde_roundtrip.rs natively and under Miri, each with glue
# generated from coroutine layouts compiled with the matching flags.
set -euo pipefail
cd "$(dirname "$0")/.."
export RUSTC_WRAPPER=
cargo build -q -p durable-driver
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
mkdir -p "$TD/durable"
DRV="$TD/debug/durable-driver"
T=(--test serde_roundtrip)

# Two-pass glue for the test crate, built with extra RUSTFLAGS ($1).
gen() {
    local flags=$1 out=$2
    touch tests/serde_roundtrip.rs
    RUSTFLAGS="$flags" RUSTC_WORKSPACE_WRAPPER="$DRV" DURABLE_CRATES=serde_roundtrip \
        DURABLE_GLUE_OUT="$out" cargo test -q --no-run "${T[@]}" 2>/dev/null
    RUSTFLAGS="$flags" RUSTC_WORKSPACE_WRAPPER="$DRV" DURABLE_CRATES=serde_roundtrip \
        DURABLE_TEST_GLUE_SRC="$out" DURABLE_GLUE_CHECK="$out" cargo test -q --no-run "${T[@]}" 2>/dev/null
}

echo "== native"
gen "" "$TD/durable/test_glue_native.rs"
DURABLE_TEST_GLUE_SRC="$TD/durable/test_glue_native.rs" cargo test -q "${T[@]}" -- --nocapture 2>&1 | grep -E "glue entries|test result|panicked|skipped"

echo "== miri"
# Miri's default rustc flags (MIRI_DEFAULT_ARGS) plus cargo-miri's --cfg miri.
MIRI_RUSTC="--cfg miri -Zalways-encode-mir -Zextra-const-ub-checks -Zmir-preserve-ub -Zmir-opt-level=0 -Zmir-enable-passes=-CheckAlignment,-CheckNull,-CheckEnums"
gen "$MIRI_RUSTC" "$TD/durable/test_glue_miri.rs"
diff -q "$TD/durable/test_glue_native.rs" "$TD/durable/test_glue_miri.rs" >/dev/null \
    && echo "(glue identical to native)" || echo "(glue differs from native: layouts depend on the flags)"
DURABLE_TEST_GLUE_SRC="$TD/durable/test_glue_miri.rs" MIRIFLAGS="-Zmiri-disable-isolation" \
    cargo miri test "${T[@]}" -- --nocapture 2>&1 | grep -E "glue entries|test result|panicked|skipped|Undefined|error|leak" || true
