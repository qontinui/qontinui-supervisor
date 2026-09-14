#!/usr/bin/env bash
# The ONE reader/writer of coord's qontinui-schemas sibling pin.
#
# Usage:
#   schemas-pin.sh read     [conf]                # print the pinned 40-hex SHA
#   schemas-pin.sh rewrite  <new-sha> [conf]      # move the pin, nothing else
#   schemas-pin.sh checkout <parent-dir> [conf]   # materialise <parent-dir>/qontinui-schemas at the pin
#
# Env:
#   GH_TOKEN               optional; sent as an auth header on fetch (never persisted)
#   SCHEMAS_REMOTE_URL     override the fetch URL (tests use a file:// fixture)
#
# ---------------------------------------------------------------------------
# WHY THIS EXISTS (plan 2026-08-31-schemas-releases-strand-consumer-cargo-locks §7.4 Phase 3b)
#
# qontinui-supervisor path-depends on ../qontinui-schemas/{rust,rust-runner-client},
# and Cargo.lock records a path crate's VERSION but no COMMIT. CI used to
# `git clone --depth 1` schemas MAIN, so the tree CI compiled was (supervisor SHA
# + schemas main): every qontinui-types release left the committed lock stale
# (supervisor#189 was the hand fix for 2.0.0), and nothing caught it because
# nothing ran `--locked`.
#
# The schemas commit is now recorded in `.github/sibling-pins.conf` and every CI
# checkout reads it through this script. A schemas release reaches supervisor
# only through a PR that moves the pin AND Cargo.lock together
# (`.github/workflows/schemas-pin-bump.yml`, or by hand), and ci.yml's
# `cargo metadata --locked` step verifies that pair on that PR.
#
# This is a copy of qontinui-coord's `.github/scripts/schemas-pin.sh` (same
# subcommands, same file format as qontinui-runner's `.github/sibling-pins.conf`).
# Only the qontinui-schemas line is read here.
set -euo pipefail

SLUG="qontinui/qontinui-schemas"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_CONF="$HERE/../sibling-pins.conf"

die() { printf 'schemas-pin: %s\n' "$*" >&2; exit 1; }

is_sha() { [[ "$1" =~ ^[0-9a-f]{40}$ ]]; }

# Exactly one well-formed line for $SLUG, or refuse. A missing, duplicated or
# malformed pin must never degrade to "fetch main": that is the defect.
read_pin() {
  local conf="$1"
  [ -f "$conf" ] || die "pin file not found: $conf"
  local lines
  lines="$(awk -v slug="$SLUG" '
    { sub(/#.*/, "") }            # strip comments
    NF == 0 { next }
    $1 == slug { print NF " " $2 }
  ' "$conf")"
  local n
  n="$(printf '%s' "$lines" | grep -c . || true)"
  [ "$n" -eq 1 ] || die "expected exactly one '$SLUG' entry in $conf, found $n"
  local fields sha
  fields="${lines%% *}"
  sha="${lines#* }"
  [ "$fields" -eq 2 ] || die "'$SLUG' line in $conf must be '<owner>/<repo> <sha>', got $fields fields"
  is_sha "$sha" || die "'$SLUG' pin in $conf is not a lowercase 40-hex SHA: $sha"
  printf '%s\n' "$sha"
}

cmd="${1:-}"; shift || true
case "$cmd" in
  read)
    read_pin "${1:-$DEFAULT_CONF}"
    ;;

  rewrite)
    new="${1:-}"; conf="${2:-$DEFAULT_CONF}"
    is_sha "$new" || die "rewrite: new SHA must be lowercase 40-hex, got: '$new'"
    old="$(read_pin "$conf")"
    # Replace the SHA token on the one matching line only; comments elsewhere
    # (which may quote old SHAs) are left byte-for-byte alone.
    tmp="$(mktemp)"
    awk -v slug="$SLUG" -v old="$old" -v new="$new" '
      { line = $0; code = line; sub(/#.*/, "", code); delete f; split(code, f, " ") }
      f[1] == slug && index(line, old) { sub(old, new, line) }
      { print line }
    ' "$conf" > "$tmp"
    cat "$tmp" > "$conf"; rm -f "$tmp"
    [ "$(read_pin "$conf")" = "$new" ] || die "rewrite: read-back did not return $new"
    printf '%s -> %s\n' "$old" "$new"
    ;;

  checkout)
    parent="${1:-}"; conf="${2:-$DEFAULT_CONF}"
    [ -n "$parent" ] || die "checkout: <parent-dir> is required"
    sha="$(read_pin "$conf")"
    url="${SCHEMAS_REMOTE_URL:-https://github.com/$SLUG.git}"
    dest="$parent/qontinui-schemas"
    # Authenticate the fetch without persisting the token into .git/config: a
    # self-hosted runner keeps $_work across jobs, and anonymous git from a
    # rate-limited runner IP is answered 401 (coord main-red 2026-09-02).
    auth=()
    if [ -n "${GH_TOKEN:-}" ]; then
      auth=(-c "http.https://github.com/.extraheader=AUTHORIZATION: basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 -w0)")
    fi
    if [ ! -d "$dest/.git" ]; then
      mkdir -p "$dest"
      git -C "$dest" init -q
    fi
    # Fetch BY SHA from the explicit URL (not a remote name) so a persisted
    # checkout whose origin points elsewhere cannot answer with the wrong repo.
    git "${auth[@]}" -C "$dest" fetch -q --depth 1 "$url" "$sha"
    git -C "$dest" -c advice.detachedHead=false checkout -q --force FETCH_HEAD
    git -C "$dest" clean -qfdx
    got="$(git -C "$dest" rev-parse HEAD)"
    [ "$got" = "$sha" ] || die "checkout: $dest is at $got, expected pinned $sha"
    printf 'qontinui-schemas pinned at %s (%s)\n' "$sha" "$dest"
    ;;

  verify-landed)
    # The pin must be ON schemas main. A pin to an unmerged schemas commit
    # compiles green against a tree nobody else has, and reds main later if
    # that branch is force-pushed or deleted. Nothing in coord's flow needs an
    # unlanded pin: schemas' consumer gate does not compile coord, so the
    # schemas half always lands first (plan §7.2). An unreadable answer is
    # UNKNOWN and fails too — never "landed".
    conf="${1:-$DEFAULT_CONF}"
    sha="$(read_pin "$conf")"
    api="${SCHEMAS_API_BASE:-https://api.github.com}"
    url="$api/repos/$SLUG/compare/$sha...main"
    # The token travels in a curl config on a pipe, never on argv.
    body="$(curl -fsSL --retry 3 --retry-delay 2 --retry-all-errors \
      -K <(printf 'header = "Accept: application/vnd.github+json"\n'; \
           [ -n "${GH_TOKEN:-}" ] && printf 'header = "Authorization: Bearer %s"\n' "$GH_TOKEN") \
      "$url")" || die "verify-landed: could not read $url — UNKNOWN, not landed"
    status="$(printf '%s' "$body" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status", ""))')" \
      || die "verify-landed: unparseable compare response from $url"
    case "$status" in
      ahead|identical)
        printf 'pinned %s is on %s main (compare: %s)\n' "$sha" "$SLUG" "$status" ;;
      *)
        die "verify-landed: pinned $sha is '${status:-<none>}' relative to $SLUG main — NOT landed. Land the schemas change first, then pin a commit that is on main." ;;
    esac
    ;;

  *)
    die "usage: schemas-pin.sh {read [conf] | rewrite <sha> [conf] | checkout <parent-dir> [conf] | verify-landed [conf]}"
    ;;
esac
