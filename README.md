# RewindOS

RewindOS is an OS-scale systems project about inspectability, reversibility, and time travel. This repository currently implements the host-mode `rewind` CLI: a Rust tool for snapshotting a project directory around shell commands and safely undoing recorded changes.

`rewind` is not an operating system, kernel, GUI, daemon, or filesystem watcher. Version 0.9 works inside a normal directory and records regular files and directories only.

## What `rewind` Does

- `rewind init` creates `.rewind/objects`, `.rewind/snapshots`, and `.rewind/events.db`.
- `rewind run -- <command> [args...]` snapshots the directory before and after the command, stores changed file content by SHA-256, and records an event in SQLite.
- `rewind run --allow-dirty -- <command> [args...]` explicitly allows running when manual changes are present and marks the event as dirty-started.
- `rewind commit -m <message>` records manual worktree changes as a `commit` event.
- `rewind commit --dry-run -m <message>` previews the manual changes that would be recorded without writing snapshots, objects, or events.
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
- `rewind checkpoint create <name> -m <message>` creates a named label for the current head snapshot.
- `rewind checkpoint create --force <name> -m <message>` moves an existing checkpoint to the current head snapshot.
- `rewind checkpoint list`, `rewind checkpoint show <name>`, and `rewind checkpoint delete <name>` manage checkpoint metadata.
- `rewind checkout --checkpoint <name>` restores the whole worktree to a checkpoint snapshot.
- `rewind checkout --before <event-id>` and `rewind checkout --after <event-id>` restore the whole worktree to an event boundary.
- `rewind checkout --snapshot <snapshot-id-or-prefix>` restores the whole worktree to a stored snapshot id; unique prefixes are accepted.
- Add `--dry-run` to checkout to print the whole-tree restore plan without changing files or history.
- `rewind verify` checks Rewind metadata, snapshots, and objects without modifying anything.
- `rewind verify --strict` treats warnings, such as unreferenced storage, as failures.
- `rewind stats` prints history and storage counts.
- `rewind gc` previews unreachable snapshot and object cleanup.
- `rewind gc --yes` deletes only unreachable snapshots and objects.
- `rewind tui` opens a read-only interactive terminal timeline browser.
- `rewind tui --once` renders the same timeline information once to stdout for tests and demos.
- `rewind tui --once --selected <event-id>` renders the static view with a specific event selected.
- `rewind recover` and `rewind recover --status` show interrupted restore transaction status.
- `rewind recover --complete` completes an interrupted undo, restore, or checkout.
- `rewind recover --abort` returns to the old head when metadata was not committed yet.
- `rewind log <path>` shows the events that affected a file or directory subtree.
- `rewind cat <path> --before <event-id>` and `rewind cat <path> --after <event-id>` print historical file contents without restoring them.
- `rewind cat <path> --snapshot <snapshot-id-or-prefix>` and `rewind cat <path> --checkpoint <name>` read file contents from a stored snapshot or checkpoint.
- `rewind deleted` lists files known to history that are missing from the current head.
- `rewind grep <pattern> --snapshot <snapshot-id-or-prefix>`, `rewind grep <pattern> --checkpoint <name>`, and `rewind grep <pattern> --history` search remembered text files.

Snapshots always exclude `.rewind/` itself.

Rewind stores the current expected tree in `workspace_state.head_snapshot`. `rewind init` sets it to the initial snapshot, `rewind run` and `rewind commit` advance it to their after snapshots, and successful `rewind undo` moves it back to the undone event's before snapshot.

Every transition from one head snapshot to another should be represented by an explicit event. Before `run`, `undo`, or targeted restore mutates files, Rewind scans the current directory and compares it to `head_snapshot`. If manual changes are present, the operation refuses to run and reports added, modified, and deleted paths. This prevents Rewind from silently absorbing unrecorded work into the next command event.

Use `rewind commit -m "message"` to capture manual changes as a first-class `commit` event. After commit, `head_snapshot` advances to the new worktree state and `rewind status` is clean again.

Targeted restore does not mark the source event as undone. A successful targeted restore creates a new `restore` event whose before snapshot is the old head and whose after snapshot is the restored tree. That restore event is then undoable with normal `rewind undo`.

