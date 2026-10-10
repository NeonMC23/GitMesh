#!/usr/bin/env bash
# shellcheck disable=SC2319  # "$?" after a [ ... ] test is the test's status by design.
# Offline, deterministic checks for build.sh and tools/toolchain.sh.
#
#   ./tools/build-script-check.sh
#
# No real Rust, no network and no real Cargo build are used. Every case runs build.sh in a
# temporary repository with a scrubbed environment (env -i) and a PATH that contains only
# symlinks to basic utilities plus FAKE cargo, rustc, rustup-init, curl, cc, uname and sudo.
# The fakes record every call in a log, so the checks can assert what the script did and,
# just as importantly, what it did NOT do (download, install, or call sudo).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/gitmesh-build-check.XXXXXX")"
trap 'rm -rf "$work"' EXIT

pass=0
fail=0
skip=0
check() { # check NAME STATUS [DETAIL]: STATUS 0 = pass; a missing STATUS counts as a failure
  if [ "${2:-1}" -eq 0 ]; then
    pass=$((pass + 1)); echo "PASS $1"
  else
    fail=$((fail + 1)); echo "FAIL $1${3:+ -- $3}"
  fi
}
note_skip() { skip=$((skip + 1)); echo "SKIP $1 -- $2"; }

# ------------------------------------------------------------------ fake tools (templates)
tpl="$work/templates"
mkdir -p "$tpl"

# A fake Rust tool. Its behaviour is read from files next to it, so each copy can differ:
# .fake-version (e.g. 1.99.0) and .fake-mode (ok | broken).
cat > "$tpl/cargo" <<'FAKE'
#!/bin/sh
dir="$(dirname "$0")"
echo "cargo[$dir] $*" >> "$FAKE_LOG"
case "$1" in
  --version)
    if [ "$(cat "$dir/.fake-mode" 2>/dev/null)" = broken ]; then
      echo "cargo: error while loading shared libraries: libfake.so" >&2; exit 127
    fi
    echo "cargo $(cat "$dir/.fake-version") (fake)"; exit 0 ;;
  build)
    mode="${FAKE_BUILD:-ok}"
    [ "$mode" = fail ] && { echo "error[E0308]: fake compile error" >&2; exit 101; }
    tdir="${CARGO_TARGET_DIR:-$PWD/target}/release"
    mkdir -p "$tdir" || exit 1
    bin="${FAKE_BIN:-gitmesh}"
    exe="$tdir/$bin"
    if [ "$mode" != noexe ]; then
      printf '#!/bin/sh\necho ran >> "%s"\n' "$FAKE_LOG.ran" > "$exe"
      chmod +x "$exe"
    fi
    [ "$mode" = none ] && exit 0
    printf '{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"%s"},"executable":"%s","fresh":false}\n' "$bin" "$exe"
    if [ "$mode" = multi ]; then
      printf 'extra\n' > "$tdir/other"; chmod +x "$tdir/other"
      printf '{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"other"},"executable":"%s/other","fresh":false}\n' "$tdir"
    fi
    exit 0 ;;
esac
exit 0
FAKE
cat > "$tpl/rustc" <<'FAKE'
#!/bin/sh
dir="$(dirname "$0")"
echo "rustc[$dir] $*" >> "$FAKE_LOG"
if [ "$1" = --version ]; then
  if [ "$(cat "$dir/.fake-mode" 2>/dev/null)" = broken ]; then
    echo "rustc: error: failed to load sysroot" >&2; exit 1
  fi
  echo "rustc $(cat "$dir/.fake-version") (b940084d7 2026-09-28)"; exit 0
