#!/usr/bin/env bash
# End-to-end v0 demo: each `poc` call is a separate process.
set -u
cargo build -q --release || exit 1
BIN="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')/release/durable-poc"
W="$(mktemp -d)"; mkdir -p "$W/results"; cd "$W" || exit 1

echo "== classify: fast path";            "$BIN" start classify 8
echo "== classify: slow path";            "$BIN" start classify 7
echo 40 > results/7;    echo "== resume"; "$BIN" resume
echo 2 > results/1007;  echo "== resume"; "$BIN" resume   # expect 49

rm -f snap.bin results/*
echo "== leaky (borrow forced past check): expect crash/garbage"
"$BIN" start leaky 7; echo 40 > results/7; "$BIN" resume; echo "exit=$?"

rm -f snap.bin results/*
echo "== racy (join slow+ephemeral): run-level check refuses eviction"
"$BIN" start racy 7 2>&1 | grep -E "frame|refusing"

rm -f snap.bin results/*
echo "== racy without run-level check: expect crash/garbage"
POC_SKIP_EPHEMERAL_CHECK=1 "$BIN" start racy 7; echo 40 > results/7; "$BIN" resume; echo "exit=$?"
