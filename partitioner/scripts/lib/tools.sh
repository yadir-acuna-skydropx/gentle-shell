#!/usr/bin/env bash
# gentle-partitioner installer — pinned security-tool versions, checksums and
# helpers (S33, T38). Sourced by install.sh. POSIX tools only (curl, tar,
# sha256sum/shasum, awk, mktemp). No side effects on source.
#
# Supply-chain policy (Risk item 5): every downloaded release is pinned to an
# exact version and verified against a SHA-256 constant recorded here BEFORE it
# is extracted or installed. A mismatch refuses that tool; it never aborts the
# whole install. Update these constants (and nothing else) to bump a tool; the
# updater rebuilds a tool only when the pinned version below changes.
#
# Sources (looked up from the official GitHub releases / registries, 2026):
#   gitleaks     https://github.com/gitleaks/gitleaks/releases/tag/v8.30.1
#                gitleaks_8.30.1_checksums.txt
#   osv-scanner  https://github.com/google/osv-scanner/releases/tag/v2.7.1
#                osv-scanner_SHA256SUMS  (raw binaries, no archive)
#   trivy        https://github.com/aquasecurity/trivy/releases/tag/v0.76.2
#                trivy_0.76.2_checksums.txt
#   cargo-audit  crates.io max_stable 0.23.0 (built from source, cargo --locked)
#   semgrep      PyPI 1.185.0 (installed into a dedicated venv)

# ---- pinned versions -------------------------------------------------------
GITLEAKS_VERSION="8.30.1"
OSV_SCANNER_VERSION="2.7.1"
TRIVY_VERSION="0.76.2"
CARGO_AUDIT_VERSION="0.23.0"
SEMGREP_VERSION="1.185.0"

# ---- pinned SHA-256 digests (per platform asset) ---------------------------
# gitleaks_<ver>_<os>_<arch>.tar.gz
GITLEAKS_SHA_linux_x64="551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb"
GITLEAKS_SHA_linux_arm64="e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080"
GITLEAKS_SHA_darwin_x64="dfe101a4db2255fc85120ac7f3d25e4342c3c20cf749f2c20a18081af1952709"
GITLEAKS_SHA_darwin_arm64="b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5"

# osv-scanner_<os>_<arch>  (raw binary)
OSV_SHA_linux_amd64="fb7dab19a8bd9bbe5c3189f09d4b7ee1820c9553a23bac202648a56efd2f6ee8"
OSV_SHA_linux_arm64="983d22fbad9724514e3fcc6acc83b5fa64c5ffa6b0d9f510028f22c1b223ff81"
OSV_SHA_darwin_amd64="b8420d5485ba792626315f1bf9a5a2046e971d38eea75bf9090c38852dbc3b1b"
OSV_SHA_darwin_arm64="b22c02877a811c8f78956e8643efa554932b7378f590d13f8e97d4e9448a29f3"

# trivy_<ver>_<OS>-<arch>.tar.gz
TRIVY_SHA_Linux_64bit="bc0fc726e017b8927a62b2d3531d7a84ee01769daf5c99ed866dced54089fdbf"
TRIVY_SHA_Linux_ARM64="ab9d51f86e6a5fb936f605faff6b593ac51bc378fff4d3e8b9d58358b9c367c3"
TRIVY_SHA_macOS_64bit="e08bb9ea449ecf66c6bfb03cbb8697c503ff33d35fc585c9747e3eaeb7ed4c7b"
TRIVY_SHA_macOS_ARM64="ae7ea358326960406b703c77b2ac490190797d80b306b5b50c2a27221da0a5b8"

# ---- sha256 helper ---------------------------------------------------------
# Prints the SHA-256 of a file using whatever POSIX tool is available.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "no sha256 tool (need sha256sum or shasum)" >&2
    return 2
  fi
}

# Verify a file against an expected digest. Returns non-zero on mismatch and
# never deletes anything the caller did not create. Self-contained so tests can
# source this file and exercise the refuse-on-mismatch path with a fake file.
verify_sha256() {
  # $1 = file, $2 = expected sha256
  local file="$1" expected="$2" got
  [ -f "$file" ] || { echo "verify_sha256: missing file $file" >&2; return 2; }
  got="$(sha256_of "$file")" || return 2
  if [ "$got" != "$expected" ]; then
    echo "CHECKSUM MISMATCH: $file" >&2
    echo "  expected $expected" >&2
    echo "  got      $got" >&2
    return 1
  fi
  return 0
}

# Resolve the platform tag for each tool. Echoes "" when the platform is not a
# published target (the caller then skips that tool with a clear message).
# gitleaks tag: linux_x64 | linux_arm64 | darwin_x64 | darwin_arm64
gitleaks_platform() {
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64|Linux/amd64)   echo "linux_x64" ;;
    Linux/aarch64|Linux/arm64)  echo "linux_arm64" ;;
    Darwin/x86_64)              echo "darwin_x64" ;;
    Darwin/arm64)               echo "darwin_arm64" ;;
    *) echo "" ;;
  esac
}

# osv-scanner tag: linux_amd64 | linux_arm64 | darwin_amd64 | darwin_arm64
osv_platform() {
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64|Linux/amd64)   echo "linux_amd64" ;;
    Linux/aarch64|Linux/arm64)  echo "linux_arm64" ;;
    Darwin/x86_64)              echo "darwin_amd64" ;;
    Darwin/arm64)               echo "darwin_arm64" ;;
    *) echo "" ;;
  esac
}

# trivy tag: Linux-64bit | Linux-ARM64 | macOS-64bit | macOS-ARM64
trivy_platform() {
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64|Linux/amd64)   echo "Linux-64bit" ;;
    Linux/aarch64|Linux/arm64)  echo "Linux-ARM64" ;;
    Darwin/x86_64)              echo "macOS-64bit" ;;
    Darwin/arm64)               echo "macOS-ARM64" ;;
    *) echo "" ;;
  esac
}

# Look up a pinned SHA constant by name indirection (bash 3.2+ safe).
sha_const() { eval "printf '%s' \"\${$1:-}\""; }
