#!/usr/bin/env bash
# Build GitMesh in release mode, from any directory, with no configuration.
#
#   ./build.sh                 build (uses an existing Rust, or asks before installing one)
#   ./build.sh --install-rust  build, and allow installing Rust if none is found (no prompt)
#   ./build.sh --help
#
# What it does:
#   1. checks for a C linker (cc), which Rust needs to link programs on Linux and macOS;
#   2. uses the Rust toolchain already on your system if it works and is new enough;
#   3. otherwise, with your consent, installs stable Rust for your user only (no sudo);
#   4. runs `cargo build --release --locked` and prints where the executable is.
# It does NOT start GitMesh, install system packages, or delete build output.
# See README.md ("Quick start") for supported platforms.

# Re-run under bash when started with sh, dash, or similar.
if [ -z "${BASH_VERSION:-}" ]; then exec bash "$0" "$@"; fi
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: ./build.sh [--install-rust] [--help]

Builds the GitMesh release executable (target/release/gitmesh, or your CARGO_TARGET_DIR).
If no suitable Rust is installed, it asks before installing Rust for your user account.
Use --install-rust to allow that installation without a prompt (for scripts).
EOF
}

die() {
  echo "build.sh: error: $*" >&2
  exit 1
}

install_consent=0
for arg in "$@"; do
  case "$arg" in
    --install-rust) install_consent=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "build.sh: error: unknown argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

# The project root is the directory that contains this script, wherever it was started from.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir"
[ -f Cargo.toml ] || die "Cargo.toml not found in $script_dir; is this the GitMesh source tree?"
[ -f tools/toolchain.sh ] || die "tools/toolchain.sh not found in $script_dir; the source tree is incomplete"
# shellcheck source=tools/toolchain.sh
. tools/toolchain.sh

min_rust="$(gm_min_rust "$script_dir")"
[ -n "$min_rust" ] || die "Cargo.toml does not declare rust-version; cannot choose a Rust version"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/gitmesh-build.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

echo "build.sh: GitMesh needs Rust $min_rust or newer, and a C linker (cc)."

# ---- 1. C linker (checked first, so no download happens for a machine that cannot link)
if ! gm_linker_present; then
  gm_error "no C linker (cc) was found, and Rust needs one to link GitMesh."
  gm_note "Install it, then run ./build.sh again:"
  gm_native_hint /etc/os-release >&2
  exit 1
fi

# ---- 2. toolchain
status=0
gm_discover "$script_dir" "$min_rust" || status=$?

if [ "$status" -eq 1 ] && [ "${GM_RUSTUP_PRESENT:-0}" -eq 1 ]; then
  die "a rustup installation exists, but it has no working toolchain. It was NOT overwritten.
  Repair it with:  rustup default stable   (then run ./build.sh again)"
fi

if [ "$status" -eq 1 ]; then
  cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  gm_note "no Rust toolchain was found on PATH or in $cargo_home/bin."
  if [ "$install_consent" -eq 0 ]; then
    if [ -t 0 ] && [ -t 1 ]; then
      gm_note ""
      gm_note "This downloads the official rustup installer (verified against its published"
      gm_note "SHA-256 checksum) and installs stable Rust for your user account only, into:"
      gm_note "  $cargo_home  and  ${RUSTUP_HOME:-$HOME/.rustup}"
      gm_note "It needs no administrator rights and does not edit your shell start-up files."
      gm_note ""
      printf 'Install Rust now? [y/N] ' >&2
      answer=""
      read -r answer || answer=""
      case "$answer" in
        y|Y|yes|YES) ;;
        *) die "installation cancelled; nothing was changed. Install Rust yourself (https://rustup.rs or your package manager) and run ./build.sh again." ;;
      esac
    else
      die "Rust is not installed and this run is not interactive, so nothing was installed.
  Either re-run with:  ./build.sh --install-rust
  or install Rust yourself (https://rustup.rs or your package manager) and run ./build.sh again."
    fi
  fi
  if ! gm_bootstrap_rustup "$tmp"; then
    die "$GM_REASON"
  fi
  status=0
  gm_discover "$script_dir" "$min_rust" || status=$?
  if [ "$status" -ne 0 ]; then
    die "Rust was installed, but it still does not work: $GM_REASON"
  fi
fi

if [ "$status" -eq 2 ]; then
  die "the Rust toolchain found on this system cannot be used: $GM_REASON.
  This script will not overwrite it. Options:
    - if rustup manages it:  rustup update stable   (or: rustup default stable)
    - if it comes from your system packages: update the rust/cargo packages, or install a
      newer Rust from https://rustup.rs
  Then run ./build.sh again."
fi

echo "build.sh: using cargo $GM_CARGO_VERSION and rustc $GM_RUSTC_VERSION"
echo "build.sh: from $GM_SOURCE"

# ---- 3. build (Cargo decides the output location: CARGO_TARGET_DIR, .cargo/config, ...)
echo "build.sh: running cargo build --release --locked (this can take a few minutes the first time)"
artifacts="$tmp/artifacts.json"
if ! cargo build --release --locked --message-format=json-render-diagnostics >"$artifacts"; then
  die "cargo build --release failed; the compiler output is above. Fix the error and run ./build.sh again."
fi

# ---- 4. find the executable Cargo built, and verify it
exe_list="$(grep '"reason":"compiler-artifact"' "$artifacts" | grep '"kind":\["bin"\]' | grep -o '"executable":"[^"]*"' | sed 's/^"executable":"//; s/"$//' || true)"
count="$(printf '%s' "$exe_list" | grep -c . || true)"
[ "$count" -ge 1 ] || die "the build succeeded but Cargo reported no executable"
[ "$count" -eq 1 ] || die "the build produced $count executables; expected exactly one: $exe_list"
exe="$exe_list"
[ -f "$exe" ] && [ -x "$exe" ] || die "the build reported $exe, but that file does not exist or is not executable"

echo ""
echo "build.sh: ok: GitMesh release build finished."
echo "  executable: $exe"
echo "  to run it:  \"$exe\" --help        (build.sh does not start GitMesh)"
echo "  to install it for your user account (optional): see \"Installing for your user account\" in README.md"