fi
exit 0
FAKE
cat > "$tpl/rustup" <<'FAKE'
#!/bin/sh
echo "rustup[$(dirname "$0")] $*" >> "$FAKE_LOG"; exit 0
FAKE
# The fake rustup-init installs a fake toolchain under CARGO_HOME / RUSTUP_HOME, like the real one.
cat > "$tpl/rustup-init" <<'FAKE'
#!/bin/sh
echo "rustup-init $*" >> "$FAKE_LOG"
[ "${FAKE_INIT:-ok}" = fail ] && { echo "fake rustup-init: failure" >&2; exit 1; }
home="${CARGO_HOME:-$HOME/.cargo}"
mkdir -p "$home/bin" "${RUSTUP_HOME:-$HOME/.rustup}" || exit 1
if [ "${FAKE_INIT:-ok}" = ok ]; then
  for t in cargo rustc rustup; do cp "$FAKE_TEMPLATES/$t" "$home/bin/$t"; chmod +x "$home/bin/$t"; done
  echo 1.99.0 > "$home/bin/.fake-version"; echo ok > "$home/bin/.fake-mode"
fi
echo "installed (fake)"; exit 0
FAKE
cat > "$tpl/curl" <<'FAKE'
#!/bin/sh
out=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
    --proto|--retry) shift ;;
    -*) ;;
    *) url="$1" ;;
  esac
  shift
done
echo "curl $url" >> "$FAKE_LOG"
[ "${FAKE_CURL:-ok}" = fail ] && { echo "curl: (6) Could not resolve host" >&2; exit 6; }
case "$url" in
  */rustup-init.sha256)
    if [ "${FAKE_CURL:-ok}" = badsum ]; then
      echo "0000000000000000000000000000000000000000000000000000000000000000 *./rustup-init" > "$out"
    else
      echo "$(sha256sum "$FAKE_TEMPLATES/rustup-init" | awk '{print $1}') *./rustup-init" > "$out"
    fi ;;
  */rustup-init)
    if [ "${FAKE_CURL:-ok}" = badsum ]; then echo "tampered" > "$out"; else cp "$FAKE_TEMPLATES/rustup-init" "$out"; fi ;;
  *) exit 22 ;;
esac
exit 0
FAKE
cat > "$tpl/cc" <<'FAKE'
#!/bin/sh
echo "cc $*" >> "$FAKE_LOG"; exit 0
FAKE
cat > "$tpl/sudo" <<'FAKE'
#!/bin/sh
echo "sudo $*" >> "$FAKE_LOG"; exit 99
FAKE
cat > "$tpl/uname" <<'FAKE'
#!/bin/sh
case "$1" in
  -m) echo "${FAKE_UNAME_M:-x86_64}" ;;
  *) echo "${FAKE_UNAME_S:-Linux}" ;;
