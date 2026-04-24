# Rewind Repository Format

This document describes the `1.0.0-rc.1` repository storage contract.

- Supported repository format: `2`
- Supported SQLite DB schema: `1`
- Current snapshot manifest version for newly written snapshots: `2`
- Backward-compatible snapshot manifest version: `1`

Rewind must not silently migrate ordinary command execution. Legacy repositories are detected and guided to `rewind migrate`.

## Layout

```text
.rewind/
  repo.json
  config.toml
  events.db
  objects/
  snapshots/
  journal/
    active.json
    completed/
    aborted/
  traces/
    tmp/
```

`.rewind/` is always excluded from workspace snapshots, dirty detection, restore plans, checkout, replay materialization, grep, and TUI output.

## `.rewind/repo.json`

The repository manifest records the explicit format contract:

```json
{
  "format_version": 2,
  "db_schema_version": 1,
  "repo_id": "stable-repo-id",
  "created_at": "2026-04-24T10:00:00Z",
  "created_by_version": "1.0.0-rc.1",
  "last_migrated_at": "2026-04-24T10:00:00Z",
  "last_migrated_by_version": "1.0.0-rc.1"
}
```

The manifest is written with a temp-file-and-rename pattern. `repo_id` is stable for the life of the repository. `format_version` governs on-disk layout and snapshot semantics. `db_schema_version` must agree with SQLite schema metadata.

## `.rewind/config.toml`

Repo-local config is not tracked workspace content. Missing config uses built-in defaults:

```toml
[ignore]
enabled = true
file = ".rewindignore"
```

Unknown config keys and invalid ignore paths fail clearly for current-scan commands.

## `.rewindignore`

The configured ignore file lives in the workspace root by default, outside `.rewind/`, and is normal workspace content. It can be recorded in history. Ignore rules affect current/future scans only and do not erase historical snapshots.

## SQLite: `.rewind/events.db`

The database stores:

- Events and event metadata.
- File change summaries.
- Checkpoints.
- Workspace state, including `workspace_state.head_snapshot`.
- Restore transaction IDs.
- Trace metadata.
- Replay metadata fields for run events.
- Schema metadata.

Important event fields include:

- `kind`: examples include `run`, `commit`, `restore`, and `checkout`.
- `command`: rendered user-facing command string.
- `command_argv_json`: exact argv JSON for newer run events.
- `command_cwd_relative`: workspace-relative original cwd for newer run events.
- `before_snapshot` and `after_snapshot`.
- `exit_code`.
- `started_dirty`.
- `undone`.
- `transaction_id` for journaled operations.

`workspace_state.head_snapshot` records the snapshot Rewind expects the current worktree to match.

## Objects

`.rewind/objects/` stores SHA-256 content-addressed regular-file contents. Snapshot file entries reference object hashes. Verify checks object existence, hash, and size for reachable snapshots.

Objects are only regular-file content. Symlink targets are stored in snapshot manifests as target text, not as objects.

## Snapshots

`.rewind/snapshots/` stores JSON snapshot manifests. Snapshot IDs are deterministic content IDs calculated from snapshot entries.

### v1 Snapshot Compatibility

Older v1 manifests may have separate `directories` and `files` collections:

```json
{
  "id": "...",
  "created_at": "...",
  "directories": ["src"],
  "files": {
    "src/main.rs": {
      "hash": "...",
      "size": 123
    }
  }
}
```

Rewind `1.0.0-rc.1` can still read and verify v1 manifests using v1 semantics. Migration from repository format 1 to 2 does not rewrite old snapshot files.

### v2 Snapshot Manifests

New snapshots use explicit entry kinds:

```json
{
  "manifest_version": 2,
  "id": "...",
  "created_at": "...",
  "entries": {
    "src": {
      "kind": "directory"
    },
    "src/main.rs": {
      "kind": "file",
      "hash": "...",
      "size": 123,
      "executable": false
    },
    "bin/tool": {
      "kind": "file",
      "hash": "...",
      "size": 456,
      "executable": true
    },
    "linked-config": {
      "kind": "symlink",
      "target": "config/dev.toml"
    }
  }
}
```

Snapshot IDs include entry kind, file hash, file size, executable flag, symlink target text, and directory entries. Changing a symlink target or executable bit changes the snapshot ID.

Snapshot paths must be relative, must not contain `..`, and must not point under `.rewind/`. Symlink targets are target text and are not interpreted as workspace paths for escape checks.

## Filesystem Semantics

Supported entries:

- Regular files.
- Directories and empty directories.
- Symlinks as symlinks.
- Unix executable bit for regular files where supported.

Unsupported as first-class metadata:

- Hard-link identity.
- Owners, groups, ACLs, xattrs, and full mode bits.
- Sockets, FIFOs, block devices, character devices, and platform-specific metadata.

Symlinks are never followed during scanning, dirty detection, restore, checkout, replay materialization, verify, or comparison.

## Checkpoints

Checkpoints are metadata labels pointing to snapshot IDs. Creating, moving, listing, showing, or deleting a checkpoint does not create events, write snapshots, write objects, modify files, or update `head_snapshot`.

## Journal Lifecycle

Journaled operations are `undo`, targeted `restore`, and `checkout`.

Phases:

- `prepared`
- `applying`
- `committing`
- `committed`

The active journal lives at `.rewind/journal/active.json`. Completed or aborted journals are archived under `.rewind/journal/completed/` or `.rewind/journal/aborted/`. Archive filenames are sanitized from journal IDs and must remain under the journal directory.

`recover --status` is read-only. `recover --complete` and `recover --abort` are explicit user choices. Abort refuses after metadata commit.

## Traces

Parsed trace metadata lives in SQLite. Raw trace files are deleted by default. If `--trace-keep-raw` is used, raw traces must stay under `.rewind/traces/`; verify reports raw trace paths outside that tree as errors.

Parsed trace paths are workspace-relative when inside the workspace. Outside-workspace paths are redacted as `<outside-workspace>`.

## Migration From Format 1 To Format 2

Format-1 repositories are detected as needing migration. `rewind migrate` updates repository metadata to format 2 and schema metadata to schema 1 without:

- Rewriting old snapshots.
- Rewriting objects.
- Creating events.
- Modifying checkpoints.
- Modifying journals.
- Updating `head_snapshot`.
- Touching tracked workspace files.

After migration, old v1 snapshots remain readable and new snapshots are written as v2 manifests.
