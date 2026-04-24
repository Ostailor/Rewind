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
rewind run -- sh -c "echo config > config.toml"
rewind run --trace=auto -- sh -c "cat config.toml >/dev/null; echo hello > notes.txt"

echo
echo "Explain event 2:"
rewind explain 2

echo
echo "Why notes.txt is in its current state:"
rewind why notes.txt

echo
echo "Trace-based impact for config.toml, if trace data was captured:"
rewind impact config.toml

echo
echo "Text provenance graph:"
rewind graph 2
