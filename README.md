# RewindOS

Rewind is a local time-travel tool for project directories. It records command effects, snapshots filesystem state, lets you inspect and restore history, and helps explain what changed through traces, provenance, and replay.

This repository currently ships the host-mode `rewind` CLI. It is not an operating system, kernel, GUI, daemon, filesystem watcher, network service, or security sandbox.

Version: `1.0.0-rc.1`

## Who It Is For

Rewind is for developers working in local project directories who want a command-level safety net outside normal version-control commits. It is useful when you want to:

- Run a risky command and keep a reversible before/after record.
- Capture manual worktree edits as explicit events.
- Inspect historical file contents without restoring them.
- Undo, restore, or checkout recorded states with recovery support.
- Explain what changed using file diffs, optional Linux `strace` metadata, provenance views, and replay.

It is not a replacement for Git, backups, OS snapshots, package manager sandboxes, or container isolation.

## Safe To Use Now

- Local snapshot history for regular files, directories, empty directories, symlinks, and the Unix executable bit.
- `run`, `commit`, `status`, `history`, `diff`, `undo`, targeted `restore`, `checkout`, checkpoints, `verify`, `recover`, and `gc`.
- Read-only forensics: `log`, `cat`, `deleted`, `grep`.
- Read-only explanation tools: `trace`, `explain`, `why`, `impact`, `graph`.
- Workspace-safe replay analysis for historical `run` events.
- Explicit repository format metadata and metadata-only migration from format 1 to format 2.

## Experimental Or Best-Effort

- Process tracing is optional and currently uses Linux `strace` when available.
- Raw traces are deleted by default; raw traces may contain sensitive absolute paths and process details when kept with `--trace-keep-raw`.
- Provenance depends on available trace metadata and is best-effort.
- Replay can diverge because old environment, time, absolute paths, host tools, network state, and external files are not fully captured.
- Replay is workspace-safe, not security-safe. Do not replay untrusted commands.

## Explicitly Not Supported

- Kernel, filesystem driver, FUSE mount, GUI desktop app, daemon, file watcher, or mutating TUI actions.
- Hard-link identity, owners, groups, ACLs, xattrs, sockets, FIFOs, block devices, character devices, and full mode bits.
- Network tracing or remote storage.
- AI features.

See [docs/LIMITATIONS.md](docs/LIMITATIONS.md) for the full limitations list.

## Install

From a checkout:

```sh
cargo install --path crates/rewind-cli
rewind --version
rewind self-test
```

For local development without installing:

```sh
cargo run -p rewind-cli -- --version
cargo run -p rewind-cli -- version
cargo run -p rewind-cli -- env
```

More installation notes are in [docs/INSTALL.md](docs/INSTALL.md).

## First Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo hello > notes.txt"
rewind run -- sh -c "echo goodbye > notes.txt"

rewind history
rewind diff 2
rewind undo
cat notes.txt
# expected: hello

rewind verify
```

The examples directory contains safe temp-directory workflows:

```sh
examples/basic-time-travel.sh
examples/ignore-demo.sh
examples/replay-demo.sh
examples/recovery-demo.sh
examples/provenance-demo.sh
```

Run all stable examples with:

```sh
scripts/run-examples.sh
```

## Recover From Problems

If Rewind reports an active recovery transaction:

```sh
rewind recover --status
rewind recover --complete
```

Use `rewind recover --abort` only when the transaction has not already committed metadata. If abort refuses, complete recovery first, then use normal `rewind undo` if you want to reverse the completed event.

If repository format metadata is missing or old:

```sh
rewind repo-info
rewind doctor
rewind migrate --check
rewind migrate
```

If storage corruption is suspected:

```sh
rewind verify
rewind verify --strict
rewind doctor
```

## Safety Model

Rewind stores metadata under `.rewind/`, which is always excluded from snapshots and restore plans. Restore-style operations (`undo`, targeted `restore`, and `checkout`) build a validated plan and write a recovery journal before mutating files. Most mutating commands refuse to run with an active journal.

Read-only commands must not create events, snapshots, objects, journals, checkpoints, or update `head_snapshot`.

See [docs/SAFETY.md](docs/SAFETY.md) for the safety matrix.

## Repository Format

The v1 RC storage contract is:

- Repository format: `2`
- DB schema: `1`
- New snapshots use v2 manifests with explicit file, directory, and symlink entries.
- Old v1 snapshot manifests remain readable and verifiable.

See [docs/REPO_FORMAT.md](docs/REPO_FORMAT.md) for the storage contract.

## Command Reference

Use `rewind --help` and `rewind <command> --help` for CLI syntax. A grouped v1 command reference is available in [docs/COMMANDS.md](docs/COMMANDS.md).

Common commands:

- Setup: `init`, `repo-info`, `doctor`, `migrate`, `config show`.
- Record: `run`, `commit`.
- Inspect: `status`, `history`, `timeline`, `show`, `diff`, `log`, `cat`, `deleted`, `grep`.
- Time travel: `undo`, `restore`, `checkout`, `checkpoint`.
- Maintenance: `recover`, `verify`, `stats`, `gc`.
- Explain/replay: `trace`, `explain`, `why`, `impact`, `graph`, `replay`.
- Release/diagnostics: `version`, `env`, `completions`, `man`, `self-test`.

## Ignore Rules And Config

`.rewind/config.toml` is repo-local Rewind configuration. Missing config uses defaults:

```toml
[ignore]
enabled = true
file = ".rewindignore"
```

`.rewindignore` lives in the workspace root by default and is normal workspace content. Ignore rules affect current/future scans only, not historical commands. Ignored tracked entries are carried forward from the head snapshot so adding ignore rules does not create fake deletions.

## Packaging

```sh
rewind completions bash > rewind.bash
rewind completions zsh > _rewind
rewind completions fish > rewind.fish
rewind man > rewind.1
rewind env
```

`rewind env` is read-only and useful for bug reports after reviewing local paths.

## Development Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p rewind-cli -- self-test
scripts/ci-check.sh
scripts/run-examples.sh
cargo run -p rewind-cli -- --version
cargo run -p rewind-cli -- version
cargo run -p rewind-cli -- man > rewind.1
cargo run -p rewind-cli -- completions bash > rewind.bash
```

Testing guidance is in [docs/TESTING.md](docs/TESTING.md). Release steps are in [docs/RELEASE.md](docs/RELEASE.md). The changelog is in [CHANGELOG.md](CHANGELOG.md).