A checkpoint is metadata only: a name, message, timestamp, and snapshot id. Creating, moving, listing, showing, or deleting a checkpoint does not modify files, write a new snapshot, create an event, or advance `head_snapshot`.

Checkout is a full-worktree state transition. A successful checkout requires a clean worktree, restores the current tree to the selected snapshot, creates a new `checkout` event, and advances `head_snapshot`. Because checkout creates an event, normal `rewind undo` can undo it.

`rewind verify` is an fsck-style check for Rewind's own repository data. It verifies that `head_snapshot`, events, checkpoints, snapshot manifests, paths, object hashes, and object sizes are internally consistent. It does not repair or modify state.

Reachability starts from `workspace_state.head_snapshot`, every event's before and after snapshots, and every checkpoint's snapshot id. Undone events still preserve history, so their snapshots remain reachable. Objects are reachable when any reachable snapshot references them.

`rewind gc` is dry-run by default. `rewind gc --yes` removes only unreachable snapshot manifests and unreachable objects, then prunes empty object-store directories. It never deletes events, checkpoints, reachable history, `.rewind/events.db`, or working-tree files.

`rewind tui` is the first interactive timeline surface. It is strictly read-only: it can browse events, checkpoints, diffs, status, and storage stats, and it suggests commands to run outside the TUI, but it does not execute restore, checkout, undo, commit, GC, or any other mutating operation.

Restore-style operations are journaled under `.rewind/journal/` before they modify files. This includes `undo`, targeted `restore`, and `checkout`. If Rewind is interrupted while one of those operations is active, most mutating commands refuse to continue until `rewind recover` is used. `recover` is read-only by default. `recover --complete` finishes the interrupted operation by restoring the whole worktree to the journal target snapshot and finishing metadata updates. `recover --abort` restores the old head before metadata commit; after metadata is already committed, abort refuses and asks you to complete first, then use normal `undo` if needed.

The forensic commands are read-only. `log`, `cat`, `deleted`, and `grep` never write snapshots, objects, events, or `head_snapshot`. They may run while an active recovery journal exists, but they print a warning because results can reflect an in-progress restore.

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

## Manual Capture Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo original > notes.txt"

echo manual > notes.txt
rewind status

rewind run -- sh -c "echo later > other.txt"
# expected: refuses because worktree is dirty

rewind commit -m "Manual edit to notes"
rewind status
# expected: clean

rewind run -- sh -c "echo later > other.txt"
rewind timeline

rewind undo
rewind undo
cat notes.txt
# expected: original
```

The escape hatch is explicit:

```sh
echo dirty > scratch.txt
rewind run --allow-dirty -- sh -c "echo later > other.txt"
rewind show <event-id>
# shows: Started from dirty worktree: yes
```

## Checkpoint And Checkout Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo v1 > notes.txt"
rewind checkpoint create v1 -m "Version one"

rewind run -- sh -c "echo v2 > notes.txt"
cat notes.txt
# expected: v2

rewind checkout --checkpoint v1
cat notes.txt
# expected: v1

rewind timeline
# shows a checkout event and checkpoint list

rewind undo
cat notes.txt
# expected: v2

rewind checkout --before 2 --dry-run
# prints a restore plan and does not modify files
```

## Storage Integrity Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo v1 > notes.txt"
rewind checkpoint create v1 -m "Version one"
rewind run -- sh -c "echo v2 > notes.txt"

rewind stats
rewind verify

rewind checkout --snapshot <unique-prefix>
rewind verify

rewind gc
# dry-run only; pass --yes to delete unreachable storage
```

## Interactive Timeline Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo good > notes.txt"
rewind run -- sh -c "echo bad > notes.txt"
rewind checkpoint create before-bad -m "Before inspecting bad edit"

rewind tui --once
rewind tui --once --selected 2
rewind tui
```

Inside `rewind tui`, use `Up/Down` or `j/k` to move through events, `Tab` to switch the bottom panel, `r` to reload, `?` for help, and `q` or `Esc` to quit.

## Recovery Demo

Normal completed operations leave no active transaction:

```sh
mkdir lab
cd lab
rewind init
rewind run -- sh -c "echo v1 > notes.txt"
rewind run -- sh -c "echo v2 > notes.txt"
rewind undo
rewind recover --status
# expected: no active transaction
```

