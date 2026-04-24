# Rewind Limitations

Rewind is at the `1.0.0-rc.1` release-candidate stage. Its safety model is intentionally narrow and explicit.

## Filesystem Metadata

Supported:

- Regular files.
- Directories and empty directories.
- Symlinks as symlinks.
- Unix executable bit for regular files where supported.

Not supported as first-class metadata:

- Hard-link identity.
- Owners, groups, ACLs, xattrs, and full mode bits.
- Sockets, FIFOs, block devices, and character devices.
- Platform-specific reparse point semantics.

Unsupported special files fail current scans clearly rather than being silently treated as regular files.

## Transactionality

The restore journal records durable intent and supports explicit recovery, but Rewind is not a fully atomic filesystem transaction system. Power loss, filesystem behavior, and external processes can still create states that require manual recovery.

## Replay

Replay is not a security sandbox. It is also not guaranteed to reproduce historical events exactly. Replays can diverge because of time, random values, absolute paths, host tools, environment variables, network state, permissions, or external files that were not captured in snapshots.

Legacy events without exact argv metadata use a Unix shell fallback when available.

## Tracing And Provenance

Process tracing is best-effort. Linux `strace` is the first supported tracer, and normal Rewind usage does not require it.

Provenance commands correlate available evidence. They do not prove that every dependency or syscall was captured.

## Platform Notes

Some symlink and executable-bit behavior is Unix-specific. Tests that require these capabilities should be platform-gated.
