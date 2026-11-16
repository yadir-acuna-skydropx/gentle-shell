#!/usr/bin/env bash
# gentle-partitioner updater (S33, T38). Thin wrapper over install.sh --update:
# fetch + fast-forward the current branch only when the worktree is clean (never
# reset/force), upgrade gentle-shell, rebuild the engines when the commit
# changed, and bump security tools only when their pinned versions changed.
# Safe to run from cron. All flags are forwarded (e.g. --dry-run, --force,
# --no-security-tools).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
exec "$SCRIPT_DIR/install.sh" --update "$@"
