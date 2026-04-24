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
rewind run -- sh -c "echo v1 > notes.txt"
rewind run -- sh -c "echo v2 > notes.txt"
rewind history
rewind undo

echo "notes.txt after undo:"
cat notes.txt
