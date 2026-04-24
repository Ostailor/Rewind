# Testing Rewind

## Required Local Checks

Run the same sequence used by maintainers:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p rewind-cli -- self-test
scripts/run-examples.sh
```

Or use:

```sh
scripts/ci-check.sh
```

## Test Shape

- Use temporary directories for integration tests.
- Do not require network access.
- Do not require Linux `strace`.
- Gate Unix-only cases such as symlink, executable-bit, or newline filename behavior.
- Prefer stable substrings over long exact CLI-output matches.
- Example scripts must create temporary directories and must not require network, `strace`, root privileges, or user-owned paths.

## Corruption Fixtures

Corruption tests may mutate `.rewind/` metadata inside a test temp directory. Keep fixtures small and local to the test unless a shared `tests/fixtures/` file makes the intent clearer.

Useful corruption scenarios:

- Malformed snapshot JSON.
- Invalid snapshot paths.
- Missing or corrupt referenced objects.
- Invalid repo manifests or schema metadata.
- Corrupt active journals.
- Unsafe raw trace paths.

## Self-Test

`rewind self-test` creates a temporary Rewind repo, runs a small command, verifies status, undoes the event, and runs `verify`. It should stay independent of the caller's current workspace.
