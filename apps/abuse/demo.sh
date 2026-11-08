#!/usr/bin/env bash
# One process per step. Run from the repo root: apps/abuse/demo.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
RUSTC_WRAPPER= cargo build -q -p durable-driver && "$TD/debug/cargo-durable" durable build -q -p abuse || exit 1
BIN="$TD/debug/abuse"
W="$(mktemp -d)"; cd "$W"
run() { echo "\$ abuse $*"; "$BIN" "$@"; echo; }

run submit r1 hello there             # fast path: nothing stored
run submit r2 buy cheap scam pills    # fast path
run submit r3 is this ok              # unclear: parks, two review requests sent
run submit r4 hmm, not sure           # unclear: parks
run pending
run deliver r3 0 true                 # reviewer-a on r3
run deliver r3 0 true                 # duplicate: ignored
run deliver r3 1 false                # reviewer-b disagrees: supervisor asked
run deliver r3 2 true                 # supervisor: done
echo "(waiting for r4's review deadline)"; sleep 3.2
run timers                            # r4: deadline passed, supervisor asked
run deliver r4 2 false
run status r1                         # never stored
