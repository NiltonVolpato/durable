#!/usr/bin/env bash
# Park `evolve` with one build, resume it with a changed build.
set -uo pipefail
cd "$(dirname "$0")/.."
TD="$(cargo metadata -q --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
BIN="$TD/release/durable-poc"
W="$(mktemp -d)"; mkdir -p "$W/results"
build() { EVOLVE=$1 ./build-glue.sh >/dev/null || exit 1; echo "-- built v$1; layout of evolve:"; \
          awk '/^coroutine .*evolve/{p=1;print;next} /^coroutine/{p=0} p' "$TD/durable/layout.txt"; }
run() { (cd "$W" && "$BIN" "$@"); }

build 1
echo "== v1: park at the first slow call"; run sstart evolve 7
python3 -c "import json;f=json.load(open('$W/snap.json'))['task']['frame'];print('  sig:', f['sig'])"

build 2
echo 40 > "$W/results/7"
echo "== v2: resume the v1 snapshot (expect a new park at event 1007)"; run sresume
echo 2 > "$W/results/1007"
echo "== v2: finish (expect 1+5+40+2+7=55)"; run sresume

rm -f "$W/results/"*
build 1
echo "== v1: park again"; run sstart evolve 7
build 3
echo "== v3: resume the v1 snapshot (expect refusal)"; run sresume; echo "exit=$?"

build 1 >/dev/null # leave the default build in place
