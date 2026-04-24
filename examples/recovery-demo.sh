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

echo
echo "Recovery status in a healthy repo:"
rewind recover --status

echo
echo "If a journaled undo/restore/checkout is interrupted, run:"
echo "  rewind recover --status"
echo "  rewind recover --complete"
echo "or, before metadata commit only:"
echo "  rewind recover --abort"

rewind verify
