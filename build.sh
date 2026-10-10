#!/usr/bin/env bash
# Build GitMesh in release mode.
#
#   ./build.sh
#
# Takes no arguments and needs no configuration. It works from any directory: it builds the
# repository that contains this script, using the Rust environment of tools/rust-env.sh
# (the same one the tests use). It runs `cargo build --release`, checks that the executable
# exists, and prints where it is. It does NOT start GitMesh, install anything, or delete
# build output.
set -euo pipefail

die() {
  echo "build.sh: error: $*" >&2
  exit 1
}

if [ "$#" -ne 0 ]; then
  die "this script takes no arguments (got: $*). Run it as: ./build.sh"
fi

# Work from the repository root, wherever the script was launched from.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir"

[ -f Cargo.toml ] || die "Cargo.toml not found in $script_dir; is this the GitMesh repository?"
[ -f tools/rust-env.sh ] || die "tools/rust-env.sh not found in $script_dir; it provides the Rust environment"

# The executable name is the [[bin]] name in Cargo.toml, not a guess.
bin_name="$(awk '
  /^\[\[bin\]\]/ { in_bin = 1; next }
  /^\[/          { in_bin = 0 }
  in_bin && /^name[[:space:]]*=/ {
    sub(/^name[[:space:]]*=[[:space:]]*"/, "")
    sub(/"[[:space:]]*$/, "")
    print
    exit
  }' Cargo.toml)"
[[ "$bin_name" =~ ^[A-Za-z0-9_-]+$ ]] || die "could not read the [[bin]] name from Cargo.toml"

# Cargo's target directory: CARGO_TARGET_DIR if set, otherwise ./target.
target_dir="${CARGO_TARGET_DIR:-$script_dir/target}"
exe="$target_dir/release/$bin_name"

echo "build.sh: building $bin_name (release) in $script_dir"
if ! bash tools/rust-env.sh cargo build --release; then
  die "cargo build --release failed; the error is printed above. Fix it and run ./build.sh again."
fi

if [ ! -f "$exe" ] || [ ! -x "$exe" ]; then
  die "the build finished, but the executable was not found at $exe (check CARGO_TARGET_DIR)"
fi

echo "build.sh: ok: release executable is at:"
echo "  $exe"
echo "build.sh: to run it: \"$exe\" --help   (this script does not start GitMesh)"
