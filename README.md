# RewindOS

RewindOS is an OS-scale systems project about inspectability, reversibility, and time travel. This repository currently implements the host-mode `rewind` CLI: a Rust tool for snapshotting a project directory around shell commands and safely undoing recorded changes.

`rewind` is not an operating system, kernel, GUI, daemon, or filesystem watcher. Version 0.3 works inside a normal directory and records regular files and directories only.

## What `rewind` Does

- `rewind init` creates `.rewind/objects`, `.rewind/snapshots`, and `.rewind/events.db`.
- `rewind run -- <command> [args...]` snapshots the directory before and after the command, stores changed file content by SHA-256, and records an event in SQLite.
- `rewind status` compares the current worktree to Rewind's stored head snapshot.
- `rewind history` prints a compact event table.
- `rewind timeline` prints event kinds, snapshot transitions, and the current head snapshot.
- `rewind show <event-id>` prints one event and its changed files.
- `rewind diff <event-id>` shows the file and directory changes introduced by one event, with small text diffs when practical.
- `rewind undo` restores the latest non-undone event whose after snapshot matches the current head snapshot.
- `rewind undo --dry-run` prints the restore plan without changing files or history.
- `rewind restore <path> --before <event-id>` restores one file or directory subtree from before an event.
- `rewind restore <path> --after <event-id>` restores one file or directory subtree from after an event.
- Add `--dry-run` to targeted restore to print the path-scoped restore plan without changing files or history.

Snapshots always exclude `.rewind/` itself.

Rewind stores the current expected tree in `workspace_state.head_snapshot`. `rewind init` sets it to the initial snapshot, `rewind run` advances it to the command's after snapshot, and successful `rewind undo` moves it back to the undone event's before snapshot.

Before undo or targeted restore mutates files, Rewind scans the current directory and compares it to `head_snapshot`. If manual changes are present, the operation refuses to run and reports added, modified, and deleted paths. This prevents Rewind from deleting or overwriting work it did not record.

Targeted restore does not mark the source event as undone. A successful targeted restore creates a new `restore` event whose before snapshot is the old head and whose after snapshot is the restored tree. That restore event is then undoable with normal `rewind undo`.

## Demo

```sh
mkdir lab
cd lab
rewind init
rewind run -- sh -c "echo hello > notes.txt"
rewind run -- sh -c "echo goodbye > notes.txt"
rewind run -- rm notes.txt
rewind history
rewind status
rewind undo
cat notes.txt
```

Expected final output:

```text
goodbye
```

## Time Navigation Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo good > notes.txt"
rewind run -- sh -c "echo bad > notes.txt"

rewind timeline
rewind diff 2
rewind restore notes.txt --before 2

cat notes.txt
# expected: good

rewind history
# shows a new restore event

rewind undo
cat notes.txt
# expected: bad
```

From this repository, use Cargo directly:

```sh
cargo run -p rewind-cli -- init
cargo run -p rewind-cli -- run -- sh -c "echo hello > notes.txt"
cargo run -p rewind-cli -- status
cargo run -p rewind-cli -- history
cargo run -p rewind-cli -- timeline
cargo run -p rewind-cli -- diff 1
cargo run -p rewind-cli -- restore notes.txt --before 1 --dry-run
cargo run -p rewind-cli -- undo --dry-run
cargo run -p rewind-cli -- undo
```

## Development

```sh
cargo test
```

Project layout:

```text
Cargo.toml
crates/
  rewind-cli/      # clap-based command-line interface
  rewind-core/     # init, snapshot, diff, history, restore, and run logic
tests/
  integration_tests.rs
```

## Current Limitations

- Only the latest non-undone event can be undone by `rewind undo`.
- Undo marks the original event as `undone`; it does not create a separate undo event.
- Targeted restore creates a new event and does not mark its source event as undone.
- `rewind diff` uses a small, simple line-oriented text diff; it skips large, binary, or invalid UTF-8 content.
- Snapshotting supports regular files and directories only.
- Symlinks, hard links, owners, extended attributes, ACLs, and special files are ignored during snapshotting.
- Restore refuses to modify paths through symlinks.
- There is no daemon, GUI, kernel component, or filesystem event watcher.
- Restore is practical and cautious, but not a transactional filesystem operation.
