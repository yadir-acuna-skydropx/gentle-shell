#!/usr/bin/env bash
# Tests for the gentle-partitioner installer (S33, T38). Never runs the real
# installer against the system and never downloads tools: it exercises
#   1. syntax (bash -n) of install.sh, update.sh, lib/tools.sh
#   2. shellcheck when available
#   3. the --dry-run plan (every step + the pinned tool commands are printed,
#      and NO network/cargo/git command actually runs)
#   4. the checksum-mismatch path refuses a tool (simulated with a fake file)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
INSTALL="$SCRIPT_DIR/install.sh"
UPDATE="$SCRIPT_DIR/update.sh"
LIB="$SCRIPT_DIR/lib/tools.sh"
LIBE="$SCRIPT_DIR/lib/engram.sh"

TESTTMP="$(mktemp -d "${TMPDIR:-/tmp}/gp-test.XXXXXX")"
trap 'rm -rf "$TESTTMP"' EXIT
pass=0 fail=0
ok()   { pass=$((pass + 1)); printf 'ok   - %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf 'FAIL - %s\n' "$1"; }
check() { if "$@"; then ok "$DESC"; else bad "$DESC"; fi; }

# ---- 1. syntax -------------------------------------------------------------
for f in "$INSTALL" "$UPDATE" "$LIB" "$LIBE"; do
  DESC="bash -n $(basename "$f")"
  if bash -n "$f" 2>"$TESTTMP/bn.err"; then ok "$DESC"; else bad "$DESC"; cat "$TESTTMP/bn.err"; fi
done

# ---- 2. shellcheck (optional) ---------------------------------------------
if command -v shellcheck >/dev/null 2>&1; then
  for f in "$INSTALL" "$UPDATE" "$LIB" "$LIBE"; do
    DESC="shellcheck $(basename "$f")"
    if shellcheck -S warning "$f"; then ok "$DESC"; else bad "$DESC"; fi
  done
else
  printf 'skip - shellcheck not installed\n'
fi

# ---- 3. dry-run plan -------------------------------------------------------
# --force so the engine rebuild is always planned regardless of recorded commit.
PLAN="$(bash "$INSTALL" --dry-run --force 2>&1)"

assert_contains() {
  DESC="dry-run plans: $1"
  if printf '%s' "$PLAN" | grep -qF -- "$1"; then ok "$DESC"; else bad "$DESC"; fi
}
assert_absent() {
  DESC="dry-run does NOT run: $1"
  # In dry-run every real command is printed with a leading "    + "; assert the
  # command never appears WITHOUT that marker (i.e. it was never executed).
  if printf '%s' "$PLAN" | grep -E "^\s*\+ .*$1" >/dev/null; then ok "$DESC (printed, not run)"; else
    # also acceptable: the token simply never appears
    if printf '%s' "$PLAN" | grep -qF -- "$1"; then bad "$DESC"; else ok "$DESC (absent)"; fi
  fi
}

assert_contains "DRY RUN: no command will be executed"
assert_contains "[1/6] gentle-shell"
assert_contains "[2/6] Rust toolchain"
assert_contains "[3/6] Partitioner engines"
assert_contains "[4/6] gentle-shell launcher"
assert_contains "[5/6] Security tools"
assert_contains "[6/6] Engram memory server health"
assert_contains "engram: health check (planned)"

# --help lists the new Engram flags
HELP="$(bash "$INSTALL" --help 2>&1)"
DESC="--help lists --fix-engram"
if printf '%s' "$HELP" | grep -qF -- "--fix-engram"; then ok "$DESC"; else bad "$DESC"; fi
DESC="--help lists --no-engram"
if printf '%s' "$HELP" | grep -qF -- "--no-engram"; then ok "$DESC"; else bad "$DESC"; fi
assert_contains "cargo install --locked --force --path"
assert_contains "/partitioner/apps"
assert_contains "/partitioner/engines"
assert_contains "gitleaks/releases/download/v8.30.1"
assert_contains "osv-scanner/releases/download/v2.7.1"
assert_contains "trivy/releases/download/v0.76.2"
assert_contains "cargo install --locked cargo-audit --version 0.23.0"
assert_contains "semgrep==1.185.0"
assert_contains "gentle-explain key set"
assert_contains "gh auth refresh -s project"
# dry-run must not actually download / build
assert_absent "curl"

# ---- 4. checksum-mismatch refuses ------------------------------------------
# shellcheck source=lib/tools.sh
. "$LIB"
TMPD="$TESTTMP"
FAKE="$TMPD/fake-tool.bin"
printf 'not the real binary\n' > "$FAKE"
REAL_SHA="$(sha256_of "$FAKE")"
WRONG_SHA="0000000000000000000000000000000000000000000000000000000000000000"

DESC="verify_sha256 accepts a matching digest"
if verify_sha256 "$FAKE" "$REAL_SHA" 2>/dev/null; then ok "$DESC"; else bad "$DESC"; fi

DESC="verify_sha256 refuses a mismatched digest (non-zero)"
if verify_sha256 "$FAKE" "$WRONG_SHA" 2>/dev/null; then bad "$DESC"; else ok "$DESC"; fi

DESC="verify_sha256 refuses a missing file (non-zero)"
if verify_sha256 "$TMPD/does-not-exist" "$REAL_SHA" 2>/dev/null; then bad "$DESC"; else ok "$DESC"; fi

# ---- 5. dry-run update path (fast-forward only) ----------------------------
UPLAN="$(bash "$UPDATE" --dry-run 2>&1 || true)"
# Safety property: no planned/executed git command line (leading "+ ") may use a
# destructive verb. The ff-only intent is planned on a clean tree and skipped on
# a dirty one (the dev checkout is dirty), so accept either.
DESC="update.sh never plans a destructive git command"
if printf '%s' "$UPLAN" | grep -E '^[[:space:]]*\+ git .*(reset|--force|--hard| clean | checkout | restore | rebase)' >/dev/null; then
  bad "$DESC"
else
  ok "$DESC"
fi
DESC="update.sh plans fast-forward-only OR refuses on a dirty tree"
if printf '%s' "$UPLAN" | grep -qF "merge --ff-only" \
   || printf '%s' "$UPLAN" | grep -qF "worktree not clean"; then
  ok "$DESC"
else
  bad "$DESC"
fi

# ---- 6. engram helpers (pure) ----------------------------------------------
# shellcheck source=lib/engram.sh
. "$LIBE"

DESC="engram_parse_health_version extracts the version"
if [ "$(engram_parse_health_version '{"service":"engram","status":"ok","version":"3.7.2"}')" = "3.7.2" ]; then ok "$DESC"; else bad "$DESC"; fi

DESC="engram_parse_health_version is empty without a version"
if [ -z "$(engram_parse_health_version '{"status":"ok"}')" ]; then ok "$DESC"; else bad "$DESC"; fi

DESC="semver_lt: 3.5.2 < 3.7.2"
if semver_lt 3.5.2 3.7.2; then ok "$DESC"; else bad "$DESC"; fi

DESC="semver_lt: equal is not less"
if semver_lt 3.7.2 3.7.2; then bad "$DESC"; else ok "$DESC"; fi

DESC="semver_lt: numeric, 3.10.0 is not < 3.9.9"
if semver_lt 3.10.0 3.9.9; then bad "$DESC"; else ok "$DESC"; fi

DESC="semver_lt: numeric, 3.9.9 < 3.10.0"
if semver_lt 3.9.9 3.10.0; then ok "$DESC"; else bad "$DESC"; fi

FAKERT="$TESTTMP/runtime"
mkdir -p "$FAKERT/engram-3.5.2" "$FAKERT/engram-3.7.2" "$FAKERT/engram-3.10.1" "$FAKERT/not-engram"
DESC="engram_newest_runtime_version picks the highest X.Y.Z"
if [ "$(GENTLE_SHELL_RUNTIME_DIR="$FAKERT" engram_newest_runtime_version)" = "3.10.1" ]; then ok "$DESC"; else bad "$DESC"; fi

FAKESTORE="$TESTTMP/store"
mkdir -p "$FAKESTORE"
printf 'db\n'  > "$FAKESTORE/engram.db"
printf 'wal\n' > "$FAKESTORE/engram.db-wal"
BK="$(engram_backup_store "$FAKESTORE")"
DESC="engram_backup_store copies the db files and keeps the source"
if [ -f "$BK/engram.db" ] && [ -f "$BK/engram.db-wal" ] && [ -f "$FAKESTORE/engram.db" ]; then ok "$DESC"; else bad "$DESC"; fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
