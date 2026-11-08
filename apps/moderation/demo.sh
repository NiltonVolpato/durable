#!/usr/bin/env bash
# One process per step. Run from the repo root: apps/moderation/demo.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
RUSTC_WRAPPER= cargo build -q -p durable-driver && "$TD/debug/cargo-durable" durable build -q -p moderation || exit 1
BIN="$TD/debug/moderation"
W="$(mktemp -d)"; cd "$W"
export SPOT_CHECK_PERCENT=100   # acme spot-checks every clean item in this demo (10% by default)
m() { echo "\$ moderation $*"; "$BIN" "$@"; echo; }

m submit a1 acme holiday photos            # will be clean -> spot check -> manual review
m submit a2 acme free virus download       # malware -> removed
m submit a3 acme article about the ceo     # clean -> spot check -> review -> legal
m submit g1 globex quarterly report macro  # suspicious -> review with a "2 day" SLA
m submit g2 globex crash test file         # sandbox crashes on the first run -> resubmitted
m run 2.5                                  # scanner service: poll sandboxes, retry, report verdicts
m pending
m deliver a1 1 '{"flags":[],"note":"fine"}'
m deliver a3 1 '{"flags":["Defamation"],"note":"names a private person"}'
m deliver a3 2 '{"takedown":true}'
echo "(g1's review SLA runs out)"; sleep 4
m run 0.3                                  # the SLA timer resumes g1
m status g1
