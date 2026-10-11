#!/usr/bin/env bash
# Install the GitMesh executable for your user account. No sudo, no network, no shell
# start-up files are edited, and no project data is touched. See docs/INSTALLATION.md.
#
#   ./install.sh                        build (./build.sh), then install to ~/.local/bin/gitmesh
#   ./install.sh --bin-dir DIR          install to DIR/gitmesh instead (DIR must be absolute)
#   ./install.sh --no-build             install the release build that already exists
#   ./install.sh --force                also replace a file at the destination that is not GitMesh
#   ./install.sh --uninstall            remove the installed GitMesh executable (nothing else)
#   ./install.sh --uninstall --bin-dir DIR
#   ./install.sh --help
#
# Identity: a file counts as "GitMesh" when it is a regular file that contains GitMesh's own
# help text. The file is searched, never executed, before it is replaced or removed.

# Re-run under bash when started with sh, dash, or similar.
if [ -z "${BASH_VERSION:-}" ]; then exec bash "$0" "$@"; fi
set -euo pipefail

# Text that appears in every GitMesh executable (from the CLI description).
readonly GM_MARKER="GitMesh lets you work in a single local project"

usage() {
  cat <<'EOF'
Usage: ./install.sh [--bin-dir DIR] [--no-build] [--force]
       ./install.sh --uninstall [--bin-dir DIR]

Installs the GitMesh executable as DIR/gitmesh (default DIR: $HOME/.local/bin).
  --bin-dir DIR   destination directory; must be an absolute path (or start with ~/)
  --no-build      do not run build.sh; install target/release/gitmesh as it is
  --force         replace an existing file at the destination that is not GitMesh
  --uninstall     remove DIR/gitmesh if it is a GitMesh executable; touches nothing else
  -h, --help      show this help
EOF
}

die() {
  echo "install.sh: error: $*" >&2
  exit 1
}

bin_dir=""
if [ -n "${HOME:-}" ]; then
  bin_dir="$HOME/.local/bin"
fi
mode=install
build=1
force=0
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir)
      [ $# -ge 2 ] || die "--bin-dir needs a directory argument"
      bin_dir="$2"
      shift
      ;;
    --bin-dir=*) bin_dir="${1#--bin-dir=}" ;;
    --no-build) build=0 ;;
    --force) force=1 ;;
    --uninstall) mode=uninstall ;;
    -h|--help) usage; exit 0 ;;
    *) echo "install.sh: error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [ "$mode" = uninstall ]; then
  [ "$force" -eq 0 ] || die "--force does not apply to --uninstall (uninstall never removes a non-GitMesh file)"
  [ "$build" -eq 1 ] || die "--no-build does not apply to --uninstall"
fi

# ---- validate the destination directory (no creation or change before this passes) ----
case "$bin_dir" in
  "~/"*) bin_dir="$HOME/${bin_dir#\~/}" ;;