esac
FAKE
chmod +x "$tpl"/*

# ------------------------------------------------------------------ environments
# new_env NAME: a fresh project copy plus a minimal PATH. Prints the environment directory.
new_env() {
  local e="$work/$1"
  mkdir -p "$e/bin" "$e/home" "$e/tmp" "$e/work tree/GitMesh/tools"
  cp "$here/build.sh" "$e/work tree/GitMesh/build.sh"
  cp "$here/tools/toolchain.sh" "$e/work tree/GitMesh/tools/toolchain.sh"
  cp "$here/tools/rust-env.sh" "$e/work tree/GitMesh/tools/rust-env.sh"
  cp "$here/Cargo.toml" "$e/work tree/GitMesh/Cargo.toml"
  # Basic utilities only: nothing that could be a real cargo or rustc.
  local t p
  for t in bash sh awk sed grep cat cp mv rm mkdir mktemp dirname basename chmod head tr cut sort wc ls tail env sha256sum shasum python3; do
    p="$(command -v "$t" 2>/dev/null)" && [ -n "$p" ] && ln -sf "$p" "$e/bin/$t"
  done
  for t in cargo rustc rustup cc sudo uname curl; do
    cp "$tpl/$t" "$e/bin/$t"
  done
  echo 1.99.0 > "$e/bin/.fake-version"; echo ok > "$e/bin/.fake-mode"
  : > "$e/log"
  printf '%s\n' "$e"
}

# run ENV CWD [VAR=VALUE ...] -- ARGS...   Sets RC and OUT. Scrubbed environment.
run() {
  local e="$1" cwd="$2"; shift 2
  local vars=() args=()
  while [ "$#" -gt 0 ]; do
    if [[ "$1" =~ ^[A-Z][A-Z0-9_]*= ]]; then vars+=("$1"); else args+=("$1"); fi
    shift
  done
  OUT="$(cd "$cwd" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" \
      FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" "${vars[@]}" \
      bash "$e/work tree/GitMesh/build.sh" "${args[@]}" 2>&1 < /dev/null)"
  RC=$?
}
has() { grep -qF -- "$2" "$1"; }                      # has FILE TEXT
logged() { grep -qF -- "$2" "$1/log"; }               # logged ENV TEXT
notlogged() { ! grep -qF -- "$2" "$1/log"; }

# ------------------------------------------------------------------ 0. static checks
for f in build.sh tools/toolchain.sh tools/rust-env.sh tools/build-script-check.sh examples/demo.sh; do
  bash -n "$here/$f"; check "bash -n $f" $?
done
[ -x "$here/build.sh" ] && [ -x "$here/tools/rust-env.sh" ] && [ -x "$here/tools/build-script-check.sh" ]
check "build.sh, rust-env.sh and this check are executable" $?
! grep -v '^[[:space:]]*#' "$here/build.sh" "$here/tools/toolchain.sh" "$here/tools/rust-env.sh" | grep -q '/usr/local'
check "no executable line refers to /usr/local (comments excluded)" $?
! grep -nE '^[[:space:]]*sudo[[:space:]]' "$here/build.sh" "$here/tools/toolchain.sh" "$here/tools/rust-env.sh" >/dev/null
check "no line runs sudo" $?
msrv_cargo="$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([0-9.]*\)".*/\1/p' "$here/Cargo.toml")"
grep -qF "Rust $msrv_cargo or newer" "$here/README.md"
check "README states the same minimum Rust as Cargo.toml (rust-version $msrv_cargo)" $?

# ------------------------------------------------------------------ 1. library unit checks
(
  set -u
  . "$here/tools/toolchain.sh"
  bad=0
  while read -r have need want; do
    if gm_version_ge "$have" "$need"; then got=0; else got=1; fi
    [ "$got" = "$want" ] || { echo "  gm_version_ge $have $need -> $got (want $want)"; bad=1; }
  done <<'TABLE'
1.99.0 1.88 0
1.88.0 1.88 0
1.87.9 1.88 1
1.74 1.88 1
2.0.0 1.88 0
abc 1.88 1
1.88 xyz 1
TABLE
  exit $bad
)
check "version comparison table (incl. 1.87 < 1.88, 2.0 >= 1.88, garbage rejected)" $?

(
  . "$here/tools/toolchain.sh"
  bad=0
  mkdir -p "$work/osr"
  printf 'NAME="Nobara Linux"\nID=nobara\nID_LIKE="fedora"\n' > "$work/osr/nobara"
  printf 'ID=debian\nID_LIKE=""\n' > "$work/osr/debian"
  printf 'ID=ubuntu\nID_LIKE=debian\n' > "$work/osr/ubuntu"
  printf 'ID=arch\n' > "$work/osr/arch"
  printf 'ID=gentoo\n' > "$work/osr/gentoo"
  gm_native_hint "$work/osr/nobara" | grep -q 'sudo dnf install gcc' || bad=1
  gm_native_hint "$work/osr/debian" | grep -q 'sudo apt install build-essential' || bad=1
  gm_native_hint "$work/osr/ubuntu" | grep -q 'sudo apt install build-essential' || bad=1
  gm_native_hint "$work/osr/arch" | grep -q 'sudo pacman -S base-devel' || bad=1
  gm_native_hint "$work/osr/gentoo" | grep -q 'your system' || bad=1
  exit $bad
)
check "native hints per distribution (Nobara/Fedora, Debian, Ubuntu, Arch, unknown)" $?

