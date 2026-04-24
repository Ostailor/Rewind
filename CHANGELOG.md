# Changelog

## 1.0.0-rc.1

- Prepared the first v1 release candidate without changing repository format or DB schema.
- Bumped CLI/workspace version to `1.0.0-rc.1`.
- Added the v1 command reference and repository format contract docs.
- Added a command safety matrix to the safety documentation.
- Reworked README into a shorter product, install, quickstart, recovery, and docs index.
- Added safe recovery/provenance examples and an example runner script.
- Polished CLI help/manpage safety notes for release-critical commands.

## 0.17.0

- Added Hardening Beta regression coverage for weird filenames, read-only command non-mutation, unsafe raw trace paths, journal archive names, and docs/scripts.
- Hardened raw trace path verification against parent-directory escapes.
- Sanitized archived journal filenames derived from journal IDs.
- Added safety, limitations, testing docs, and `scripts/ci-check.sh`.
- Kept repository format at version 2 and DB schema at version 1.

## 0.16.0

- Added `rewind version`, `rewind completions`, `rewind man`, `rewind env`, and `rewind self-test`.
- Added best-effort build metadata for git commit, dirty state, target, and build profile.
- Added installation, release, and example workflow docs for packaging a real CLI release.
- Kept repository format at version 2 and DB schema at version 1.

## Earlier Milestones

- 0.15: Symlink-as-data snapshots, executable-bit metadata, v2 snapshot manifests, and format-2 migration.
- 0.14: Repo-local config and `.rewindignore` for current/future scans.
- 0.13: Explicit `.rewind/repo.json` format metadata and migration hardening.
- 0.12: Replay workbench for historical run events.
- 0.11: Provenance and causality commands.
- 0.10: Optional Linux `strace` process tracing.
- 0.9: Read-only forensics and historical search.
- 0.8: Transactional restore and recovery journal.
- 0.7: Read-only terminal timeline UI.
