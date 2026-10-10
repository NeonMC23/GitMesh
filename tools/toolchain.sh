# shellcheck shell=bash
# shellcheck disable=SC2034  # GM_* variables are read by the scripts that source this file
# Shared toolchain discovery for GitMesh's shell entry points. This file is SOURCED, not run.
#
#   . tools/toolchain.sh
#
# It never installs anything by itself. Installation is an explicit step: gm_bootstrap_rustup,
# which build.sh calls only after the user has agreed (see build.sh). It never uses /usr/local,
# never uses sudo, never edits shell start-up files, and never overwrites an existing rustup
# installation. Requires bash.

# ---------------------------------------------------------------- messages

gm_error() { echo "error: $*" >&2; }
gm_note() { echo "$*" >&2; }

# ---------------------------------------------------------------- versions

# gm_version_ge HAVE NEED: succeeds when HAVE >= NEED. Both are "MAJOR.MINOR[.PATCH]".
gm_version_ge() {
  local have="$1" need="$2" h1 h2 n1 n2
  [[ "$have" =~ ^[0-9]+\.[0-9]+ ]] || return 1
  [[ "$need" =~ ^[0-9]+\.[0-9]+$ ]] || return 1
  IFS=. read -r h1 h2 _ <<<"$have"
  IFS=. read -r n1 n2 <<<"$need"
  if [ "$h1" -ne "$n1" ]; then [ "$h1" -gt "$n1" ]; else [ "$h2" -ge "$n2" ]; fi
}

# gm_min_rust PROJECT_ROOT: the minimum Rust version declared by Cargo.toml (rust-version).
gm_min_rust() {
  sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([0-9.]*\)".*/\1/p' "$1/Cargo.toml" | head -n 1
}

# ---------------------------------------------------------------- probing

# gm_probe DIR MIN: check the cargo and rustc that DIR (or PATH, when DIR is empty) provides.
# Sets GM_CARGO_VERSION, GM_RUSTC_VERSION and GM_REASON. Returns:
#   0 usable, 1 nothing installed there, 2 present but unusable (GM_REASON says why).
gm_probe() {
  local dir="$1" min="$2" cargo rustc out
  GM_REASON=""
  if [ -n "$dir" ]; then
    cargo="$dir/cargo"; rustc="$dir/rustc"
    [ -x "$cargo" ] || cargo=""
    [ -x "$rustc" ] || rustc=""
  else
    cargo="$(command -v cargo 2>/dev/null || true)"
    rustc="$(command -v rustc 2>/dev/null || true)"
  fi
  if [ -z "$cargo" ] && [ -z "$rustc" ]; then
    return 1
  fi
  if [ -z "$cargo" ]; then
    GM_REASON="rustc was found at $rustc, but cargo was not found next to it"
    return 2
  fi
  if [ -z "$rustc" ]; then
    GM_REASON="cargo was found at $cargo, but rustc was not found next to it"
    return 2
  fi

  # Presence is not enough: both must actually run and report a version.
  if ! out="$("$rustc" --version 2>&1)"; then
    GM_REASON="rustc at $rustc does not run: $(printf '%s' "$out" | head -n 1)"
    return 2
  fi
  if [[ "$out" =~ ^rustc\ ([0-9]+\.[0-9]+(\.[0-9]+)?) ]]; then
    GM_RUSTC_VERSION="${BASH_REMATCH[1]}"
  else
    GM_REASON="rustc at $rustc reported an unexpected version line: $out"
    return 2
  fi
  if ! out="$("$cargo" --version 2>&1)"; then
    GM_REASON="cargo at $cargo does not run: $(printf '%s' "$out" | head -n 1)"
    return 2
  fi
  if [[ "$out" =~ ^cargo\ ([0-9]+\.[0-9]+(\.[0-9]+)?) ]]; then
    GM_CARGO_VERSION="${BASH_REMATCH[1]}"
  else
    GM_REASON="cargo at $cargo reported an unexpected version line: $out"
    return 2
  fi
  GM_CARGO_PATH="$cargo"
  GM_RUSTC_PATH="$rustc"

  if [ -n "$min" ] && ! gm_version_ge "$GM_RUSTC_VERSION" "$min"; then
    GM_REASON="rustc $GM_RUSTC_VERSION at $rustc is older than the minimum $min required by Cargo.toml"
    return 2
  fi
  return 0
}

# gm_discover PROJECT_ROOT MIN: find a usable toolchain, in this order:
#   1. cargo and rustc already on PATH (distribution packages, rustup, Homebrew, ...)
#   2. the rustup bin directory: ${CARGO_HOME:-$HOME/.cargo}/bin
# Returns 0 when usable (GM_SOURCE says where; PATH is extended for step 2), 1 when nothing is
# installed, 2 when something is installed but unusable (GM_REASON says why).
gm_discover() {
  local min="$2" cargo_home bin status=1 first_reason=""
  cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  bin="$cargo_home/bin"
  GM_RUSTUP_PRESENT=0
  [ -e "$bin/rustup" ] && GM_RUSTUP_PRESENT=1
  [ -e "${RUSTUP_HOME:-$HOME/.rustup}" ] && GM_RUSTUP_PRESENT=1

  gm_probe "" "$min"; status=$?
  if [ "$status" -eq 0 ]; then
    GM_SOURCE="PATH ($(dirname "$GM_CARGO_PATH"))"
    return 0
  fi
  [ "$status" -eq 2 ] && first_reason="$GM_REASON"

  gm_probe "$bin" "$min"; status=$?
  if [ "$status" -eq 0 ]; then
    GM_SOURCE="$bin (rustup bin directory)"
    PATH="$bin:$PATH"; export PATH
    return 0
  fi
  if [ "$status" -eq 2 ]; then
    GM_REASON="${first_reason:+$first_reason; }$GM_REASON"
    return 2
  fi
  if [ -n "$first_reason" ]; then
    GM_REASON="$first_reason"
    return 2
  fi
  GM_REASON=""
  return 1
}