(
  . "$here/tools/toolchain.sh"
  PATH="$work/templates:$PATH"
  bad=0
    [ "$(FAKE_UNAME_S=Linux FAKE_UNAME_M=x86_64 gm_platform_triple)" = x86_64-unknown-linux-gnu ] || bad=1
  [ "$(FAKE_UNAME_S=Linux FAKE_UNAME_M=aarch64 gm_platform_triple)" = aarch64-unknown-linux-gnu ] || bad=1
  [ "$(FAKE_UNAME_S=Darwin FAKE_UNAME_M=arm64 gm_platform_triple)" = aarch64-apple-darwin ] || bad=1
  [ "$(FAKE_UNAME_S=Darwin FAKE_UNAME_M=x86_64 gm_platform_triple)" = x86_64-apple-darwin ] || bad=1
  FAKE_UNAME_S=Linux FAKE_UNAME_M=riscv64 gm_platform_triple >/dev/null 2>&1 && bad=1
  exit $bad
)
check "platform triples: Linux x86_64/aarch64, macOS arm64/x86_64; others refused" $?

# ------------------------------------------------------------------ 2. toolchain discovery
# A system cargo/rustc on PATH is used as is: no download, no install.
e="$(new_env system-cargo)"
run "$e" "$e"
check "existing system Rust on PATH: build succeeds (rc 0)" "$RC" "$OUT"
printf '%s' "$OUT" | grep -q 'from PATH'; check "existing system Rust on PATH: reports the source" $?
notlogged "$e" "curl" && notlogged "$e" "rustup-init" && [ ! -e "$e/home/.cargo" ]
check "existing system Rust on PATH: nothing downloaded or installed" $?

e="$(new_env system-cargo-consent)"
run "$e" "$e" --install-rust
check "--install-rust does not install when a working Rust exists" "$([ "$RC" -eq 0 ] && notlogged "$e" "curl" && echo 0 || echo 1)" "$OUT"

# rustup-managed toolchain: cargo is NOT on PATH, but is in CARGO_HOME/bin.
e="$(new_env rustup-managed)"
mkdir -p "$e/home/.cargo/bin" && cp "$tpl/cargo" "$tpl/rustc" "$tpl/rustup" "$e/home/.cargo/bin/" && echo 1.99.0 > "$e/home/.cargo/bin/.fake-version" && echo ok > "$e/home/.cargo/bin/.fake-mode"
rm "$e/bin/cargo" "$e/bin/rustc"
run "$e" "$e"
check "rustup-managed toolchain in ~/.cargo/bin (not on PATH) is used" "$RC" "$OUT"
printf '%s' "$OUT" | grep -q 'rustup bin directory'; check "rustup-managed toolchain: reports ~/.cargo/bin as the source" $?
notlogged "$e" "curl" ; check "rustup-managed toolchain: no download" $?

# Preconfigured CARGO_HOME with a path containing spaces, plus RUSTUP_HOME respected.
e="$(new_env custom-cargo-home)"
mkdir -p "$e/custom cargo home/bin" && cp "$tpl/cargo" "$tpl/rustc" "$tpl/rustup" "$e/custom cargo home/bin/" && echo 1.99.0 > "$e/custom cargo home/bin/.fake-version" && echo ok > "$e/custom cargo home/bin/.fake-mode"
rm "$e/bin/cargo" "$e/bin/rustc"
run "$e" "$e" CARGO_HOME="$e/custom cargo home" RUSTUP_HOME="$e/custom rustup home"
check "preconfigured CARGO_HOME (with spaces) is honoured" "$RC" "$OUT"
[ ! -e "$e/home/.cargo" ] && [ ! -e "$e/home/.rustup" ]; check "preconfigured CARGO_HOME: nothing created in the default home" $?

# A valid toolchain in CARGO_HOME is preferred over an old one on PATH; the old one is never used.
e="$(new_env old-path-good-home)"
echo 1.60.0 > "$e/bin/.fake-version"
mkdir -p "$e/home/.cargo/bin" && cp "$tpl/cargo" "$tpl/rustc" "$tpl/rustup" "$e/home/.cargo/bin/" && echo 1.99.0 > "$e/home/.cargo/bin/.fake-version" && echo ok > "$e/home/.cargo/bin/.fake-mode"
run "$e" "$e"
check "an old toolchain on PATH falls back to a valid rustup toolchain" "$RC" "$OUT"
notlogged "$e" "cargo[$e/bin] build"; check "the old PATH toolchain is never used for the build" $?

