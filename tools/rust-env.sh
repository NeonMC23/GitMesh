#!/usr/bin/env bash
# Ensure a Rust toolchain exists in this (possibly freshly recycled) sandbox, then run
# the given command with the right environment. Kept in the workspace because only
# /home/user persists between tool calls.
#
#   ./tools/rust-env.sh cargo test
set -euo pipefail

export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME=/usr/local/cargo
export PATH=/usr/local/cargo/bin:$PATH

if [ ! -x /usr/local/cargo/bin/cargo ]; then
  echo "[rust-env] installing toolchain (fresh sandbox)..." >&2
  mkdir -p /usr/local/rustup /usr/local/cargo
  curl -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
  sh /tmp/rustup-init.sh -y --no-modify-path --default-toolchain stable --profile minimal \
    >/tmp/rustup-install.log 2>&1 || { tail -20 /tmp/rustup-install.log; exit 1; }
  rustup component add rustfmt clippy >>/tmp/rustup-install.log 2>&1 || true
  echo "[rust-env] $(rustc --version), $(cargo --version)" >&2
fi

exec "$@"