esac
case "$bin_dir" in
  "") die "no destination directory: pass --bin-dir DIR (HOME is not set, so the default is unavailable)" ;;
  /*) ;;
  *) die "--bin-dir must be an absolute path (got '$bin_dir'); use e.g. --bin-dir \"\$HOME/bin\"" ;;
esac
while [ "${#bin_dir}" -gt 1 ] && [ "${bin_dir%/}" != "$bin_dir" ]; do
  bin_dir="${bin_dir%/}"
done
if [ -e "$bin_dir" ] && [ ! -d "$bin_dir" ]; then
  die "$bin_dir exists and is not a directory"
fi
dest="$bin_dir/gitmesh"

is_gitmesh_exe() {
  [ -f "$1" ] && [ ! -L "$1" ] && grep -aq -- "$GM_MARKER" "$1"
}

# ---- uninstall ----
if [ "$mode" = uninstall ]; then
  if [ ! -e "$dest" ] && [ ! -L "$dest" ]; then
    echo "nothing to remove: $dest does not exist"
    exit 0
  fi
  if [ -L "$dest" ]; then
    die "$dest is a symbolic link, so it was NOT removed"
  fi
  if ! is_gitmesh_exe "$dest"; then
    die "$dest is not a GitMesh executable, so it was NOT removed"
  fi
  rm -f -- "$dest" || die "cannot remove $dest (permission denied?)"
  echo "removed: $dest"
  echo "Not removed: the directory $bin_dir, your projects, .gitmesh/project.toml files,"
  echo "             Git repositories and their history, and any PATH line you added yourself."
  exit 0
fi

# ---- locate the release executable (build first unless --no-build) ----
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir"
[ -f Cargo.toml ] && [ -f build.sh ] || die "this is not the GitMesh source tree ($script_dir)"

if [ "$build" -eq 1 ]; then
  bash "$script_dir/build.sh" || die "the build failed; nothing was installed"
fi

target_dir="${CARGO_TARGET_DIR:-$script_dir/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$script_dir/$target_dir" ;;
esac
src="$target_dir/release/gitmesh"
[ -f "$src" ] || die "no release executable at $src; run ./build.sh first (or drop --no-build)"
is_gitmesh_exe "$src" || die "$src is not a GitMesh executable; run ./build.sh again"

# ---- check the destination before changing anything ----
# A symbolic link is never followed, replaced or treated as GitMesh, not even with --force:
# `mv` onto a link to a directory would move the new file into that directory.
if [ -L "$dest" ]; then
  die "$dest is a symbolic link, so it was NOT changed. Remove the link yourself, or choose another --bin-dir."
fi
if [ -d "$dest" ]; then
  die "$dest is a directory; choose another --bin-dir"
fi
replacing=""
if [ -e "$dest" ] || [ -L "$dest" ]; then
  if is_gitmesh_exe "$dest"; then
    replacing="gitmesh"
  elif [ "$force" -eq 1 ]; then
    replacing="other"
  else
    die "$dest already exists and is not a GitMesh executable, so it was NOT changed.
       Re-run with --force to replace it, or choose another directory with --bin-dir."
  fi
fi

# ---- create the directory (never with sudo) ----
if [ ! -d "$bin_dir" ]; then
  mkdir -p -- "$bin_dir" || die "cannot create $bin_dir; choose another --bin-dir. This installer never uses sudo."
fi
[ -w "$bin_dir" ] || die "$bin_dir is not writable by your user account; choose another --bin-dir. This installer never uses sudo."

if [ "$replacing" = gitmesh ] && cmp -s -- "$src" "$dest"; then
  echo "already up to date: $dest"
else
  # Copy beside the destination, then rename over it: the switch is atomic and a failed
  # copy never leaves a half-written executable behind.
  tmp="$(mktemp "$bin_dir/.gitmesh-install.XXXXXX")" || die "cannot create a temporary file in $bin_dir"
  trap 'rm -f -- "$tmp"' EXIT
  cp -- "$src" "$tmp" || die "cannot copy the executable into $bin_dir"
  chmod 0755 -- "$tmp"
  mv -f -- "$tmp" "$dest" || die "cannot write $dest"
  trap - EXIT
  case "$replacing" in
    gitmesh) echo "updated: $dest" ;;
    other) echo "replaced the non-GitMesh file at $dest (--force)" ;;
    *) echo "installed: $dest" ;;
  esac
fi

# ---- verify the installed copy by running it ----
version_line="$("$dest" --version 2>/dev/null || true)"
case "$version_line" in
  gitmesh\ *) echo "version: $version_line" ;;
  *) die "the installed file did not report a GitMesh version; check $dest" ;;
esac

case ":$PATH:" in
  *":$bin_dir:"*)
    echo "ready: run 'gitmesh --help' from any directory."
    ;;
  *)
    echo ""
    echo "$bin_dir is not in your PATH, so 'gitmesh' will not be found yet."
    echo "For this shell only:       export PATH=\"$bin_dir:\$PATH\""
    echo "To make it permanent, add that line to your shell start-up file (for example ~/.bashrc"
    echo "or ~/.zshrc), or for fish run: fish_add_path $bin_dir"
    echo "This installer does not edit shell files. See docs/INSTALLATION.md (PATH troubleshooting)."
    ;;
esac
