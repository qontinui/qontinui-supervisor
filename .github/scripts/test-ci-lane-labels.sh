#!/usr/bin/env bash
# Tests for check-ci-lane-labels.py: the real ci.yml passes, and every kind of
# drift the check exists for fails it. A check that has never been seen to
# fail proves nothing, so each mutation below must turn it red.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$here/check-ci-lane-labels.py"
ci="$here/../workflows/ci.yml"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail=0

pass_case() {
  if python3 -I -B "$check" "$1" > "$work/out" 2>&1; then
    echo "ok   - $2"
  else
    echo "FAIL - $2 (expected pass)"; cat "$work/out"; fail=1
  fi
}

fail_case() {
  local rc=0
  python3 -I -B "$check" "$1" > "$work/out" 2>&1 || rc=$?
  if [ "$rc" -eq 1 ] && grep -q "$3" "$work/out"; then
    echo "ok   - $2"
  else
    echo "FAIL - $2 (expected exit 1 naming '$3', got exit $rc)"; cat "$work/out"; fail=1
  fi
}

# Mutate the FIRST occurrence of a pattern only, so one place drifts. The
# replacement is a sed replacement: never put a bare & in it (it means the match).
mutate() { sed "0,/$2/s//$3/" "$ci" > "$work/$1.yml"; echo "$work/$1.yml"; }

pass_case "$ci" "the real ci.yml is consistent"
fail_case "$(mutate route 'self-hosted,Linux,X64,ram-16g' 'self-hosted,Linux,X64,ram-32g')" \
  "route's --self-hosted-labels drifts" "!= route's \\[.*ram-32g"
fail_case "$(mutate pools '# coord-pools: \[self-hosted, Linux, X64, ram-16g\]' '# coord-pools: [self-hosted, Linux, ram-16g]')" \
  "one coord-pools line drifts" "coord-pools pool"
fail_case "$(mutate forced '"X64","ram-16g"' '"X64","ram-32g"')" \
  "one runs-on forced literal drifts" "forced-lane literal"
fail_case "$(mutate floor "|| '\[\"ubuntu-latest\"\]'" "|| '[\"ubuntu-24.04\"]'")" \
  "one runs-on floor drifts" "floor literal"
fail_case "$(mutate unrouted 'needs.route.outputs.labels) ||' 'needs.other.outputs.labels) ||')" \
  "a runs-on stops reading route's output" "does not read needs.route.outputs.labels"
# A job that hardcodes the pool outside the router.
cp "$ci" "$work/hardcoded.yml"
printf '  sneaky:\n    runs-on: [self-hosted, Linux, X64, ram-16g]\n    steps:\n      - run: true\n' >> "$work/hardcoded.yml"
fail_case "$work/hardcoded.yml" "an unrouted job hardcodes the pool" "does not .needs: route."
# A block-list needs is still read as routed.
python3 -I - "$ci" "$work/block.yml" <<'PY'
import sys
s = open(sys.argv[1]).read().replace("    needs: route\n", "    needs:\n      - route\n", 1)
open(sys.argv[2], "w").write(s)
PY
grep -q '^      - route$' "$work/block.yml" || { echo "FAIL - block-list mutation did not apply"; fail=1; }
pass_case "$work/block.yml" "a block-list needs: - route counts as routed"
rc=0; python3 -I -B "$check" "$work/absent.yml" > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 2 ]; then echo "ok   - an unreadable file exits 2"; else echo "FAIL - unreadable file exited $rc, want 2"; fail=1; fi

exit "$fail"
