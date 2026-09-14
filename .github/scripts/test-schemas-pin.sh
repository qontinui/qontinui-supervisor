#!/usr/bin/env bash
# Tests for `.github/scripts/schemas-pin.sh` — no network.
#
# The pin exists so a qontinui-schemas release cannot red coord main (plan
# 2026-08-31-schemas-releases-strand-consumer-cargo-locks §7). Its one failure
# mode that would silently undo that is DEGRADING TO MAIN: a missing, duplicated
# or malformed pin that some site reads as "no pin, fetch the default branch".
# So the refusals are tested as hard as the happy path, and `checkout` is run
# against a local file:// fixture repo to prove it lands on the pinned commit —
# including a NON-tip commit and a persisted checkout that already exists.
#
# Run: bash .github/scripts/test-schemas-pin.sh
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUT="$HERE/schemas-pin.sh"
fails=0

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

pass() { printf 'ok   - %s\n' "$1"; }
fail() { printf 'FAIL - %s\n' "$1"; fails=$((fails + 1)); }

A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

# --- read -----------------------------------------------------------------
cat > "$TMP/good.conf" <<EOF
# header comment quoting an old pin: qontinui/qontinui-schemas $B
qontinui/ui-bridge $B

qontinui/qontinui-schemas   $A   # trailing comment
EOF
out="$(bash "$SUT" read "$TMP/good.conf" 2>&1)"
[ "$out" = "$A" ] && pass "read returns the pinned sha (ignores comments, other repos, spacing)" \
  || fail "read good conf: got '$out'"

expect_refusal() {
  local name="$1" conf="$2"
  if bash "$SUT" read "$conf" >/dev/null 2>&1; then
    fail "read must refuse: $name"
  else
    pass "read refuses: $name"
  fi
}
printf 'qontinui/ui-bridge %s\n' "$A" > "$TMP/missing.conf"
expect_refusal "no schemas entry" "$TMP/missing.conf"
printf 'qontinui/qontinui-schemas %s\nqontinui/qontinui-schemas %s\n' "$A" "$B" > "$TMP/dup.conf"
expect_refusal "duplicate schemas entry" "$TMP/dup.conf"
printf 'qontinui/qontinui-schemas main\n' > "$TMP/branch.conf"
expect_refusal "a branch name instead of a sha" "$TMP/branch.conf"
printf 'qontinui/qontinui-schemas %s\n' "${A:0:12}" > "$TMP/short.conf"
expect_refusal "an abbreviated sha" "$TMP/short.conf"
printf 'qontinui/qontinui-schemas %s\n' "${A^^}" > "$TMP/upper.conf"
expect_refusal "an uppercase sha" "$TMP/upper.conf"
printf 'qontinui/qontinui-schemas %s extra\n' "$A" > "$TMP/extra.conf"
expect_refusal "a trailing extra field" "$TMP/extra.conf"
printf '# qontinui/qontinui-schemas %s\n' "$A" > "$TMP/commented.conf"
expect_refusal "a commented-out entry" "$TMP/commented.conf"
expect_refusal "a missing file" "$TMP/does-not-exist.conf"

# --- rewrite --------------------------------------------------------------
cp "$TMP/good.conf" "$TMP/rw.conf"
if bash "$SUT" rewrite "$B" "$TMP/rw.conf" >/dev/null 2>&1 \
   && [ "$(bash "$SUT" read "$TMP/rw.conf")" = "$B" ]; then
  pass "rewrite moves the pin"
else
  fail "rewrite did not move the pin"
fi
# Only the schemas line changed; the header comment that quotes $B and the
# ui-bridge line are byte-identical.
if diff <(grep -v '^qontinui/qontinui-schemas' "$TMP/good.conf") \
        <(grep -v '^qontinui/qontinui-schemas' "$TMP/rw.conf") >/dev/null; then
  pass "rewrite touches only the schemas line"
else
  fail "rewrite changed lines other than the schemas pin"
fi
grep -q "^qontinui/qontinui-schemas   $B   # trailing comment$" "$TMP/rw.conf" \
  && pass "rewrite preserves spacing and the trailing comment" \
  || fail "rewrite mangled the schemas line: $(grep '^qontinui/qontinui-schemas' "$TMP/rw.conf")"
cp "$TMP/good.conf" "$TMP/rw-bad.conf"
if bash "$SUT" rewrite "not-a-sha" "$TMP/rw-bad.conf" >/dev/null 2>&1; then
  fail "rewrite must refuse a malformed sha"
