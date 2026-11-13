#!/usr/bin/env bash
# Tests for check-ci-lane-labels.py: the real ci.yml passes, and every kind of
# drift the check exists for fails it. A check that has never been seen to
# fail proves nothing, so each mutation below must turn it red.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$here/check-ci-lane-labels.py"
ci="$here/../workflows/ci.yml"
real_decl="$here/../../.qontinui/ci-pool-variables.toml"
# The declaration each case checks against; a declaration case overrides it.
decl="$real_decl"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail=0

pass_case() {
  if python3 -I -B "$check" "$1" "$decl" > "$work/out" 2>&1; then
    echo "ok   - $2"
  else
    echo "FAIL - $2 (expected pass)"; cat "$work/out"; fail=1
  fi
}

fail_case() {
  local rc=0
  python3 -I -B "$check" "$1" "$decl" > "$work/out" 2>&1 || rc=$?
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

# The lane-variable declaration (.qontinui/ci-pool-variables.toml) coord reads.
pass_case "$ci" "the real declaration matches ci.yml"
# With no second argument the declaration is found beside the workflow's .github.
if python3 -I -B "$check" "$ci" > "$work/out" 2>&1; then echo "ok   - the default declaration path resolves"
else echo "FAIL - the default declaration path did not resolve"; cat "$work/out"; fail=1; fi
# Like mutate, but refuses a substitution that changed nothing: a pass case
# over an unmutated declaration would pass vacuously.
mutate_decl() {
  sed "0,/$2/s//$3/" "$real_decl" > "$work/$1.toml"
  if cmp -s "$real_decl" "$work/$1.toml"; then
    echo "FAIL - declaration mutation '$1' did not apply" >&2; fail=1
  fi
  echo "$work/$1.toml"
}
# Command substitution runs mutate_decl in a subshell, so its `fail=1` is
# lost there; it says FAIL on stderr and this check catches the no-op instead.
decl_applied() { cmp -s "$real_decl" "$decl" && { echo "FAIL - $1 mutation did not apply"; fail=1; }; true; }
decl="$(mutate_decl declpool '"ram-16g"\]' '"ram-8g"]')"
decl_applied declpool
fail_case "$ci" "the declared pool drifts from the forced literal" "declared pool"
decl="$(mutate_decl declorder 'pool = .*' 'pool = ["RAM-16G", "x64", "linux", "self-hosted"]')"
decl_applied declorder
pass_case "$ci" "the declared pool in another order and case is the same pool"
decl="$(mutate_decl declname 'SUPERVISOR_CI_LINUX_LANE' 'SUPERVISOR_CI_OTHER_LANE')"
decl_applied declname
fail_case "$ci" "a routed job reads an undeclared lane variable" "is not declared in"
decl="$work/declstale.toml"
{ cat "$real_decl"; printf '\n[[variables]]\nname = "SUPERVISOR_CI_STALE_LANE"\npool = ["self-hosted", "Linux"]\n'; } > "$decl"
fail_case "$ci" "a declared variable no routed job reads" "no routed runs-on reads it"
decl="$work/declbad.toml"
{ cat "$real_decl"; printf '\n[[variables]\n'; } > "$decl"
fail_case "$ci" "a declaration that does not parse" "does not parse"
decl="$work/declkey.toml"
sed 's/^name = .*/&\nnote = "x"/' "$real_decl" > "$decl"; decl_applied declkey
fail_case "$ci" "a declaration entry with an unknown key" "unknown key(s)"
decl="$work/decltop.toml"
{ printf 'comment = "x"\n'; cat "$real_decl"; } > "$decl"
fail_case "$ci" "a declaration with an unknown top-level key" "unknown top-level key(s)"
decl="$work/decldup.toml"
{ cat "$real_decl"; printf '\n[[variables]]\nname = "supervisor_ci_linux_lane"\npool = ["self-hosted", "Linux", "X64", "ram-16g"]\n'; } > "$decl"
fail_case "$ci" "a variable declared twice" "declared more than once"
decl="$work/decltable.toml"
sed 's/^\[\[variables\]\]$/[variables]/' "$real_decl" > "$decl"; decl_applied decltable
fail_case "$ci" "variables written as a single table" "array of tables"
decl="$work/absent.toml"
rc=0; python3 -I -B "$check" "$ci" "$decl" > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 2 ]; then echo "ok   - a missing declaration exits 2"; else echo "FAIL - missing declaration exited $rc, want 2"; fail=1; fi

exit "$fail"
