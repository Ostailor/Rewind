# Rewind Command Reference

This reference describes the public `rewind` CLI for `1.0.0-rc.1`.

Mutation labels:

- Read-only: does not mutate workspace or `.rewind/` state.
- Metadata: mutates only Rewind metadata.
- Workspace: may mutate workspace files and Rewind metadata.
- External temp: mutates only its own temporary directory.

## Setup

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind init` | Initialize `.rewind/` in the current directory. | `rewind init` | Metadata | Refuses incompatible existing repos; new repos are born current. |
| `rewind migrate` | Upgrade legacy repo metadata to the current format. | `rewind migrate` | Metadata | Refuses active journals; does not rewrite snapshots or tracked files. |
| `rewind migrate --check` | Check migration status. | `rewind migrate --check` | Read-only | Exits nonzero when migration is needed or impossible. |
| `rewind repo-info` | Show repo identity, counts, and migration status. | `rewind repo-info` | Read-only | Works on legacy/current repos when possible. |
| `rewind doctor` | Summarize repo health and suggested actions. | `rewind doctor` | Read-only | Exits nonzero for migration/integrity problems. |
| `rewind config show` | Show effective config and ignore status. | `rewind config show` | Read-only | Invalid config or ignore syntax is reported clearly. |

## Recording

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind run -- <cmd>` | Run a command and record before/after snapshots. | `rewind run -- sh -c "echo hi > notes.txt"` | Workspace + metadata | Refuses dirty worktrees by default. |
| `rewind run --allow-dirty -- <cmd>` | Run even when manual changes exist. | `rewind run --allow-dirty -- make` | Workspace + metadata | Records `started_dirty = true`; use intentionally. |
| `rewind run --trace=auto -- <cmd>` | Run with optional Linux `strace` metadata. | `rewind run --trace=auto -- make` | Workspace + metadata | Normal use never requires `strace`; auto falls back. |
| `rewind run --trace=strace -- <cmd>` | Require Linux `strace` before running. | `rewind run --trace=strace -- make` | Workspace + metadata | Fails before running if unsupported/unavailable. |
| `rewind run --trace-keep-raw -- <cmd>` | Preserve raw trace output. | `rewind run --trace=auto --trace-keep-raw -- make` | Workspace + metadata | Raw traces may contain sensitive absolute paths. |
| `rewind commit -m <msg>` | Capture manual worktree changes as an event. | `rewind commit -m "manual edit"` | Metadata | Advances `head_snapshot`; no event when clean. |
| `rewind commit --dry-run -m <msg>` | Preview manual commit. | `rewind commit --dry-run -m "manual edit"` | Read-only | Does not write objects, snapshots, events, or head. |

## Inspecting

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind status` | Compare current workspace to Rewind head. | `rewind status` | Read-only | Applies current ignore rules. |
| `rewind status --ignored` | Show status plus ignored paths. | `rewind status --ignored` | Read-only | Ignored paths do not change clean/dirty semantics. |
| `rewind history` | Show compact event history. | `rewind history` | Read-only | Includes undone/trace/replay indicators where available. |
| `rewind timeline` | Show event timeline and checkpoints. | `rewind timeline` | Read-only | Does not execute suggested commands. |
| `rewind show <event>` | Show one event and summaries. | `rewind show 3` | Read-only | Suggests related commands only. |
| `rewind diff <event>` | Show before/after diff for one event. | `rewind diff 3` | Read-only | Text diffs apply only to regular text files. |
| `rewind log <path>` | Show events that changed a path/subtree. | `rewind log notes.txt` | Read-only | Rejects absolute, `..`, and `.rewind/` paths. |
| `rewind cat <path> --after <event>` | Print historical file content. | `rewind cat notes.txt --after 2` | Read-only | Does not follow symlinks. |
| `rewind deleted` | List historical files missing at head. | `rewind deleted` | Read-only | Suggestions are best-effort restore commands. |
| `rewind grep <pattern> --history` | Search historical text files. | `rewind grep TODO --history` | Read-only | Skips binary/invalid UTF-8 and large files. |

## Time Travel

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind undo` | Undo the latest undoable event. | `rewind undo` | Workspace | Requires clean worktree; journaled. |
| `rewind undo --dry-run` | Preview latest undo. | `rewind undo --dry-run` | Read-only | No journal or metadata writes. |
| `rewind restore <path> --before <event>` | Restore a path from before an event. | `rewind restore notes.txt --before 3` | Workspace | Requires clean worktree; journaled; path-scoped. |
| `rewind restore <path> --after <event>` | Restore a path from after an event. | `rewind restore notes.txt --after 3` | Workspace | Creates a new restore event on success. |
| `rewind restore ... --dry-run` | Preview targeted restore. | `rewind restore notes.txt --before 3 --dry-run` | Read-only | No journal or metadata writes. |
| `rewind checkout --checkpoint <name>` | Restore whole tree to checkpoint. | `rewind checkout --checkpoint stable` | Workspace | Requires clean worktree; journaled; creates checkout event. |
| `rewind checkout --before/--after <event>` | Restore whole tree to event boundary. | `rewind checkout --before 4` | Workspace | Prior events remain active/undone as-is. |
| `rewind checkout --snapshot <id-prefix>` | Restore whole tree to snapshot. | `rewind checkout --snapshot abc123` | Workspace | Prefix must be unique. |
| `rewind checkout ... --dry-run` | Preview checkout. | `rewind checkout --before 4 --dry-run` | Read-only | No journal or metadata writes. |
| `rewind checkpoint create <name> -m <msg>` | Label current head snapshot. | `rewind checkpoint create stable -m "Known good"` | Metadata | Does not create a snapshot or event. |
| `rewind checkpoint list/show/delete` | Manage checkpoint metadata. | `rewind checkpoint list` | Read-only/Metadata | Delete removes only checkpoint metadata. |

