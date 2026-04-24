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

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

if [ -n "${REWIND_BIN:-}" ]; then
  rewind_bin="$REWIND_BIN"
elif [ -x "$repo_root/target/debug/rewind" ]; then
  rewind_bin="$repo_root/target/debug/rewind"
else
  section "build rewind"
  CARGO_NET_OFFLINE=true cargo build -p rewind-cli
  rewind_bin="$repo_root/target/debug/rewind"
fi

tmp_bin="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp_bin"
}
trap cleanup EXIT INT TERM

cat > "$tmp_bin/rewind" <<EOF
#!/usr/bin/env sh
exec "$rewind_bin" "\$@"
EOF
chmod +x "$tmp_bin/rewind"
PATH="$tmp_bin:$PATH"
export PATH

for example in \
  examples/basic-time-travel.sh \
  examples/ignore-demo.sh \
  examples/replay-demo.sh \
  examples/recovery-demo.sh \
  examples/provenance-demo.sh
do
  section "$example"
  "$repo_root/$example"
done

section "examples ok"
