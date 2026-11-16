# gentle-shell launcher (S2, T26) — make `gentle-shell` open the
# gentle-partitioner visual wrapper, while the real gentle-shell engine still
# runs as the partitioner's child.
#
# No recursion: the partitioner resolves its child via PATH (not shell
# functions) and exports GENTLE_PARTITIONER=1, so inside it this function runs
# the real `gentle-shell` binary instead of launching another partitioner.
#
# Persistence (S34): when `gentle-session` is installed and we are not already
# inside a background session, the partitioner is wrapped in a per-folder
# `gentle-session attach`, so closing the terminal or dropping SSH leaves it
# running and reopening `gentle-shell` in the same folder reattaches. The
# session server exports GENTLE_SESSION=1 to its child, so this function never
# nests a session inside another. Set GENTLE_NO_SESSION=1 to opt out.
#
# Enable by sourcing this file from your shell rc (~/.bashrc or ~/.zshrc):
#     . /path/to/partitioner/scripts/gentle-shell-launcher.sh
# or run partitioner/scripts/install-gentle-shell-launcher.sh once.
gentle-shell() {
  if [ -n "${GENTLE_PARTITIONER:-}" ] || ! command -v gentle-partitioner >/dev/null 2>&1; then
    # Already inside the partitioner, or it is not installed: run the real engine.
    command gentle-shell "$@"
  elif [ -z "${GENTLE_SESSION:-}" ] && [ -z "${GENTLE_NO_SESSION:-}" ] \
    && command -v gentle-session >/dev/null 2>&1; then
    # Not inside a session and not opted out: keep the partitioner alive in a
    # per-folder background session and attach (or reattach) to it.
    command gentle-session attach --dir "$PWD" -- gentle-partitioner "$@"
  else
    command gentle-partitioner "$@"
  fi
}