# ---------------------------------------------------------------- native prerequisites

# gm_linker_present: rustc links through the system C compiler driver, `cc`.
gm_linker_present() {
  command -v cc >/dev/null 2>&1
}

# gm_native_hint [OS_RELEASE_FILE]: the exact, OS-specific package to install for a C linker.
# Only prints advice; it never runs a package manager.
gm_native_hint() {
  local file="${1:-/etc/os-release}" id="" like="" kernel
  kernel="$(uname -s 2>/dev/null || echo unknown)"
  if [ "$kernel" = "Darwin" ]; then
    echo "  install the Xcode Command Line Tools:  xcode-select --install"
    return 0
  fi
  if [ -r "$file" ]; then
    id="$(sed -n 's/^ID=//p' "$file" | head -n 1 | tr -d '"')"
    like="$(sed -n 's/^ID_LIKE=//p' "$file" | head -n 1 | tr -d '"')"
  fi
  case " $id $like " in
    *" fedora "*|*" nobara "*|*" rhel "*|*" centos "*)
      echo "  install the compiler tools:  sudo dnf install gcc" ;;
    *" debian "*|*" ubuntu "*)
      echo "  install the compiler tools:  sudo apt install build-essential" ;;
    *" arch "*)
      echo "  install the compiler tools:  sudo pacman -S base-devel" ;;
    *" suse "*|*" opensuse "*)
      echo "  install the compiler tools:  sudo zypper install gcc" ;;
    *)
      echo "  install a C compiler/linker that provides 'cc' with your system's package manager" ;;
  esac
}

# ---------------------------------------------------------------- download and bootstrap

# gm_platform_triple: the rustup-init target triple for this machine, or nothing if unsupported.
gm_platform_triple() {
  local kernel machine
  kernel="$(uname -s 2>/dev/null)"
  machine="$(uname -m 2>/dev/null)"
  case "$kernel/$machine" in
    Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin/x86_64) echo "x86_64-apple-darwin" ;;
    Darwin/arm64) echo "aarch64-apple-darwin" ;;
    *) return 1 ;;
  esac
}

# gm_fetch URL OUTFILE: download over HTTPS only, with curl or wget.
gm_fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    return 127
  fi
}

# gm_sha256 FILE: print the SHA-256 digest of FILE.
gm_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 127
  fi
}

# gm_bootstrap_rustup TMPDIR: install stable Rust with the official rustup-init, into the
# user's own CARGO_HOME and RUSTUP_HOME. Refuses to touch an existing rustup installation.
# Verifies rustup-init against the SHA-256 published next to it. Sets GM_REASON on failure.
gm_bootstrap_rustup() {
  local tmp="$1" triple base expected actual cargo_home
  cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  GM_REASON=""

  if [ "${GM_RUSTUP_PRESENT:-0}" -eq 1 ]; then
    GM_REASON="a rustup installation already exists ($cargo_home or ${RUSTUP_HOME:-$HOME/.rustup}) but it does not provide a working toolchain; it was NOT overwritten. Repair it with: rustup default stable  (or remove it yourself, then run again)"
    return 2
  fi
  if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    GM_REASON="neither curl nor wget is installed, so rustup cannot be downloaded"
    return 2
  fi
  if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
    GM_REASON="neither sha256sum nor shasum is installed, so the download cannot be verified"
    return 2
  fi
  if ! triple="$(gm_platform_triple)"; then
    GM_REASON="automatic installation is not available for this platform ($(uname -s) $(uname -m)); install Rust yourself from https://rustup.rs or your package manager"
    return 2
  fi

  base="https://static.rust-lang.org/rustup/dist/$triple"
  gm_note "downloading rustup-init from $base ..."
  if ! gm_fetch "$base/rustup-init" "$tmp/rustup-init"; then
    GM_REASON="could not download rustup-init from $base. Check your network connection and proxy settings (https_proxy), then run again"
    return 2
  fi
  if ! gm_fetch "$base/rustup-init.sha256" "$tmp/rustup-init.sha256"; then
    GM_REASON="could not download the checksum for rustup-init from $base. Check your network connection, then run again"
    return 2
  fi
  expected="$(awk '{print $1}' "$tmp/rustup-init.sha256" | head -n 1)"
  actual="$(gm_sha256 "$tmp/rustup-init")"
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    GM_REASON="the downloaded rustup-init does not match its published SHA-256 checksum; it was not run"
    return 2
  fi
  chmod +x "$tmp/rustup-init"

  gm_note "installing Rust (stable, minimal profile) into $cargo_home and ${RUSTUP_HOME:-$HOME/.rustup} ..."
  # --no-modify-path: do not edit shell start-up files. Runs as the current user, never as root.
  if ! "$tmp/rustup-init" -y --profile minimal --default-toolchain stable --no-modify-path; then
    GM_REASON="rustup-init failed; see the messages above"
    return 2
  fi
  return 0
}