# Missing Cargo and rustc: non-interactive run refuses, installs nothing, downloads nothing.
e="$(new_env none-noninteractive)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e"
check "no Rust, non-interactive: exits 1" "$([ "$RC" -eq 1 ] && echo 0 || echo 1)" "$OUT"
printf '%s' "$OUT" | grep -q -- "--install-rust"; check "no Rust, non-interactive: names the --install-rust option" $?
notlogged "$e" "curl" && [ ! -e "$e/home/.cargo" ]; check "no Rust, non-interactive: no download, nothing installed" $?

# Cargo present, rustc missing: reason is reported, and nothing is installed over it.
e="$(new_env cargo-no-rustc)"
rm "$e/bin/rustc"
run "$e" "$e"
printf '%s' "$OUT" | grep -q "rustc was not found"; check "cargo without rustc: reports the missing rustc" $?
notlogged "$e" "curl"; check "cargo without rustc: no download" $?

# rustc present but broken, and cargo broken: "present" is not "working".
e="$(new_env rustc-broken)"; printf '#!/bin/sh\necho "rustc: error: failed to load sysroot" >&2; exit 1\n' > "$e/bin/rustc"; chmod +x "$e/bin/rustc"
run "$e" "$e"
check "rustc that does not run is reported as unusable (rc 1)" "$(printf '%s' "$OUT" | grep -q "does not run" && ([ "$RC" -eq 1 ] && printf '%s' "$OUT" | grep -q "does not run" && echo 0 || echo 1))"
notlogged "$e" "curl"; check "rustc that does not run: no download, no overwrite" $?
e="$(new_env cargo-broken)"; printf '#!/bin/sh\necho "cargo: error while loading shared libraries" >&2; exit 127\n' > "$e/bin/cargo"; chmod +x "$e/bin/cargo"; 
run "$e" "$e"
check "cargo that does not run is reported as unusable (rc 1)" "$(printf '%s' "$OUT" | grep -q "cargo at" && ([ "$RC" -eq 1 ] && printf '%s' "$OUT" | grep -q "cargo at" && echo 0 || echo 1))"

# Incompatible version: refused with the reason and a recovery path, never silently bootstrapped.
e="$(new_env too-old)"; echo 1.60.0 > "$e/bin/.fake-version"
run "$e" "$e" --install-rust
check "Rust older than Cargo.toml rust-version: refused (rc 1)" "$([ "$RC" -eq 1 ] && echo 0 || echo 1)" "$OUT"
printf '%s' "$OUT" | grep -q "older than the minimum $msrv_cargo"; check "incompatible Rust: states the minimum version" $?
printf '%s' "$OUT" | grep -q "will not overwrite"; check "incompatible Rust: says it will not overwrite the toolchain" $?
notlogged "$e" "curl"; check "incompatible Rust: never bootstraps a replacement, even with --install-rust" $?

# A rustup directory that exists but has no working toolchain is never overwritten.
e="$(new_env rustup-broken)"
rm "$e/bin/cargo" "$e/bin/rustc"; mkdir -p "$e/home/.cargo/bin"; cp "$tpl/rustup" "$e/home/.cargo/bin/rustup"
run "$e" "$e" --install-rust
check "existing broken rustup: refused, reported as NOT overwritten" "$(printf '%s' "$OUT" | grep -q "NOT overwritten" && ([ "$RC" -eq 1 ] && printf '%s' "$OUT" | grep -q "NOT overwritten" && echo 0 || echo 1))"
notlogged "$e" "rustup-init"; check "existing broken rustup: rustup-init never run" $?

# The library refuses to overwrite an existing rustup installation even when called directly.
e="$(new_env lib-no-overwrite)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"; mkdir -p "$e/home/.cargo/bin"; cp "$tpl/rustup" "$e/home/.cargo/bin/rustup"
# shellcheck disable=SC2016  # the single-quoted body is meant to be expanded by the child bash
OUT="$(cd "$e" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" TOOLCHAIN_SH="$here/tools/toolchain.sh" bash -c '
  . "$TOOLCHAIN_SH"
  GM_RUSTUP_PRESENT=1
  gm_bootstrap_rustup "$TMPDIR"; echo "rc=$?"; echo "$GM_REASON"' 2>&1 < /dev/null)"
