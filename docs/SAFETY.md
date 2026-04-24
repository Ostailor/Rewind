# Rewind Safety Model

Rewind is a host-mode CLI for local project history. It is designed to be conservative with project data, but it is not a replacement for backups, version control, or filesystem-level transactions.

## What Rewind Promises

- `.rewind/` metadata is excluded from workspace snapshots and restore plans.
- Mutating restore-style operations build and validate a restore plan before changing files.
- `undo`, targeted `restore`, and `checkout` create an active journal before mutating files.
- Most mutating commands refuse to run while an active journal exists.
- `recover --status` is read-only.
- `recover --complete` and `recover --abort` make the user choose an explicit recovery direction.
- Repository migrations are explicit and metadata-only.
- Read-only inspection commands must not create events, snapshots, objects, journals, checkpoints, or update `head_snapshot`.

## Command Safety Matrix

| Command | Mutates workspace? | Mutates `.rewind/` metadata? | Requires clean worktree? | Journaled? | Notes |
| --- | --- | --- | --- | --- | --- |
| `status`, `status --ignored` | No | No | No | No | Applies current ignore rules. |
| `run` | User command may | Yes | Yes, unless `--allow-dirty` | No | Records before/after snapshots and event metadata. |
| `run --allow-dirty` | User command may | Yes | No | No | Records `started_dirty = true`; use intentionally. |
| `commit` | No | Yes | No | No | Captures current manual changes as a commit event. |
| `commit --dry-run` | No | No | No | No | Preview only. |
| `undo` | Yes | Yes | Yes | Yes | Restores previous snapshot for latest undoable event. |
| `undo --dry-run` | No | No | Yes | No | Plan only. |
| `restore` | Yes | Yes | Yes | Yes | Path-scoped restore; creates restore event. |
| `restore --dry-run` | No | No | Yes | No | Plan only. |
| `checkout` | Yes | Yes | Yes | Yes | Whole-tree restore; creates checkout event. |
| `checkout --dry-run` | No | No | Yes | No | Plan only. |
| `recover`, `recover --status` | No | No | No | Reads journal | Status only. |
| `recover --complete` | Yes | Yes | No | Uses active journal | Completes interrupted transaction. |
| `recover --abort` | Yes | Yes | No | Uses active journal | Refuses if metadata already committed. |
| `verify`, `verify --strict` | No | No | No | No | Reports errors; does not repair. |
| `gc` | No | No | No | No | Dry-run by default. |
| `gc --yes` | No | Yes | No | No | Deletes only unreachable Rewind snapshots/objects. |
| `trace`, `explain`, `why`, `impact`, `graph` | No | No | No | No | Read-only analysis; provenance is best-effort. |
| `log`, `cat`, `deleted`, `grep` | No | No | No | No | Forensic historical commands; warn with active journal. |
| `replay --dry-run` | No | No | No | No | Planning only. |
| `replay --sandbox`, `replay --compare` | No real workspace | No real repo metadata | No | No | Uses temporary workspace; not a security sandbox. |
| `migrate --check` | No | No | No | No | Read-only migration status. |
| `migrate` | No | Yes | No | No | Metadata-only; refuses active journal. |
| `config show`, `repo-info`, `doctor`, `stats`, `history`, `timeline`, `show`, `diff`, `tui` | No | No | No | No | Read-only inspection. |
| `version`, `env`, `completions`, `man` | No | No | No | No | Do not require an initialized repo. |
| `self-test` | No caller workspace | No caller repo metadata | No | No | Mutates only its own temporary directory. |

## Path And Symlink Policy

- User-targeted workspace paths must be relative.
- Absolute paths, `..`, and paths under `.rewind/` are rejected where workspace-relative paths are required.
- Symlinks are stored as symlink entries with target text.
- Symlinks are not followed during snapshotting, dirty detection, restore, checkout, replay materialization, verify, or comparison.
- Restore refuses to modify a path through a symlink ancestor.
- A symlink target can point anywhere, but Rewind stores only the target string and does not scan or restore the target content.

## Replay Safety

Replay is workspace-safe analysis, not a security sandbox. It creates a temporary workspace outside the real project, materializes an old snapshot there, and runs the historical command there.

Do not replay untrusted commands. A replayed command can still use host tools, environment-visible resources, and normal process capabilities.

## Trace Privacy

Parsed trace metadata stores workspace-relative paths and redacts outside-workspace paths. Raw traces are deleted by default. If `--trace-keep-raw` is used, raw trace files may contain absolute paths and sensitive process details.

## Corruption Handling

`rewind verify` is the primary corruption detection tool. It checks repo metadata, schema metadata, snapshots, object hashes, trace metadata, and active journals. It reports problems; it does not silently repair them.

Use normal backups before relying on Rewind for important project directories.
