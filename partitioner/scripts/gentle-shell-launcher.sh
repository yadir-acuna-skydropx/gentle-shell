# gentle-shell launcher (S2, T26) — make `gentle-shell` open the
# gentle-partitioner visual wrapper, while the real gentle-shell engine still
# runs as the partitioner's child.
#
# No recursion: the partitioner resolves its child via PATH (not shell
# functions) and exports GENTLE_PARTITIONER=1, so inside it this function runs
# the real `gentle-shell` binary instead of launching another partitioner.
#
# Enable by sourcing this file from your shell rc (~/.bashrc or ~/.zshrc):
#     . /path/to/partitioner/scripts/gentle-shell-launcher.sh
# or run partitioner/scripts/install-gentle-shell-launcher.sh once.
gentle-shell() {
  if [ -n "${GENTLE_PARTITIONER:-}" ] || ! command -v gentle-partitioner >/dev/null 2>&1; then
    # Already inside the partitioner, or it is not installed: run the real engine.
    command gentle-shell "$@"
  else
    command gentle-partitioner "$@"
  fi
}
