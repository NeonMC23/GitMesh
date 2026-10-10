#!/usr/bin/env bash
# Fast checks for ./build.sh. They never run a real Cargo build: each case copies build.sh and
# the real Cargo.toml into a temporary repository whose tools/rust-env.sh is a fake that records
# its arguments and succeeds or fails on request.
#
#   ./tools/build-script-check.sh
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
fail=0
check() { # check <name> <ok:0|1> [detail]
  if [ "$2" -eq 0 ]; then
    pass=$((pass + 1)); echo "PASS $1"
  else
    fail=$((fail + 1)); echo "FAIL $1${3:+ — $3}"
  fi
}

# A temporary repository: the real build.sh and Cargo.toml, plus a fake rust-env.sh.
# $1 = directory; FAKE_MODE in the fake: ok (builds the executable), fail (exit 3), noexe (exit 0, no file).
make_repo() {
  mkdir -p "$1/tools"
  cp "$repo/build.sh" "$1/build.sh"
  cp "$repo/Cargo.toml" "$1/Cargo.toml"
  cat > "$1/tools/rust-env.sh" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >> "$(dirname "$0")/../calls.log"
case "${FAKE_MODE:-ok}" in
  fail) echo "error: simulated compile failure" >&2; exit 3 ;;
  noexe) exit 0 ;;
  *)
    target="${CARGO_TARGET_DIR:-$(dirname "$0")/../target}"
    mkdir -p "$target/release"
    printf '#!/bin/sh\nexit 0\n' > "$target/release/gitmesh"
    chmod +x "$target/release/gitmesh"
    exit 0 ;;
esac
FAKE
  chmod +x "$1/tools/rust-env.sh"
}

# 1. Syntax and permissions of the real script.
bash -n "$repo/build.sh"; check "build.sh passes bash -n" $?
[ -x "$repo/build.sh" ]; check "build.sh is executable" $?
bash -n "$repo/tools/build-script-check.sh"; check "this check script passes bash -n" $?

# 2. Success from another directory, with a relative invocation path.
r="$work/ok"; make_repo "$r"
out="$(cd / && FAKE_MODE=ok bash "$r/build.sh" 2>&1)"; code=$?
check "success exits 0 when run from /" "$code" "$out"
printf '%s\n' "$out" | grep -qxF "  $r/target/release/gitmesh"; check "success reports the executable path (from the real Cargo.toml name)" $?
grep -qx 'cargo build --release' "$r/calls.log" 2>/dev/null; check "cargo is invoked as cargo build --release" $?
out2="$(cd "$work" && FAKE_MODE=ok bash ./ok/build.sh 2>&1)"; check "success works with a relative path from another directory" $?

# 3. The build fails: non-zero exit and a clear message.
r="$work/fail"; make_repo "$r"
out="$(FAKE_MODE=fail bash "$r/build.sh" 2>&1)"; code=$?
[ "$code" -ne 0 ]; check "a failed build exits non-zero" $?
printf '%s' "$out" | grep -q "cargo build --release failed"; check "a failed build prints an actionable message" $?

# 4. The build reports success but produces no executable.
r="$work/noexe"; make_repo "$r"
out="$(FAKE_MODE=noexe bash "$r/build.sh" 2>&1)"; code=$?
[ "$code" -ne 0 ]; check "a missing executable exits non-zero" $?
printf '%s' "$out" | grep -q "was not found"; check "a missing executable is reported clearly" $?

# 5. Arguments are refused, so a mistyped option cannot silently change the build.
r="$work/args"; make_repo "$r"
out="$(FAKE_MODE=ok bash "$r/build.sh" --debug 2>&1)"; code=$?
[ "$code" -ne 0 ]; check "an unexpected argument is refused" $?
[ ! -f "$r/calls.log" ]; check "a refused argument does not start a build" $?

# 6. Missing tools/rust-env.sh.
r="$work/noenv"; make_repo "$r"; rm "$r/tools/rust-env.sh"
out="$(bash "$r/build.sh" 2>&1)"; code=$?
[ "$code" -ne 0 ]; check "a missing rust-env.sh exits non-zero" $?
printf '%s' "$out" | grep -q "tools/rust-env.sh not found"; check "a missing rust-env.sh is named in the message" $?

# 7. Missing Cargo.toml.
r="$work/nocargo"; make_repo "$r"; rm "$r/Cargo.toml"
out="$(FAKE_MODE=ok bash "$r/build.sh" 2>&1)"; code=$?
[ "$code" -ne 0 ]; check "a missing Cargo.toml exits non-zero" $?

# 8. No [[bin]] name in Cargo.toml: refuse rather than guess.
r="$work/nobin"; make_repo "$r"; printf '[package]\nname = "gitmesh"\n' > "$r/Cargo.toml"
out="$(FAKE_MODE=ok bash "$r/build.sh" 2>&1)"; code=$?
[ "$code" -ne 0 ]; check "a Cargo.toml without a [[bin]] name is refused" $?

# 9. CARGO_TARGET_DIR is honoured for the reported location.
r="$work/tdir"; make_repo "$r"
out="$(cd / && CARGO_TARGET_DIR="$work/elsewhere" FAKE_MODE=ok bash "$r/build.sh" 2>&1)"; code=$?
check "CARGO_TARGET_DIR success exits 0" "$code" "$out"
printf '%s\n' "$out" | grep -qxF "  $work/elsewhere/release/gitmesh"; check "CARGO_TARGET_DIR location is reported" $?

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
