# Installing Rewind

## From Source

```sh
git clone <repo-url>
cd Rewind_OS
cargo install --path crates/rewind-cli
rewind --version
rewind self-test
```

## Local Development Build

```sh
cargo build --workspace
cargo run -p rewind-cli -- --version
cargo run -p rewind-cli -- self-test
```

## First Run

```sh
mkdir lab
cd lab
rewind init
rewind run -- sh -c "echo hello > notes.txt"
rewind status
rewind history
```

## Shell Completions

Generate completions from the installed binary and place them wherever your shell expects completion files:

```sh
rewind completions bash > rewind.bash
rewind completions zsh > _rewind
rewind completions fish > rewind.fish
rewind completions powershell > rewind.ps1
rewind completions elvish > rewind.elv
```

## Manpage

```sh
rewind man > rewind.1
man ./rewind.1
```

## Notes

- Rewind is a host-mode CLI, not a kernel, daemon, filesystem watcher, or security sandbox.
- Replay is workspace-safe analysis, but it still executes historical commands. Do not replay untrusted commands.
- Raw traces kept with `--trace-keep-raw` can contain sensitive absolute paths and process details.
