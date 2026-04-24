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
mkdir -p target
echo build > target/output.log
rewind status

cat > .rewindignore <<'EOF'
target/
*.tmp
EOF

rewind status
rewind status --ignored
rewind commit -m "Add ignore rules"
rewind run -- sh -c "echo hello > notes.txt"
rewind diff 2
rewind config show
