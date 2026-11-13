#!/usr/bin/env bash
# Enables the gentle-shell launcher (T26): typing `gentle-shell` opens the
# gentle-partitioner wrapper. Idempotent; pass an explicit rc file as $1 to
# override the shell autodetection.
set -eu

here="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
launcher="$here/gentle-shell-launcher.sh"
[ -f "$launcher" ] || { echo "launcher not found: $launcher" >&2; exit 1; }

rc="${1:-}"
if [ -z "$rc" ]; then
  case "${SHELL:-}" in
    *zsh) rc="$HOME/.zshrc" ;;
    *)    rc="$HOME/.bashrc" ;;
  esac
fi

line=". \"$launcher\"  # gentle-shell -> gentle-partitioner (T26)"
if grep -qF "$launcher" "$rc" 2>/dev/null; then
  echo "Already enabled in $rc"
else
  printf '\n%s\n' "$line" >> "$rc"
  echo "Enabled in $rc"
fi
echo "Open a new shell, or run:  source \"$rc\""
echo "Then \`gentle-shell\` opens the partitioner. Use \`command gentle-shell\` for the raw engine."
