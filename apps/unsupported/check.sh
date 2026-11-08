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
# Glue limit
check lint_errors "is private to its module"
# Borrow
check lint_errors "\`r\` is live across the await: \`&str\` holds a reference"
# Opaque
check lint_errors "tokio::future::maybe_done::MaybeDone"
check lint_errors "\`tokio::time::Timeout<durable::Request<u64>>\` does not implement Serialize"
check lint_errors "\`later\` is live across the await"
# Effect
check lint_errors "\`lease\` has a destructor that would run when the task parks"
check lint_errors "\`job\` is live across the await; \`Lease\` implements \`Drop\`"
check lint_errors "\`f.c0\` has a destructor"
check lint_errors "\`later.upvar0\` has a destructor"
check lint_errors "\`ls\` is live across the await; \`Lease\` implements \`Drop\`"
# Identity
check lint_errors "\`std::sync::Arc<std::sync::Mutex<u64>>\` is shared and mutable through \`std::sync::Mutex<u64>\`"
check lint_errors "\`Shared\` holds \`std::rc::Rc<std::cell::RefCell<std::vec::Vec<std::string::String>>>\`, which is shared and mutable"
check lint_errors "is shared and mutable through \`std::cell::Cell<u64>\`"
check lint_errors "\`std::sync::Weak<u64>\` is a \`Weak\`; a stored one comes back empty"
# Handle
check lint_errors "\`SlotId\` is a handle valid only in the process that created it"
check lint_errors "\`std::vec::Vec<ConnId>\` holds \`ConnId\`, which is a handle valid only"
check lint_errors "\`slotmap::DefaultKey\` holds \`slotmap::KeyData\`, which is a handle valid only"
echo "full output: $TD/durable/{macro_errors,lint_errors}.log"
exit $fail
