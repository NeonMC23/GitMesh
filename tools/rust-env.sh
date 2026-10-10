#!/usr/bin/env bash
# Run a command with the Rust toolchain that build.sh would use, for development and tests.
#
#   ./tools/rust-env.sh cargo test
#   ./tools/rust-env.sh cargo clippy --all-targets -- -D warnings
#
# It never installs Rust. If no working toolchain is found it says why and exits 1; run
# ./build.sh (which can install Rust with your consent) to set one up. Uses only the user's own
# PATH, CARGO_HOME and RUSTUP_HOME. Nothing here refers to /usr/local or to any sandbox.
if [ -z "${BASH_VERSION:-}" ]; then exec bash "$0" "$@"; fi
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: tools/rust-env.sh COMMAND [ARGS...]" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091  # sourced via $root at run time
. "$root/tools/toolchain.sh"

min_rust="$(gm_min_rust "$root")"
status=0
gm_discover "$root" "$min_rust" || status=$?
if [ "$status" -ne 0 ]; then
  if [ "$status" -eq 1 ]; then
    gm_error "no Rust toolchain was found (need Rust $min_rust or newer)."
  else
    gm_error "the Rust toolchain found on this system cannot be used: $GM_REASON"
  fi
  gm_note "Run ./build.sh to set up or diagnose the toolchain."
  exit 1
fi

exec "$@"