elif cmp -s "$TMP/good.conf" "$TMP/rw-bad.conf"; then
  pass "rewrite refuses a malformed sha and leaves the file untouched"
else
  fail "rewrite refused but modified the file"
fi

# --- checkout (file:// fixture) -------------------------------------------
FIX="$TMP/fixture"
git init -q "$FIX"
git -C "$FIX" config user.email t@t; git -C "$FIX" config user.name t
# A non-tip sha is only fetchable by id when upload-pack allows it; github.com
# does, so the fixture must too or the test would prove nothing about the pin.
git -C "$FIX" config uploadpack.allowAnySHA1InWant true
echo one > "$FIX/f"; git -C "$FIX" add f; git -C "$FIX" commit -qm one
SHA1="$(git -C "$FIX" rev-parse HEAD)"
echo two > "$FIX/f"; git -C "$FIX" commit -qam two
SHA2="$(git -C "$FIX" rev-parse HEAD)"

printf 'qontinui/qontinui-schemas %s\n' "$SHA1" > "$TMP/co.conf"
mkdir -p "$TMP/work"
if SCHEMAS_REMOTE_URL="file://$FIX" bash "$SUT" checkout "$TMP/work" "$TMP/co.conf" >/dev/null 2>&1 \
   && [ "$(git -C "$TMP/work/qontinui-schemas" rev-parse HEAD)" = "$SHA1" ] \
   && [ "$(cat "$TMP/work/qontinui-schemas/f")" = "one" ]; then
  pass "checkout materialises a NON-tip pinned commit (not the default branch)"
else
  fail "checkout of a non-tip pin"
fi

# A persisted self-hosted checkout: dirty, with an untracked file, moved forward.
echo junk > "$TMP/work/qontinui-schemas/untracked"
echo dirty > "$TMP/work/qontinui-schemas/f"
printf 'qontinui/qontinui-schemas %s\n' "$SHA2" > "$TMP/co.conf"
if SCHEMAS_REMOTE_URL="file://$FIX" bash "$SUT" checkout "$TMP/work" "$TMP/co.conf" >/dev/null 2>&1 \
   && [ "$(git -C "$TMP/work/qontinui-schemas" rev-parse HEAD)" = "$SHA2" ] \
   && [ "$(cat "$TMP/work/qontinui-schemas/f")" = "two" ] \
   && [ ! -e "$TMP/work/qontinui-schemas/untracked" ]; then
  pass "checkout refreshes a persisted, dirty checkout onto the new pin"
else
  fail "checkout refresh of a persisted checkout"
fi

# An unfetchable pin fails loudly instead of leaving the old tree in place.
printf 'qontinui/qontinui-schemas %s\n' "$A" > "$TMP/co.conf"
if SCHEMAS_REMOTE_URL="file://$FIX" bash "$SUT" checkout "$TMP/work" "$TMP/co.conf" >/dev/null 2>&1; then
  fail "checkout must fail for a sha the remote does not have"
else
  pass "checkout fails for a sha the remote does not have"
fi

# --- verify-landed (file:// stand-in for the GitHub compare API) ----------
API="$TMP/api"
mkdir -p "$API/repos/qontinui/qontinui-schemas/compare"
printf 'qontinui/qontinui-schemas %s\n' "$A" > "$TMP/vl.conf"
compare_file="$API/repos/qontinui/qontinui-schemas/compare/$A...main"
vl() { SCHEMAS_API_BASE="file://$API" bash "$SUT" verify-landed "$TMP/vl.conf" >/dev/null 2>&1; }
for st in ahead identical; do
  printf '{"status": "%s", "files": [{"status": "modified"}]}' "$st" > "$compare_file"
  vl && pass "verify-landed accepts '$st'" || fail "verify-landed must accept '$st'"
done
for st in behind diverged; do
  printf '{"status": "%s"}' "$st" > "$compare_file"
  vl && fail "verify-landed must refuse '$st'" || pass "verify-landed refuses '$st' (pin not on main)"
done
printf '{"message": "No common ancestor"}' > "$compare_file"
vl && fail "verify-landed must refuse a response with no status" || pass "verify-landed refuses a response with no status"
printf 'not json' > "$compare_file"
vl && fail "verify-landed must refuse an unparseable response" || pass "verify-landed refuses an unparseable response"
rm -f "$compare_file"
vl && fail "verify-landed must fail when the API cannot be read" || pass "verify-landed fails (UNKNOWN) when the API cannot be read"

if [ "$fails" -ne 0 ]; then
  printf '\n%d test(s) failed\n' "$fails"
  exit 1
fi
printf '\nall schemas-pin tests passed\n'
