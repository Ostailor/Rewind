#!/usr/bin/env sh
set -eu

usage() {
  printf 'usage: %s\n' "$0"
}

if [ "${1:-}" = "--help" ]; then
  usage
  exit 0
fi

if [ "$#" -ne 0 ]; then
  usage >&2
  exit 2
fi

section() {
  printf '\n==> %s\n' "$1"
}

section "cargo fmt"
CARGO_NET_OFFLINE=true cargo fmt --check

section "cargo clippy"
CARGO_NET_OFFLINE=true cargo clippy --workspace --all-targets --all-features -- -D warnings

section "cargo test"
CARGO_NET_OFFLINE=true cargo test --workspace

section "rewind self-test"
CARGO_NET_OFFLINE=true cargo run -p rewind-cli -- self-test

section "examples"
scripts/run-examples.sh

section "ok"
