#!/usr/bin/env bash
# gentle-partitioner — one installer + updater for everything, user space, no
# sudo (S33, T38). Idempotent; POSIX tools only.
#
#   install.sh                 fresh/idempotent install of the whole stack
#   install.sh --update        refresh everything (same as update.sh)
#   install.sh --dry-run       print every command without executing anything
#   install.sh --force         rebuild/reinstall even when nothing changed
#   install.sh --no-security-tools   skip gitleaks/osv/trivy/cargo-audit/semgrep
#
# What it installs, all under $HOME (never sudo):
#   1. gentle-shell (which itself pins gentle-ai and Engram)
#   2. Rust via rustup (user space) when cargo is missing
#   3. the partitioner engines (gentle-partitioner, gentle-apps, gentle-explain,
#      gentle-arch, gentle-sec, and gentle-session when present)
#   4. the gentle-shell launcher
#   5. security tools, pinned + SHA-256 verified (Risk item 5)
#
# gentle-shell sources are never modified (S1).
set -euo pipefail

# ---- locate ourselves and the repo ----------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
PART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"          # .../partitioner
REPO_ROOT="$(cd "$PART_DIR/.." && pwd)"           # repo checkout root
# shellcheck source=lib/tools.sh
. "$SCRIPT_DIR/lib/tools.sh"

# ---- user-space locations --------------------------------------------------
BIN_DIR="$HOME/.local/bin"
SHARE_DIR="$HOME/.local/share/gentle-partitioner"
TOOLS_DIR="$SHARE_DIR/tools"
VENV_DIR="$SHARE_DIR/semgrep-venv"
STATE_DIR="$HOME/.local/state/gentle-partitioner"
COMMIT_FILE="$STATE_DIR/installed-commit"

# ---- flags -----------------------------------------------------------------
UPDATE=0
DRY_RUN=0
FORCE=0
SECURITY_TOOLS=1

usage() {
  sed -n '2,20p' "$SCRIPT_DIR/install.sh" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --update) UPDATE=1 ;;
    --dry-run) DRY_RUN=1 ;;
    --force) FORCE=1 ;;
    --no-security-tools) SECURITY_TOOLS=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

# ---- output helpers --------------------------------------------------------
STEP_N=0
STEP_TOTAL=6
step() { STEP_N=$((STEP_N + 1)); printf '\n==> [%d/%d] %s\n' "$STEP_N" "$STEP_TOTAL" "$*"; }
note() { printf '    - %s\n' "$*"; }
warn() { printf '    ! %s\n' "$*" >&2; }

# Failures collected here are reported at the end; they never abort the install.
SUMMARY_OK=""
SUMMARY_FAIL=""
ok()   { SUMMARY_OK="$SUMMARY_OK  $1"$'\n'; }
fail() { SUMMARY_FAIL="$SUMMARY_FAIL  $1"$'\n'; warn "$1"; }

# run: execute a command, or print it in --dry-run. Use for every mutating or
# network command so --dry-run shows the full plan without side effects.
run() {
  if [ "$DRY_RUN" = 1 ]; then
    printf '    + %s\n' "$*"
    return 0
  fi
  "$@"
}

have() { command -v "$1" >/dev/null 2>&1; }

ensure_dirs() {
  run mkdir -p "$BIN_DIR" "$TOOLS_DIR" "$STATE_DIR"
}

# ===========================================================================
# 1. gentle-shell (pins gentle-ai + Engram itself; S1: never patch its sources)
# ===========================================================================
install_gentle_shell() {
  step "gentle-shell (pins gentle-ai + Engram)"
  if have gentle-shell; then
    note "gentle-shell present"
    if [ "$UPDATE" = 1 ] || [ "$FORCE" = 1 ]; then
      note "updating via its own upgrade command"
      run gentle-shell upgrade || fail "gentle-shell upgrade failed (run it manually)"
    fi
    ok "gentle-shell: present"
  else
    note "gentle-shell missing — installing the documented npm package (gentle-pi)"
    if have pi; then
      run pi install npm:gentle-pi && ok "gentle-shell: installed via pi" \
        || fail "gentle-shell: 'pi install npm:gentle-pi' failed"
    elif have npm; then
      run npm install -g gentle-pi && ok "gentle-shell: installed via npm" \
        || fail "gentle-shell: 'npm install -g gentle-pi' failed"
    elif have pnpm; then
      run pnpm add -g gentle-pi && ok "gentle-shell: installed via pnpm" \
        || fail "gentle-shell: 'pnpm add -g gentle-pi' failed"
    else
      fail "gentle-shell: no pi/npm/pnpm found — install Node.js or use the browser installer (README 'Path C')"
    fi
  fi
  report_managed_versions
}

