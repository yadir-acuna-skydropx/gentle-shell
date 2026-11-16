#!/usr/bin/env bash
# gentle-partitioner installer — Engram server health helpers (S36, T39).
# Sourced by install.sh. POSIX-ish bash; no side effects on source.
#
# Why this exists (S36): Gentle Shell shares one Engram server on
# 127.0.0.1:7437. It reuses a listener only with verified /compatibility
# evidence and, by design, NEVER stops a server already listening there
# (data-safety). So when a stale lower-version Engram `serve` is orphaned on
# that port (e.g. left over from a pre-upgrade launch), every Shell start finds
# an incompatible server, falls back to the guard ENGRAM_BIN, and memory stays
# disabled — recurring until the old server is removed by hand.
#
# These helpers let the installer/updater (our code; gentle-shell sources stay
# untouched, S1) DETECT that situation and, backup-first, reconcile it: stop the
# strictly-older server and start the bundled one. They only ever act on a
# version strictly older than the newest bundled runtime, and never delete the
# store.

# Port Engram listens on (Engram's own default is 7437).
engram_port() { printf '%s' "${ENGRAM_PORT:-7437}"; }

# Data directory Engram opens (its own default is ~/.engram).
engram_store_dir() { printf '%s' "${ENGRAM_DATA_DIR:-$HOME/.engram}"; }

# Base directory holding the bundled `engram-<version>/` runtimes.
engram_runtime_base() { printf '%s' "${GENTLE_SHELL_RUNTIME_DIR:-$HOME/.gentle-shell/runtime}"; }

# Extract the "version":"X.Y.Z" value from an Engram /health JSON body.
# Reads $1 if given, otherwise stdin. Pure; empty when absent.
engram_parse_health_version() {
  local json="${1-}"
  if [ "$#" -eq 0 ]; then json="$(cat)"; fi
  printf '%s' "$json" \
    | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([0-9][0-9.]*\)".*/\1/p' \
    | head -n1
}

# True (0) iff $1 < $2, where both are plain X.Y.Z versions. Equal is false.
semver_lt() {
  [ "$1" = "$2" ] && return 1
  local lowest
  lowest="$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n 2>/dev/null | head -n1)"
  [ "$lowest" = "$1" ]
}

# Newest bundled runtime version (highest engram-X.Y.Z under the runtime base).
# Empty when none is present.
engram_newest_runtime_version() {
  local base best="" ver d
  base="$(engram_runtime_base)"
  [ -d "$base" ] || return 0
  for d in "$base"/engram-*; do
    [ -d "$d" ] || continue
    ver="${d##*/engram-}"
    case "$ver" in
      [0-9]*.[0-9]*.[0-9]*) ;;
      *) continue ;;
    esac
    if [ -z "$best" ] || semver_lt "$best" "$ver"; then best="$ver"; fi
  done
  printf '%s' "$best"
}

# Path to the engram binary of the newest bundled runtime; empty when none.
engram_newest_runtime_bin() {
  local base best="" bestbin="" ver d
  base="$(engram_runtime_base)"
  [ -d "$base" ] || return 0
  for d in "$base"/engram-*; do
    [ -x "$d/engram" ] || continue
    ver="${d##*/engram-}"
    case "$ver" in
      [0-9]*.[0-9]*.[0-9]*) ;;
      *) continue ;;
    esac
    if [ -z "$best" ] || semver_lt "$best" "$ver"; then best="$ver"; bestbin="$d/engram"; fi
  done
  printf '%s' "$bestbin"
}

# Version reported by the server currently listening on the Engram port, if any.
# Empty when nothing answers, curl is missing, or the body has no version.
engram_running_version() {
  command -v curl >/dev/null 2>&1 || return 0
  local port body
  port="$(engram_port)"
  body="$(curl -fsS -m 3 "http://127.0.0.1:${port}/health" 2>/dev/null)" || return 0
  engram_parse_health_version "$body"
}

# PID listening on the Engram port (best effort via ss, then lsof). Empty when
# unknown.
engram_listener_pid() {
  local port; port="$(engram_port)"
  if command -v ss >/dev/null 2>&1; then
    ss -ltnpH "sport = :${port}" 2>/dev/null | sed -n 's/.*pid=\([0-9]\{1,\}\).*/\1/p' | head -n1
  elif command -v lsof >/dev/null 2>&1; then
    lsof -ti "tcp:${port}" -sTCP:LISTEN 2>/dev/null | head -n1
  fi
}

# Copy the store's db files into a timestamped backup dir inside the store.
# Echoes the backup path. Never deletes anything. Non-zero only on mkdir/copy
# failure.
engram_backup_store() {
  local store="$1" ts dest f
  ts="$(date +%Y%m%d-%H%M%S)"
  dest="$store/backup-reconcile-$ts"
  mkdir -p "$dest" || return 1
  for f in engram.db engram.db-wal engram.db-shm; do
    if [ -f "$store/$f" ]; then
      cp -p "$store/$f" "$dest/" || return 1
    fi
  done
  printf '%s' "$dest"
}