printf '%s' "$OUT" | grep -qF "rc=2" && printf '%s' "$OUT" | grep -qF "NOT overwritten"; check "library guard: gm_bootstrap_rustup refuses to overwrite an existing rustup (rc 2)" $?
notlogged "$e" "rustup-init"; check "library guard: no rustup-init run and no download when rustup exists" $?

# ------------------------------------------------------------------ 3. bootstrap (fake network)
e="$(new_env bootstrap-ok)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e" --install-rust
check "--install-rust with no Rust: installs and builds (rc 0)" "$RC" "$OUT"
logged "$e" "rustup-init -y --profile minimal --default-toolchain stable --no-modify-path"
check "rustup-init runs non-interactively, minimal profile, stable, no shell edits" $?
[ -x "$e/home/.cargo/bin/cargo" ] && [ -e "$e/home/.rustup" ]; check "the toolchain is installed under the user's home (~/.cargo, ~/.rustup)" $?
! logged "$e" "/usr/local" && ! logged "$e" "sudo"; check "bootstrap never uses /usr/local or sudo" $?
logged "$e" "curl https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init.sha256"
check "the checksum is fetched from the official endpoint over HTTPS" $?

e="$(new_env no-network)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e" FAKE_CURL=fail --install-rust
check "failed download: clear message (rc 1)" "$(printf '%s' "$OUT" | grep -q "could not download rustup-init" && ([ "$RC" -eq 1 ] && printf '%s' "$OUT" | grep -q "could not download" && echo 0 || echo 1))"
notlogged "$e" "rustup-init -y"; check "failed download: nothing executed" $?

e="$(new_env bad-checksum)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e" FAKE_CURL=badsum --install-rust
check "checksum mismatch: refused with a clear message (rc 1)" "$(printf '%s' "$OUT" | grep -q "does not match its published SHA-256" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"
notlogged "$e" "rustup-init -y"; check "checksum mismatch: the tampered rustup-init is never run" $?

e="$(new_env bootstrap-fails)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e" FAKE_INIT=fail --install-rust
check "failed bootstrap: clear message (rc 1)" "$(printf '%s' "$OUT" | grep -q "rustup-init failed" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"

e="$(new_env no-curl-no-wget)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup" "$e/bin/curl"
run "$e" "$e" --install-rust
check "no curl and no wget: clear message (rc 1)" "$(printf '%s' "$OUT" | grep -q "neither curl nor wget" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"

e="$(new_env unsupported-arch)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e" FAKE_UNAME_M=riscv64 --install-rust
check "unsupported platform: no download, clear message (rc 1)" "$(printf '%s' "$OUT" | grep -q "not available for this platform" && ([ "$RC" -eq 1 ] && notlogged "$e" "curl" && echo 0 || echo 1))"

# Installed but still not working (rustup-init reported success without a toolchain).
e="$(new_env installed-but-broken)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
run "$e" "$e" FAKE_INIT=empty --install-rust
check "installed but not working: reported, not claimed as success (rc 1)" "$(printf '%s' "$OUT" | grep -q "still does not work" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"

