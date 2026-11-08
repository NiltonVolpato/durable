#!/usr/bin/env bash
# Build each example with cargo durable and check that every case gets its error.
set -uo pipefail
cd "$(dirname "$0")/../.."
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
RUSTC_WRAPPER= cargo build -q -p durable-driver || exit 1
fail=0
check() { # example, expected message
    if grep -qF -- "$2" "$TD/durable/$1.log"; then echo "  ok    $2"; else echo "  MISSING $2"; fail=1; fi
}
for ex in macro_errors lint_errors; do
    echo "== $ex"
    "$TD/debug/cargo-durable" durable build -q -p unsupported --features check --example "$ex" >"$TD/durable/$ex.log" 2>&1 && { echo "  built?!"; fail=1; }
done
check macro_errors "durable workflows cannot be generic"
check macro_errors "methods cannot be durable workflows"
check macro_errors "goes on an \`async fn\`"
[ "$(grep -c 'durable workflows cannot be generic' "$TD/durable/macro_errors.log")" -ge 2 ] && echo "  ok    both generic forms" || { echo "  MISSING impl-Trait form"; fail=1; }
check lint_errors "is private to its module"
check lint_errors "\`r\` is live across the await: \`&str\` holds a reference"
check lint_errors "tokio::future::maybe_done::MaybeDone"
check lint_errors "tokio::time::Timeout"
check lint_errors "\`later\` is live across the await"
check lint_errors "std::sync::Arc<u64>\` does not implement Serialize"
echo "full output: $TD/durable/{macro_errors,lint_errors}.log"
exit $fail