## Recovery And Maintenance

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind recover` | Show active transaction status. | `rewind recover` | Read-only | Same as status by default. |
| `rewind recover --status` | Show active transaction status. | `rewind recover --status` | Read-only | Suggested before complete/abort. |
| `rewind recover --complete` | Finish interrupted transaction. | `rewind recover --complete` | Workspace | Idempotent metadata completion; explicit user choice. |
| `rewind recover --abort` | Abort interrupted transaction when safe. | `rewind recover --abort` | Workspace | Refuses after metadata commit. |
| `rewind verify` | Check repository integrity. | `rewind verify` | Read-only | Reports errors; does not repair. |
| `rewind verify --strict` | Treat warnings as failures. | `rewind verify --strict` | Read-only | Useful for release checks. |
| `rewind stats` | Show history/storage/replay/trace stats. | `rewind stats` | Read-only | Counts are derived from current metadata. |
| `rewind gc` | Preview unreachable storage cleanup. | `rewind gc` | Read-only | Dry-run by default. |
| `rewind gc --yes` | Delete unreachable snapshots/objects. | `rewind gc --yes` | Metadata | Never deletes reachable history or worktree files. |

## Tracing, Explanation, And Replay

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind trace <event>` | Show trace summary/details. | `rewind trace 2 --files` | Read-only | Missing trace data is normal. |
| `rewind explain <event>` | Explain final changes and trace correlation. | `rewind explain 2` | Read-only | Provenance is best-effort. |
| `rewind why <path>` | Explain current path state. | `rewind why notes.txt` | Read-only | Uses file_changes and current head. |
| `rewind impact <path>` | Show trace-based later access. | `rewind impact config.toml` | Read-only | Untraced events may be missing. |
| `rewind graph <event>` | Print text provenance graph. | `rewind graph 2` | Read-only | Graph is approximate when trace data is missing. |
| `rewind graph <event> --dot` | Emit Graphviz DOT. | `rewind graph 2 --dot` | Read-only | DOT is printed to stdout only. |
| `rewind replay <event> --dry-run` | Plan replay. | `rewind replay 2 --dry-run` | Read-only | Default mode; run events only. |
| `rewind replay <event> --sandbox` | Run replay in temp workspace. | `rewind replay 2 --sandbox` | External temp | Executes command on host; not a security sandbox. |
| `rewind replay <event> --compare` | Replay and show detailed comparison. | `rewind replay 2 --compare` | External temp | Does not compare historical stdout/stderr. |
| `rewind replay ... --keep` | Preserve replay sandbox. | `rewind replay 2 --compare --keep` | External temp | Leaves temp files for inspection. |

## UI

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind tui` | Open read-only terminal timeline UI. | `rewind tui` | Read-only | No mutating TUI actions in v1 RC. |
| `rewind tui --once` | Render deterministic TUI snapshot. | `rewind tui --once` | Read-only | Used by tests and demos. |
| `rewind tui --once --selected <event>` | Render with event selected. | `rewind tui --once --selected 2` | Read-only | Suggestions are informational only. |

## Release And Diagnostics

| Command | Description | Example | Mutation | Safety caveat |
| --- | --- | --- | --- | --- |
| `rewind --version` | Print short version. | `rewind --version` | Read-only | Does not require a repo. |
| `rewind version` | Print build and format info. | `rewind version` | Read-only | Git metadata may be `unknown`. |
| `rewind env` | Print bug-report diagnostics. | `rewind env` | Read-only | Review local paths before sharing. |
| `rewind completions <shell>` | Generate shell completions. | `rewind completions bash > rewind.bash` | Read-only | Supports bash, zsh, fish, powershell, elvish. |
| `rewind man` | Generate manpage to stdout. | `rewind man > rewind.1` | Read-only | Does not install the manpage. |
| `rewind self-test` | Run built-in smoke test in temp dir. | `rewind self-test` | External temp | Does not mutate the caller's repo. |
| `rewind self-test --keep` | Preserve self-test temp dir. | `rewind self-test --keep` | External temp | Prints kept path. |