If Rewind is interrupted during `undo`, `restore`, or `checkout`, inspect the journal and choose a recovery direction:

```sh
rewind recover --status
rewind recover --complete
# or, before metadata commit:
rewind recover --abort
```

## Forensics Demo

```sh
mkdir lab
cd lab
rewind init

rewind run -- sh -c "echo good > notes.txt"
rewind run -- sh -c "echo bad > notes.txt"
rewind run -- rm notes.txt

rewind log notes.txt
rewind deleted
rewind cat notes.txt --before 3
rewind grep "good" --history
```

Expected behavior:

- `log` shows the create, modify, and delete events for `notes.txt`.
- `deleted` lists `notes.txt` with a suggested restore command.
- `cat notes.txt --before 3` prints `bad`.
- `grep "good" --history` finds the earlier snapshot that contained `good`.

Historical reads can also target checkpoints:

```sh
rewind checkout --before 3
rewind checkpoint create restored -m "State before deletion"
rewind cat notes.txt --checkpoint restored
```

From this repository, use Cargo directly:

```sh
cargo run -p rewind-cli -- init
cargo run -p rewind-cli -- run -- sh -c "echo hello > notes.txt"
cargo run -p rewind-cli -- status
cargo run -p rewind-cli -- commit --dry-run -m "manual changes"
cargo run -p rewind-cli -- commit -m "manual changes"
cargo run -p rewind-cli -- history
cargo run -p rewind-cli -- timeline
cargo run -p rewind-cli -- diff 1
cargo run -p rewind-cli -- restore notes.txt --before 1 --dry-run
cargo run -p rewind-cli -- checkpoint create v1 -m "Version one"
cargo run -p rewind-cli -- checkpoint list
cargo run -p rewind-cli -- checkpoint show v1
cargo run -p rewind-cli -- checkout --checkpoint v1 --dry-run
cargo run -p rewind-cli -- checkout --before 1 --dry-run
cargo run -p rewind-cli -- checkout --after 1 --dry-run
cargo run -p rewind-cli -- stats
cargo run -p rewind-cli -- verify
cargo run -p rewind-cli -- gc
cargo run -p rewind-cli -- tui --once
cargo run -p rewind-cli -- tui --once --selected 1
cargo run -p rewind-cli -- recover --status
cargo run -p rewind-cli -- log notes.txt
cargo run -p rewind-cli -- cat notes.txt --before 1
cargo run -p rewind-cli -- deleted
cargo run -p rewind-cli -- grep hello --history
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
- Manual captures create `commit` events and are undoable with normal `rewind undo`.
- Checkpoints are labels only; deleting a checkpoint does not garbage-collect snapshots or objects.
- Checkout creates `checkout` events and is undoable with normal `rewind undo`.
- Checkout requires a clean worktree and supports unique snapshot id prefixes for `--snapshot`.
- `rewind verify` reports unreferenced snapshots and objects as warnings; use `--strict` to fail on warnings.
- `rewind gc --yes` deletes unreachable snapshots and objects only; it does not compact history or vacuum SQLite.
- `rewind tui` is read-only and only suggests commands; it does not execute mutating actions.
- `rewind tui --once` is the deterministic rendering path used by integration tests.
- Restore-style operations use `.rewind/journal/active.json` while in progress and archive successful journals under `.rewind/journal/completed/`.
- v0.8 recovery provides durable intent and explicit complete/abort choices, but it is not a perfect crash-proof filesystem transaction system.
- v0.9 forensic commands are read-only and warn, rather than recover automatically, when an active journal exists.
- `rewind grep` skips binary, invalid UTF-8, and files larger than 1 MiB by default; history search returns at most 200 matches unless `--max-results` is provided.
- `rewind deleted` uses historical snapshots plus file-change rows for best-effort restore suggestions.
- `run --allow-dirty` is an escape hatch; the resulting event is marked as started from a dirty worktree.
- `rewind diff` uses a small, simple line-oriented text diff; it skips large, binary, or invalid UTF-8 content.
- Snapshotting supports regular files and directories only.
- Symlinks, hard links, owners, extended attributes, ACLs, and special files are ignored during snapshotting.
- Restore refuses to modify paths through symlinks.
- There is no daemon, GUI, kernel component, or filesystem event watcher.
- Restore is practical and cautious, but not a transactional filesystem operation.
