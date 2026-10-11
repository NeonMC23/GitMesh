#!/usr/bin/env bash
# Tests install.sh in a temporary directory: successful install, update, custom and
# home-relative destinations, refusals, --force, safe uninstall, permissions, and running the
# installed executable from another directory. It never touches the real ~/.local/bin.
# Uses the existing release build (--no-build); run ./build.sh first.
#
#   tools/install-check.sh

if [ -z "${BASH_VERSION:-}" ]; then exec bash "$0" "$@"; fi
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo/install.sh"
[ -x "$installer" ] || { echo "install-check: $installer is not executable" >&2; exit 1; }
[ -x "$repo/target/release/gitmesh" ] || { echo "install-check: run ./build.sh first" >&2; exit 1; }

# Only this directory is created and removed by the check.
work="$(mktemp -d "${TMPDIR:-/var/tmp}/gitmesh-install-check.XXXXXX")" || exit 1
cleanup() {
  # Make directories writable again before removing our own scratch tree.
  chmod -R u+w "$work" 2>/dev/null
  rm -rf -- "$work"
}
trap cleanup EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); echo "PASS: $1"; }
bad() { fail=$((fail + 1)); echo "FAIL: $1"; }
check() { # check "description" command...
  local desc="$1"; shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}
expect_fail() { # expect_fail "description" command...  (command must exit non-zero)
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then bad "$desc (unexpectedly succeeded)"; else ok "$desc"; fi
}

is_gitmesh() { grep -aq "GitMesh lets you work in a single local project" "$1" 2>/dev/null; }
same_file() { cmp -s -- "$1" "$2"; }
mode_is() { [ "$(stat -c %a -- "$1" 2>/dev/null)" = "$2" ]; }

home="$work/home"
mkdir -p "$home"
export HOME="$home"

# 1. Default destination under $HOME, no build.
check "default install exits 0" env HOME="$home" "$installer" --no-build
check "default install creates ~/.local/bin/gitmesh" test -f "$home/.local/bin/gitmesh"
check "installed file is GitMesh" is_gitmesh "$home/.local/bin/gitmesh"
check "installed file has mode 755" mode_is "$home/.local/bin/gitmesh" 755
check "installed file is identical to the release build" same_file "$repo/target/release/gitmesh" "$home/.local/bin/gitmesh"

# 2. Re-running updates in place and is idempotent.
out="$(env HOME="$home" "$installer" --no-build 2>&1)"
check "re-run reports 'already up to date'" grep -q "already up to date" <<<"$out"
check "re-run exits 0 and keeps mode 755" env HOME="$home" bash -c "'$installer' --no-build >/dev/null 2>&1 && test \"\$(stat -c %a '$home/.local/bin/gitmesh')\" = 755"

# 3. Custom destination: spaces and missing parents are created; unrelated files are kept.
custom="$work/custom dir/a/b"
mkdir -p "$work/custom dir/a/b"
echo "keep me" > "$work/custom dir/a/b/notes.txt"
check "custom --bin-dir install exits 0" "$installer" --no-build --bin-dir "$work/custom dir/a/b"
check "custom destination holds gitmesh" is_gitmesh "$custom/gitmesh"

nested="$work/new/deep/bin"
check "--bin-dir creates missing parents" "$installer" --no-build --bin-dir "$nested"
check "nested destination holds gitmesh" is_gitmesh "$nested/gitmesh"

# 4. --bin-dir=VALUE form and ~/ expansion.
check "--bin-dir=VALUE form works" "$installer" --no-build --bin-dir="$work/eq"
check "--bin-dir=VALUE installs gitmesh" is_gitmesh "$work/eq/gitmesh"
check "~/ prefix expands to \$HOME" env HOME="$home" "$installer" --no-build --bin-dir="~/tilde-bin"
check "~/ destination lands in \$HOME" is_gitmesh "$home/tilde-bin/gitmesh"

# 5. Invalid destinations are refused before anything changes.
expect_fail "relative --bin-dir is refused" "$installer" --no-build --bin-dir relative/bin
expect_fail "empty --bin-dir is refused" "$installer" --no-build --bin-dir ""
printf 'x' > "$work/afile"
expect_fail "--bin-dir pointing at a file is refused" "$installer" --no-build --bin-dir "$work/afile"
check "file destination is left as it was" test "$(cat "$work/afile")" = "x"
expect_fail "unknown argument is refused" "$installer" --bogus
check "unknown argument exits 2" bash -c "'$installer' --bogus >/dev/null 2>&1; test \$? -eq 2"

