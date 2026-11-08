#!/usr/bin/env bash
# Drive the probe workflow through all its slow points, one process per step.
#   usage: probe/run.sh <req_id>   (odd: note kept; even: note moved before the first park)
set -uo pipefail
cd "$(dirname "$0")/.."
./build-glue.sh probe probe probe/src/main.rs >/dev/null || exit 1
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
BIN="$TD/release/probe"; ID=${1:-7}
W="$(mktemp -d)"; mkdir -p "$W/results"; cd "$W"
show() { python3 -c "
import json; f=json.load(open('probe.json'))['frame']
print('  state', f['state'], 'at', f['at']); print('  ', {k: v for k, v in f.items() if k not in ('state','sig','at','__awaitee')})
print('   awaitee:', json.dumps(f.get('__awaitee'))[:300])"; }
step() { echo "== $1"; shift; "$BIN" "$@"; if [ -f probe.json ]; then show; fi; }

step "start: fast path on tokio, then the first slow call" start "$ID"
echo 40 > results/$ID
step "review done: now two reviews in parallel" resume
echo 1 > results/$((ID+1))
step "only one of the two done: still parked in the join" resume
echo 2 > results/$((ID+2))
step "both done: now a review raced against a 2 s deadline" resume
sleep 3
step "deadline passed (no review result): retry with a closure" resume
echo 0 > results/$((ID+4))
step "first check empty: retry attempt 2" resume
echo 5 > results/$((ID+5))
step "second check ok: done" resume
