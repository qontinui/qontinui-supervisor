#!/usr/bin/env bash
# Hermetic tests for scripts/pre-commit-cargo.sh: which CARGO_TARGET_DIR the
# pre-commit cargo hook builds into. A stub `cargo` on PATH prints the value it
# received, so no real cargo runs.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/../.." && pwd)/scripts/pre-commit-cargo.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/bin"
cat > "$tmp/bin/cargo" <<'EOF'
#!/usr/bin/env bash
# Report WHERE it would build by reading a marker inside that dir, so the
# check is independent of how this platform spells the path.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  echo "TARGET=$(cat "$CARGO_TARGET_DIR/MARKER" 2>/dev/null || echo "<no marker at $CARGO_TARGET_DIR>") ARGS=$*"
else
  echo "TARGET=<unset> ARGS=$*"
fi
EOF
chmod +x "$tmp/bin/cargo"
export PATH="$tmp/bin:$PATH"
unset CARGO_TARGET_DIR

git init -q "$tmp/prim"
git -C "$tmp/prim" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
git -C "$tmp/prim" worktree add -q "$tmp/wt" -b wt
mkdir -p "$tmp/prim/target" "$tmp/wt/target" "$tmp/chosen"
echo primary-target > "$tmp/prim/target/MARKER"
echo worktree-target > "$tmp/wt/target/MARKER"   # the dir it must NOT pick
echo chosen-target > "$tmp/chosen/MARKER"

fail=0
check() { # <name> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "ok   - $1"; else echo "FAIL - $1: expected [$2], got [$3]"; fail=1; fi
}

out="$(cd "$tmp/wt" && bash "$SCRIPT" clippy -- -D warnings 2>/dev/null)"
check "linked worktree builds into the primary's target" \
  "TARGET=primary-target ARGS=clippy -- -D warnings" "$out"

out="$(cd "$tmp/prim" && bash "$SCRIPT" clippy 2>/dev/null)"
check "primary checkout leaves CARGO_TARGET_DIR unset" "TARGET=<unset> ARGS=clippy" "$out"

out="$(cd "$tmp/wt" && CARGO_TARGET_DIR="$tmp/chosen" bash "$SCRIPT" clippy 2>/dev/null)"
check "a preset CARGO_TARGET_DIR wins" "TARGET=chosen-target ARGS=clippy" "$out"

exit "$fail"