# 6. A different file at the destination is never overwritten without --force.
mkdir -p "$work/foreign"
echo "not gitmesh" > "$work/foreign/gitmesh"
expect_fail "install refuses a foreign file at the destination" "$installer" --no-build --bin-dir "$work/foreign"
check "foreign file is unchanged after refusal" test "$(cat "$work/foreign/gitmesh")" = "not gitmesh"
check "--force replaces a foreign file" "$installer" --no-build --bin-dir "$work/foreign" --force
check "--force result is GitMesh with mode 755" bash -c "is_gitmesh() { grep -aq 'GitMesh lets you work in a single local project' \"\$1\"; }; is_gitmesh '$work/foreign/gitmesh' && test \"\$(stat -c %a '$work/foreign/gitmesh')\" = 755"

# 7. A symlink at the destination is not followed or treated as GitMesh.
mkdir -p "$work/linked"
ln -s "$repo/target/release/gitmesh" "$work/linked/gitmesh"
expect_fail "install refuses a symlink at the destination" "$installer" --no-build --bin-dir "$work/linked"
check "symlink still points where it did" test -L "$work/linked/gitmesh"

# 7b. Even with --force, a symlink is refused and nothing is written through it (a symlink to a
# directory must not receive the new file).
mkdir -p "$work/linkdir-target" "$work/linked-force"
ln -s "$work/linkdir-target" "$work/linked-force/gitmesh"
expect_fail "--force refuses a symlink at the destination" "$installer" --no-build --bin-dir "$work/linked-force" --force
check "symlink target directory received nothing" test -z "$(ls -A "$work/linkdir-target")"
check "symlink itself is kept" test -L "$work/linked-force/gitmesh"

# 8. A directory at the destination is refused.
mkdir -p "$work/isdir/gitmesh"
expect_fail "install refuses a directory at the destination" "$installer" --no-build --bin-dir "$work/isdir"

# 9. Uninstall removes only the GitMesh executable.
check "uninstall exits 0" "$installer" --uninstall --bin-dir "$work/custom dir/a/b"
check "uninstall removed gitmesh" test ! -e "$custom/gitmesh"
check "uninstall kept the unrelated file" test "$(cat "$work/custom dir/a/b/notes.txt")" = "keep me"
check "uninstall kept the directory" test -d "$custom"
check "uninstall with nothing installed exits 0" "$installer" --uninstall --bin-dir "$work/custom dir/a/b"

mkdir -p "$work/foreign2"
echo "not gitmesh" > "$work/foreign2/gitmesh"
expect_fail "uninstall refuses a non-GitMesh file" "$installer" --uninstall --bin-dir "$work/foreign2"
check "non-GitMesh file survives uninstall" test "$(cat "$work/foreign2/gitmesh")" = "not gitmesh"
check "uninstall refuses a symlink" bash -c "'$installer' --uninstall --bin-dir '$work/linked' >/dev/null 2>&1; test \$? -ne 0 && test -L '$work/linked/gitmesh'"
expect_fail "--force cannot be combined with --uninstall" "$installer" --uninstall --force --bin-dir "$work/foreign2"
expect_fail "--no-build cannot be combined with --uninstall" "$installer" --uninstall --no-build --bin-dir "$work/foreign2"

# 10. Permission errors are reported, not hidden.
if [ "$(id -u)" -ne 0 ]; then
  mkdir -p "$work/readonly"
  chmod 0555 "$work/readonly"
  expect_fail "install into a read-only directory fails" "$installer" --no-build --bin-dir "$work/readonly"
  check "read-only directory stays empty" test -z "$(ls -A "$work/readonly")"
  chmod 0755 "$work/readonly"
else
  echo "SKIP: permission-denied case (running as root)"
fi

# 11. A missing release build fails with a clear message and installs nothing.
notarget="$work/no-such-target"
expect_fail "--no-build without a release build fails" env CARGO_TARGET_DIR="$notarget" "$installer" --no-build --bin-dir "$work/nobuild"
check "nothing installed when the build is missing" test ! -e "$work/nobuild/gitmesh"

# 12. The installed executable runs from another directory with a minimal environment.
run_dir="$work/elsewhere"
mkdir -p "$run_dir"
check "installed binary reports its version from another directory" bash -c "cd '$run_dir' && env -i PATH=/usr/bin:/bin '$home/.local/bin/gitmesh' --version | grep -q '^gitmesh '"
check "installed binary shows help from another directory" bash -c "cd '$run_dir' && env -i PATH=/usr/bin:/bin '$home/.local/bin/gitmesh' --help | grep -q 'Commands:'"

# 13. The PATH hint is printed when the destination is not on PATH.
out="$(PATH=/usr/bin:/bin "$installer" --no-build --bin-dir "$work/pathcheck" 2>&1)"
check "PATH hint printed when destination is not on PATH" grep -q "is not in your PATH" <<<"$out"
out="$(PATH="$work/pathcheck:/usr/bin:/bin" "$installer" --no-build --bin-dir "$work/pathcheck" 2>&1)"
check "no PATH hint when destination is on PATH" bash -c "! grep -q 'is not in your PATH' <<<\"\$1\"" _ "$out"

echo ""
echo "install-check: $pass PASS, $fail FAIL"
[ "$fail" -eq 0 ]
