# Release Process

This project is at the `1.0.0-rc.1` release-candidate stage. Keep releases boring, reproducible, and storage-compatible.

## v1 RC Checklist

1. Update Cargo workspace/package versions.
2. Confirm repo format remains `2` and DB schema remains `1`.
3. Add a `CHANGELOG.md` entry.
4. Run the full local check script:

```sh
scripts/ci-check.sh
```

5. Run the built-in smoke test directly:

```sh
cargo run -p rewind-cli -- self-test
```

6. Run stable example scripts:

```sh
scripts/run-examples.sh
```

7. Check release identity:

```sh
cargo run -p rewind-cli -- --version
cargo run -p rewind-cli -- version
```

8. Generate release helper artifacts:

```sh
cargo run -p rewind-cli -- completions bash > rewind.bash
cargo run -p rewind-cli -- completions zsh > _rewind
cargo run -p rewind-cli -- completions fish > rewind.fish
cargo run -p rewind-cli -- man > rewind.1
```

9. Check README quickstart and docs links:

```sh
test -f docs/COMMANDS.md
test -f docs/SAFETY.md
test -f docs/LIMITATIONS.md
test -f docs/REPO_FORMAT.md
test -f docs/INSTALL.md
test -f docs/RELEASE.md
test -f docs/TESTING.md
```

10. Build a release binary:

```sh
cargo build --release -p rewind-cli
target/release/rewind version
target/release/rewind self-test
```

11. Tag the release candidate.
12. Publish artifacts later when binary distribution is ready.

## Required Checks

The CI script runs:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p rewind-cli -- self-test
```

The example script runner builds or locates a local `rewind` binary and runs temp-directory examples. It must not require network, `strace`, root privileges, symlink privileges, or user-file paths.

## Release Notes To Mention

- CLI version from `rewind --version`.
- Supported repository format and DB schema from `rewind version`.
- Migration requirement from `rewind migrate --check`.
- Replay warning: workspace-safe analysis is not a security sandbox.
- Trace privacy warning: raw traces can contain sensitive host paths.
- Symlink policy: symlinks are stored as symlinks and never followed.
- Unsupported filesystem metadata from `docs/LIMITATIONS.md`.

## Do Not Change In RC Polish

- Repository format.
- DB schema.
- Snapshot manifest semantics.
- Event kinds.
- Tracing backend.
- Replay isolation model.
- TUI mutation policy.
