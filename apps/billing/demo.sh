#!/usr/bin/env bash
# One process per step. Run from the repo root: apps/billing/demo.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
RUSTC_WRAPPER= cargo build -q -p durable-driver && "$TD/debug/cargo-durable" durable build -q -p billing || exit 1
BIN="$TD/debug/billing"
W="$(mktemp -d)"; cd "$W"
b() { echo "\$ ${CRASH_AFTER:+CRASH_AFTER=$CRASH_AFTER }billing $*"; "$BIN" "$@"; echo; }

b start alice 3
CRASH_AFTER=stripe_charge b work     # charged, then crash before the charge is stored
CRASH_AFTER=send_invoice b work      # the charge is retried with the same key: no second charge
b work                               # the invoice is retried with the same key: not sent twice
b status alice
b ledger