# Interactive consent, on a real pseudo-terminal (python3 is needed only for this check).
pty_run() { # pty_run ENV ANSWER: sets PTY_OUT and PTY_RC
  local e="$1" answer="$2"
  PTY_OUT="$(cd "$e/work tree/GitMesh" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" \
     FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" PTY_ANSWER="$answer" python3 - "$e" <<'PY'
import os, subprocess, sys, select, time
answer = os.environ["PTY_ANSWER"].encode() + b"\n"
master, slave = os.openpty()
p = subprocess.Popen(["bash", "build.sh"], stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
buf, sent, deadline = b"", False, time.time() + 60
while time.time() < deadline:
    r, _, _ = select.select([master], [], [], 0.2)
    if r:
        try:
            data = os.read(master, 4096)
        except OSError:
            break
        if not data:
            break
        buf += data
        if not sent and b"Install Rust now?" in buf:
            os.write(master, answer); sent = True
    elif p.poll() is not None:
        break
rc = p.wait(timeout=30)
sys.stdout.write(buf.decode(errors="replace").replace("\r", ""))
sys.stdout.write("\nPTY_RC=%d\n" % rc)
PY
)"
  PTY_RC="$(printf '%s' "$PTY_OUT" | sed -n 's/^PTY_RC=//p')"
}
if command -v python3 >/dev/null 2>&1; then
  e="$(new_env pty-cancel)"
  rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
  pty_run "$e" n
  check "interactive: answering no cancels, nothing changed" "$(printf '%s' "$PTY_OUT" | grep -q "installation cancelled" && ([ "$PTY_RC" = 1 ] && notlogged "$e" "curl" && [ ! -e "$e/home/.cargo" ] && echo 0 || echo 1))" "$PTY_OUT"
  e="$(new_env pty-yes)"
  rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
  pty_run "$e" y
  check "interactive: answering yes installs and builds (rc 0)" "$(printf '%s' "$PTY_OUT" | grep -q "build.sh: ok: GitMesh release build finished" && ([ "$PTY_RC" = 0 ] && echo 0 || echo 1))" "$PTY_OUT"
  e="$(new_env pty-default-enter)"
  rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
  pty_run "$e" ""
  check "interactive: pressing Enter (default No) cancels" "$(printf '%s' "$PTY_OUT" | grep -q "installation cancelled" && ([ "$PTY_RC" = 1 ] && echo 0 || echo 1))" "$PTY_OUT"
else
  note_skip "interactive consent on a pseudo-terminal" "python3 not available"
fi

# ------------------------------------------------------------------ 4. native prerequisites
e="$(new_env no-linker)"
rm "$e/bin/cc"
run "$e" "$e" --install-rust
check "missing C linker: detected before anything is downloaded (rc 1)" "$(printf '%s' "$OUT" | grep -q "no C linker (cc) was found" && ([ "$RC" -eq 1 ] && notlogged "$e" "curl" && echo 0 || echo 1))" "$OUT"

# ------------------------------------------------------------------ 5. building
e="$(new_env build-ok)"
run "$e" "/"
exe="$e/work tree/GitMesh/target/release/gitmesh"
check "successful build: rc 0" "$RC" "$OUT"
printf '%s' "$OUT" | grep -qxF "  executable: $exe"; check "successful build: prints the full executable path (from Cargo's own output)" $?
[ -f "$exe" ] && [ -x "$exe" ]; check "successful build: the executable exists and is executable" $?
logged "$e" "build --release --locked"; check "cargo is run as: build --release --locked" $?
[ ! -e "$e/log.ran" ]; check "build.sh does NOT run the built GitMesh (the executable was not executed)" $?

e="$(new_env target-dir)"
run "$e" "$e" CARGO_TARGET_DIR="$e/custom target dir"
check "CARGO_TARGET_DIR (with spaces): build succeeds (rc 0)" "$RC" "$OUT"
printf '%s' "$OUT" | grep -qxF "  executable: $e/custom target dir/release/gitmesh"; check "CARGO_TARGET_DIR: the executable is reported under it" $?
[ ! -e "$e/work tree/GitMesh/target" ]; check "CARGO_TARGET_DIR: nothing written to the default target/" $?

e="$(new_env renamed-bin)"
run "$e" "$e" FAKE_BIN=mygitmesh-tool
printf '%s' "$OUT" | grep -qxF "  executable: $e/work tree/GitMesh/target/release/mygitmesh-tool"; check "executable name comes from Cargo's output, not a hard-coded name" $?

e="$(new_env build-fails)"
run "$e" "$e" FAKE_BUILD=fail
check "failed cargo build: clear message, rc 1" "$(printf '%s' "$OUT" | grep -q "cargo build --release failed" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"
printf '%s' "$OUT" | grep -q "compiler output is above"; check "failed cargo build: the compiler output is left visible" $?