# Best-effort: report the gentle-ai and Engram versions gentle-shell reports.
# Never fails the install; just prints what the tools say.
report_managed_versions() {
  if [ "$DRY_RUN" = 1 ]; then
    note "would report: gentle-shell --version; gentle-ai + Engram (pinned by gentle-shell)"
    return 0
  fi
  if have gentle-shell; then
    note "gentle-shell: $(gentle-shell --version 2>/dev/null | head -n1 || echo '?')"
  fi
  if have gentle-ai; then
    note "gentle-ai: $(gentle-ai --version 2>/dev/null | head -n1 || echo '?')"
  else
    note "gentle-ai: pinned and installed by gentle-shell (see 'gentle-shell doctor')"
  fi
  if have engram; then
    note "Engram: $(engram --version 2>/dev/null | head -n1 || echo '?')"
  else
    note "Engram: pinned and installed by gentle-shell (gentle-engram Pi package)"
  fi
}

# ===========================================================================
# 2. Rust (rustup, user space) when cargo is missing
# ===========================================================================
install_rust() {
  step "Rust toolchain (rustup, user space)"
  if have cargo; then
    note "cargo present: $(cargo --version 2>/dev/null || echo '?')"
    ok "rust: present"
    return 0
  fi
  note "cargo missing — installing rustup into \$HOME (no sudo)"
  local tmp
  tmp="$(mktemp "${TMPDIR:-/tmp}/rustup-init.XXXXXX.sh")"
  # Download to a file first, then run it — never pipe the network to a shell.
  run curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp" https://sh.rustup.rs
  run sh "$tmp" -y --profile minimal --default-toolchain stable
  run rm -f "$tmp"
  # Make cargo available to the rest of this run.
  if [ "$DRY_RUN" = 1 ]; then
    note "would source \$HOME/.cargo/env"
  elif [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
  fi
  if have cargo || [ "$DRY_RUN" = 1 ]; then ok "rust: installed via rustup"; else fail "rust: rustup install did not yield cargo"; fi
}

# ===========================================================================
# 3. Partitioner engines (cargo install --locked --path ...)
# ===========================================================================
engines_need_build() {
  # Fresh install or --force always builds. On --update, build only when the
  # checked-out commit differs from the recorded one.
  [ "$FORCE" = 1 ] && return 0
  [ "$UPDATE" = 1 ] || return 0
  local current installed
  current="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
  installed="$(cat "$COMMIT_FILE" 2>/dev/null || echo none)"
  [ "$current" != "$installed" ]
}

build_engines() {
  step "Partitioner engines (cargo install --locked)"
  if ! have cargo && [ "$DRY_RUN" != 1 ]; then
    fail "engines: cargo unavailable — skipping engine build"
    return 0
  fi
  if ! engines_need_build; then
    note "commit unchanged ($(cat "$COMMIT_FILE" 2>/dev/null)) — skipping rebuild (use --force to override)"
    ok "engines: up to date"
    return 0
  fi

  # An array keeps checkout paths that contain spaces intact.
  local -a paths=("$PART_DIR" "$PART_DIR/apps" "$PART_DIR/engines")
  # gentle-session is built only when its crate is present.
  if [ -d "$PART_DIR/session" ]; then
    paths+=("$PART_DIR/session")
    note "found partitioner/session — will install gentle-session"
  else
    note "partitioner/session absent — skipping gentle-session (added by another task)"
  fi

  local p build_failed=0
  for p in "${paths[@]}"; do
    note "cargo install --locked --path $p"
    run cargo install --locked --force --path "$p" || { build_failed=1; fail "engines: build failed for $p"; }
  done

  if [ "$build_failed" = 0 ]; then
    local current
    current="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
    if [ "$DRY_RUN" = 1 ]; then
      note "would record commit $current in $COMMIT_FILE"
    else
      printf '%s\n' "$current" > "$COMMIT_FILE"
    fi
    ok "engines: installed (commit ${current})"
  fi
}

# ===========================================================================
# 4. Launcher (reuse the existing idempotent installer)
# ===========================================================================
install_launcher() {
  step "gentle-shell launcher"
  local launcher="$SCRIPT_DIR/install-gentle-shell-launcher.sh"
  if [ -x "$launcher" ]; then
    run "$launcher" && ok "launcher: enabled" || fail "launcher: install-gentle-shell-launcher.sh failed"
  else
    fail "launcher: $launcher not found"
  fi
}

# ===========================================================================
# 5. Security tools (pinned + SHA-256 verified; each optional)
# ===========================================================================

# Record/skip a tool by pinned version so --update only rebuilds on a bump.
tool_state_file() { echo "$STATE_DIR/tool-$1"; }
tool_up_to_date() {
  # $1 name, $2 pinned version, $3 binary path (checked for existence)
  [ "$FORCE" = 1 ] && return 1
  [ -x "$3" ] || return 1
  local recorded
  recorded="$(cat "$(tool_state_file "$1")" 2>/dev/null || echo none)"
  [ "$recorded" = "$2" ]
}
tool_record() {
  if [ "$DRY_RUN" = 1 ]; then note "would record $1 version $2"; return 0; fi
  printf '%s\n' "$2" > "$(tool_state_file "$1")"
}

# Download a tar.gz, verify, extract one binary into BIN_DIR.
install_targz_tool() {
  # $1 name  $2 version  $3 url  $4 sha  $5 inner-binary-name
  local name="$1" ver="$2" url="$3" sha="$4" inner="$5"
  if tool_up_to_date "$name" "$ver" "$BIN_DIR/$name"; then
    note "$name $ver already installed — skipping"; ok "$name: $ver (present)"; return 0
  fi
  local td; td="$(mktemp -d "${TMPDIR:-/tmp}/gp-$name.XXXXXX")"
  local arc="$td/$name.tar.gz"
  note "$name $ver <- $url"
  run curl -fsSL --proto '=https' --tlsv1.2 -o "$arc" "$url" || { rm -rf "$td"; fail "$name: download failed"; return 0; }
  if [ "$DRY_RUN" = 1 ]; then
    note "would verify sha256=$sha, extract $inner -> $BIN_DIR/$name"
    rm -rf "$td"; tool_record "$name" "$ver"; ok "$name: $ver (planned)"; return 0
  fi
  if ! verify_sha256 "$arc" "$sha"; then
    rm -rf "$td"; fail "$name: SHA-256 mismatch — refused (not installed)"; return 0
  fi
  if ! tar -xzf "$arc" -C "$td"; then rm -rf "$td"; fail "$name: extract failed"; return 0; fi
  local found; found="$(find "$td" -type f -name "$inner" | head -n1)"
  if [ -z "$found" ]; then rm -rf "$td"; fail "$name: $inner not found in archive"; return 0; fi
  install -m 0755 "$found" "$BIN_DIR/$name" || { rm -rf "$td"; fail "$name: install into $BIN_DIR failed"; return 0; }
  rm -rf "$td"; tool_record "$name" "$ver"; ok "$name: $ver installed"
}

# Download a raw binary, verify, install into BIN_DIR.
install_raw_tool() {
  # $1 name  $2 version  $3 url  $4 sha
  local name="$1" ver="$2" url="$3" sha="$4"
  if tool_up_to_date "$name" "$ver" "$BIN_DIR/$name"; then
    note "$name $ver already installed — skipping"; ok "$name: $ver (present)"; return 0
  fi
  local td; td="$(mktemp -d "${TMPDIR:-/tmp}/gp-$name.XXXXXX")"
  local bin="$td/$name"
  note "$name $ver <- $url"
  run curl -fsSL --proto '=https' --tlsv1.2 -o "$bin" "$url" || { rm -rf "$td"; fail "$name: download failed"; return 0; }
  if [ "$DRY_RUN" = 1 ]; then
    note "would verify sha256=$sha, install -> $BIN_DIR/$name"
    rm -rf "$td"; tool_record "$name" "$ver"; ok "$name: $ver (planned)"; return 0
  fi
  if ! verify_sha256 "$bin" "$sha"; then
    rm -rf "$td"; fail "$name: SHA-256 mismatch — refused (not installed)"; return 0
  fi
  install -m 0755 "$bin" "$BIN_DIR/$name" || { rm -rf "$td"; fail "$name: install failed"; return 0; }
  rm -rf "$td"; tool_record "$name" "$ver"; ok "$name: $ver installed"
}

install_gitleaks() {
  local plat; plat="$(gitleaks_platform)"
  if [ -z "$plat" ]; then fail "gitleaks: unsupported platform $(uname -s)/$(uname -m)"; return 0; fi
  local sha; sha="$(sha_const "GITLEAKS_SHA_${plat}")"
  local url="https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_${plat}.tar.gz"
  install_targz_tool "gitleaks" "$GITLEAKS_VERSION" "$url" "$sha" "gitleaks"
}

install_osv_scanner() {
  local plat; plat="$(osv_platform)"
  if [ -z "$plat" ]; then fail "osv-scanner: unsupported platform $(uname -s)/$(uname -m)"; return 0; fi
  local sha; sha="$(sha_const "OSV_SHA_${plat}")"
  local url="https://github.com/google/osv-scanner/releases/download/v${OSV_SCANNER_VERSION}/osv-scanner_${plat}"
  install_raw_tool "osv-scanner" "$OSV_SCANNER_VERSION" "$url" "$sha"
}

install_trivy() {
  local plat; plat="$(trivy_platform)"
  if [ -z "$plat" ]; then fail "trivy: unsupported platform $(uname -s)/$(uname -m)"; return 0; fi
  # trivy constants use "_" where the asset uses "-"
  local key; key="$(printf '%s' "$plat" | tr '-' '_')"
  local sha; sha="$(sha_const "TRIVY_SHA_${key}")"
  local url="https://github.com/aquasecurity/trivy/releases/download/v${TRIVY_VERSION}/trivy_${TRIVY_VERSION}_${plat}.tar.gz"
  install_targz_tool "trivy" "$TRIVY_VERSION" "$url" "$sha" "trivy"
}

install_cargo_audit() {
  if ! have cargo && [ "$DRY_RUN" != 1 ]; then fail "cargo-audit: cargo unavailable"; return 0; fi
  if tool_up_to_date "cargo-audit" "$CARGO_AUDIT_VERSION" "$HOME/.cargo/bin/cargo-audit"; then
    note "cargo-audit $CARGO_AUDIT_VERSION already installed — skipping"; ok "cargo-audit: $CARGO_AUDIT_VERSION (present)"; return 0
  fi
  note "cargo install --locked cargo-audit --version $CARGO_AUDIT_VERSION"
  run cargo install --locked cargo-audit --version "$CARGO_AUDIT_VERSION" \
    && { tool_record "cargo-audit" "$CARGO_AUDIT_VERSION"; ok "cargo-audit: $CARGO_AUDIT_VERSION installed"; } \
    || fail "cargo-audit: cargo install failed"
}

install_semgrep() {
  if tool_up_to_date "semgrep" "$SEMGREP_VERSION" "$BIN_DIR/semgrep"; then
    note "semgrep $SEMGREP_VERSION already installed — skipping"; ok "semgrep: $SEMGREP_VERSION (present)"; return 0
  fi
  if ! have python3 && [ "$DRY_RUN" != 1 ]; then fail "semgrep: python3 not found"; return 0; fi
  note "python3 -m venv $VENV_DIR && pip install semgrep==$SEMGREP_VERSION"
  if [ "$DRY_RUN" = 1 ]; then
    note "would symlink $VENV_DIR/bin/semgrep -> $BIN_DIR/semgrep"
    tool_record "semgrep" "$SEMGREP_VERSION"; ok "semgrep: $SEMGREP_VERSION (planned)"; return 0
  fi
  if ! python3 -m venv "$VENV_DIR" 2>/dev/null; then
    fail "semgrep: 'python3 -m venv' failed — install the venv module, then re-run:"
    warn "      Debian/Ubuntu: apt-get install python3-venv   (needs an admin; no sudo here)"
    return 0
  fi
  if "$VENV_DIR/bin/pip" install --quiet --upgrade pip \
     && "$VENV_DIR/bin/pip" install --quiet "semgrep==$SEMGREP_VERSION"; then
    ln -sf "$VENV_DIR/bin/semgrep" "$BIN_DIR/semgrep"
    tool_record "semgrep" "$SEMGREP_VERSION"; ok "semgrep: $SEMGREP_VERSION installed"
  else
    fail "semgrep: pip install failed"
  fi
}

install_security_tools() {
  step "Security tools (pinned + SHA-256 verified)"
  if [ "$SECURITY_TOOLS" != 1 ]; then
    note "--no-security-tools: skipping gitleaks/osv-scanner/trivy/cargo-audit/semgrep"
    ok "security tools: skipped"
    return 0
  fi
  install_gitleaks
  install_osv_scanner
  install_trivy
  install_cargo_audit
  install_semgrep
}

# ===========================================================================
# Update: fetch + fast-forward the current branch only when the tree is clean
# ===========================================================================
update_repo() {
  [ "$UPDATE" = 1 ] || return 0
  printf '\n--> Update repository (fast-forward only, cron-safe)\n'
  if ! have git; then warn "git missing — cannot update the checkout"; return 0; fi
  if [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
    warn "worktree not clean — skipping git fetch/fast-forward (never reset or force)"
    return 0
  fi
  local branch; branch="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
  note "git fetch origin; git merge --ff-only origin/$branch"
  run git -C "$REPO_ROOT" fetch --quiet origin || { warn "git fetch failed"; return 0; }
  run git -C "$REPO_ROOT" merge --ff-only "origin/$branch" || warn "fast-forward not possible (diverged?) — left untouched"
}

# ===========================================================================
# Final summary + user checklist
# ===========================================================================
final_summary() {
  printf '\n========================================================\n'
  printf ' gentle-partitioner install %s\n' "$([ "$UPDATE" = 1 ] && echo '(update)' || echo '(fresh)')"
  printf '========================================================\n'
  printf '\nInstalled / ok:\n'
  if [ -n "$SUMMARY_OK" ]; then printf '%s' "$SUMMARY_OK"; else printf '  (none)\n'; fi
  if [ -n "$SUMMARY_FAIL" ]; then
    printf '\nReported issues (install continued):\n'
    printf '%s' "$SUMMARY_FAIL"
  fi

  printf '\nInstalled versions (best-effort):\n'
  if [ "$DRY_RUN" = 1 ]; then
    note "would print versions of gentle-partitioner, gentle-apps, gentle-explain, gentle-arch, gitleaks, osv-scanner, trivy, cargo-audit, semgrep"
  else
    local b
    for b in gentle-partitioner gentle-apps gentle-explain gentle-arch gentle-sec gentle-session \
             gitleaks osv-scanner trivy cargo-audit semgrep; do
      if have "$b"; then note "$b: $("$b" --version 2>/dev/null | head -n1 || echo '?')"; fi
    done
  fi

  cat <<'EOF'

Still needs you (manual, one-time):
  - gentle-explain key set          # set your Anthropic API key (explain/arch/helper engines)
  - gh auth login                   # GitHub connector
  - gh auth refresh -s project      # to edit Project fields (Status/Estimate/Iteration/Type)
  - gentle-apps slack login         # once the Slack workspace approves the app
  - Install a Nerd Font and select it in your terminal (icons render as boxes otherwise)
  - On Linux: a running Secret Service keyring (gnome-keyring) for stored tokens
  - Make ~/.local/bin is on your PATH, then: source ~/.bashrc  (or open a new shell)
EOF
}

main() {
  printf 'gentle-partitioner installer (repo: %s)\n' "$REPO_ROOT"
  [ "$DRY_RUN" = 1 ] && printf '*** DRY RUN: no command will be executed ***\n'
  ensure_dirs
  install_gentle_shell
  install_rust
  update_repo
  build_engines
  install_launcher
  install_security_tools
  final_summary
  # Non-zero exit when a hard requirement failed is intentionally avoided:
  # the installer is cron-safe and reports issues without failing the run.
}

# Only run when executed, not when sourced (tests source this file).
if [ "${GP_INSTALL_SOURCED:-0}" != 1 ]; then
  main "$@"
fi
