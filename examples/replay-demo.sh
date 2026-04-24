#!/usr/bin/env sh
set -eu

lab="$(mktemp -d)"
echo "Using lab: $lab"
cleanup() {
  rm -rf "$lab"
}
trap cleanup EXIT INT TERM
cd "$lab"

rewind init
rewind run -- sh -c "echo hello > notes.txt"
rewind replay 1 --dry-run
rewind replay 1 --compare

rewind run -- sh -c "pwd > where.txt"
rewind replay 2 --compare