e="$(new_env no-exe-file)"
run "$e" "$e" FAKE_BUILD=noexe
check "reported executable missing on disk: rc 1 with message" "$(printf '%s' "$OUT" | grep -q "does not exist or is not executable" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"

e="$(new_env no-artifact)"
run "$e" "$e" FAKE_BUILD=none
check "cargo reports no executable: rc 1" "$(printf '%s' "$OUT" | grep -q "Cargo reported no executable" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"

e="$(new_env two-executables)"
run "$e" "$e" FAKE_BUILD=multi
check "two executables: refuses to guess (rc 1)" "$(printf '%s' "$OUT" | grep -q "expected exactly one" && ([ "$RC" -eq 1 ] && echo 0 || echo 1))"

e="$(new_env existing-artifacts)"
mkdir -p "$e/work tree/GitMesh/target/debug" && echo keep > "$e/work tree/GitMesh/target/debug/keep.txt"
mkdir -p "$e/work tree/GitMesh/target/release" && echo old > "$e/work tree/GitMesh/target/release/old-artifact"
run "$e" "$e"
[ -f "$e/work tree/GitMesh/target/debug/keep.txt" ] && [ -f "$e/work tree/GitMesh/target/release/old-artifact" ]
check "existing build artifacts are not deleted" $?

# Invocation from outside the repository, with absolute and relative paths.
e="$(new_env from-outside)"
run "$e" "/"
check "invocation from / with an absolute path works" "$RC" "$OUT"
OUT="$(cd "$e" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" bash "./work tree/GitMesh/build.sh" 2>&1 < /dev/null)"; RC=$?
check "invocation from a parent directory with a relative path works (spaces in path)" "$RC" "$OUT"
OUT="$(cd "$e/work tree/GitMesh/tools" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" bash ../build.sh 2>&1 < /dev/null)"; RC=$?
check "invocation from inside tools/ works" "$RC" "$OUT"

# Argument handling.
e="$(new_env args)"
run "$e" "$e" --release
check "an unexpected argument is refused (rc 2) without building" "$([ "$RC" -eq 2 ] && notlogged "$e" "build --release" && echo 0 || echo 1)"
run "$e" "$e" --help
check "--help prints usage and does not build (rc 0)" "$([ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -q "Usage:" && notlogged "$e" "build --release" && echo 0 || echo 1)"
OUT="$(cd "$e" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" sh "$e/work tree/GitMesh/build.sh" --help 2>&1 < /dev/null)"; RC=$?
check "started as 'sh build.sh' it re-runs under bash" "$([ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -q "Usage:" && echo 0 || echo 1)"

# ------------------------------------------------------------------ 6. tools/rust-env.sh (developer wrapper)
e="$(new_env rust-env-ok)"
OUT="$(cd "$e/work tree/GitMesh" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" bash tools/rust-env.sh cargo --version 2>&1 < /dev/null)"; RC=$?
check "tools/rust-env.sh runs a command with the discovered toolchain" "$([ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -q "^cargo 1.99.0" && echo 0 || echo 1)" "$OUT"
e="$(new_env rust-env-missing)"
rm "$e/bin/cargo" "$e/bin/rustc" "$e/bin/rustup"
OUT="$(cd "$e/work tree/GitMesh" && env -i LANG=C PATH="$e/bin" HOME="$e/home" TMPDIR="$e/tmp" FAKE_LOG="$e/log" FAKE_TEMPLATES="$tpl" bash tools/rust-env.sh cargo --version 2>&1 < /dev/null)"; RC=$?
check "tools/rust-env.sh never installs; reports and exits 1 when Rust is missing" "$([ "$RC" -eq 1 ] && notlogged "$e" "curl" && printf '%s' "$OUT" | grep -q "build.sh" && echo 0 || echo 1)"

# ------------------------------------------------------------------ 7. no sudo anywhere
found="$(grep -l '^sudo ' "$work"/*/log 2>/dev/null || true)"
[ -z "$found" ]; check "sudo was never invoked in any case" $?

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
