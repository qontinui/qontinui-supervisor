#!/usr/bin/env bash
# Run cargo for a pre-commit hook WITHOUT minting a fresh per-worktree target dir.
#
# The `cargo-clippy` hook in .pre-commit-config.yaml used to run bare
# `cargo clippy`. From a linked git worktree (every agent worktree on this
# fleet) that compiles the whole dependency graph into `<worktree>/target/`, a
# brand-new multi-GB target dir per worktree — the disk-full hazard the fleet's
# build wrappers exist to prevent. The commit-time build is invisible to the
# agent PreToolUse guards, because git runs it, not a tool call.
#
# Rule: if CARGO_TARGET_DIR is already set, it wins (an operator or wrapper
# chose it). Otherwise, inside a LINKED worktree, build into the PRIMARY
# checkout's `target/` — the shared dir a build of that checkout already uses.
# In the primary checkout itself nothing is set, so cargo's default applies.
set -euo pipefail

if [ -z "${CARGO_TARGET_DIR:-}" ]; then
  top="$(git rev-parse --show-toplevel)"
  common="$(git rev-parse --path-format=absolute --git-common-dir)"
  primary="$(dirname "$common")"
  if [ "$(cd "$primary" && pwd -P)" != "$(cd "$top" && pwd -P)" ]; then
    export CARGO_TARGET_DIR="$primary/target"
    echo "pre-commit-cargo: linked worktree; building into the primary's shared target: $CARGO_TARGET_DIR" >&2
  fi
fi

exec cargo "$@"
