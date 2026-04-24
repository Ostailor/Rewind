use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use crossterm::event::{self, Event as TerminalEvent, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use rewind_core::diff::{diff_snapshots, ChangeType, FileChange, SnapshotDiff};
use rewind_core::object_store::ObjectStore;
use rewind_core::snapshot::load_snapshot;
use rewind_core::tui_model::{TuiEvent, TuiModel};
use rewind_core::{
    checkout, checkpoint, commit, config, forensics, history, init, integrity, provenance, replay,
    repo, restore, run, status, trace, transaction, REWIND_DIR,
};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "rewind",
    version,
    about = "Host-mode reversible command runner for local project time travel",
    long_about = REWIND_LONG_ABOUT
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

const REWIND_LONG_ABOUT: &str = "\
Rewind is a host-mode CLI that records workspace snapshots around commands so \
you can inspect, verify, undo, restore, checkout, replay, and explain local \
history.

Safety notes:
- .rewind/ metadata is always excluded from workspace snapshots.
- replay is workspace-safe analysis, not a security sandbox for untrusted commands.
- raw traces may contain absolute paths and sensitive process details.
- recover and migrate are explicit; ordinary commands do not silently repair repositories.";

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize a new Rewind repository in the current directory.
    #[command(
        long_about = "Initialize a new Rewind repository in the current directory.\n\nCreates .rewind/ metadata, the object store, snapshot store, SQLite history, repo manifest, and default config. Existing Rewind repositories are not silently migrated."
    )]
    Init,
    /// Show Rewind build, repo-format, and platform information.
    Version,
    /// Generate shell completions to stdout.
    Completions { shell: CompletionShell },
    /// Generate a roff manpage to stdout.
    Man,
    /// Print a read-only environment report for bug reports.
    Env,
    /// Run a small smoke test in a temporary directory.
    #[command(
        long_about = "Run a small built-in smoke test in a temporary directory.\n\nThis mutates only its own temp directory, never the caller's current workspace. Use --keep to preserve the temp directory for inspection."
    )]
    SelfTest {
        /// Keep the temporary self-test repository after the test finishes.
        #[arg(long)]
        keep: bool,
    },
    /// Inspect repository identity, format, and migration status.
    RepoInfo,
    /// Run a read-only repository health summary.
    Doctor,
    /// Upgrade legacy Rewind metadata to the current explicit format.
    #[command(
        long_about = "Upgrade legacy Rewind metadata to the current explicit repository format.\n\nMigration is metadata-only: it may create or update .rewind/repo.json and schema metadata, but must not touch tracked files, events, snapshots, objects, checkpoints, journals, or head_snapshot. --check is read-only. Migration refuses while an active recovery journal exists."
    )]
    Migrate {
        /// Check migration status without writing metadata.
        #[arg(long)]
        check: bool,
    },
    /// Show repo-local configuration and ignore-rule status.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run a command and record its filesystem effect.
    #[command(
        long_about = "Run a command and record the before/after workspace snapshots as a run event.\n\nSafety: refuses dirty worktrees by default so unrecorded manual edits are not silently absorbed. Use --allow-dirty only when you intentionally want to start from a dirty worktree. Tracing is optional; --trace-keep-raw may preserve sensitive absolute paths in raw trace files."
    )]
    Run {
        /// Allow running even when the worktree is dirty; records started_dirty = true.
        #[arg(long)]
        allow_dirty: bool,
        /// Trace mode: off, auto, or strace. Bare --trace means --trace=auto.
        #[arg(long, value_name = "MODE", num_args = 0..=1, default_missing_value = "auto", default_value = "off")]
        trace: String,
        /// Keep raw trace output; raw traces may contain sensitive absolute paths.
        #[arg(long)]
        trace_keep_raw: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Capture current manual worktree changes as a commit event.
    #[command(
        long_about = "Capture current manual worktree changes as a commit event.\n\nThis advances head_snapshot to the current workspace state. --dry-run previews changes without writing snapshots, objects, events, or head_snapshot."
    )]
    Commit {
        #[arg(short, long)]
        message: String,
        /// Preview the commit without writing Rewind metadata.
        #[arg(long)]
        dry_run: bool,
    },
    /// Create, list, show, or delete snapshot checkpoints.
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
    /// Restore the full worktree to a checkpoint, event boundary, or snapshot.
    #[command(
        long_about = "Restore the full worktree to a checkpoint, event boundary, or snapshot.\n\nSafety: requires a clean worktree, builds a restore plan, writes a recovery journal before mutating files, creates a checkout event on success, and can be undone with rewind undo. --dry-run prints the plan without mutating files or metadata."
    )]
    Checkout {
        #[arg(long, conflicts_with_all = ["before", "after", "snapshot"], required_unless_present_any = ["before", "after", "snapshot"])]
        checkpoint: Option<String>,
        #[arg(long, conflicts_with_all = ["checkpoint", "after", "snapshot"], required_unless_present_any = ["checkpoint", "after", "snapshot"])]
        before: Option<i64>,
        #[arg(long, conflicts_with_all = ["checkpoint", "before", "snapshot"], required_unless_present_any = ["checkpoint", "before", "snapshot"])]
        after: Option<i64>,
        #[arg(long, conflicts_with_all = ["checkpoint", "before", "after"], required_unless_present_any = ["checkpoint", "before", "after"])]
        snapshot: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true)]
        debug_stop_after_journal: bool,
        #[arg(long, hide = true)]
        debug_stop_after_commit: bool,
    },
    /// Verify repository metadata, snapshots, objects, traces, and journals.
    #[command(
        long_about = "Verify repository metadata, snapshots, objects, traces, journals, config, and format metadata.\n\nThis command is read-only. Use --strict to treat warnings, such as unreferenced storage, as failures."
    )]
    Verify {
        /// Treat warnings, such as unreferenced storage, as failures.
        #[arg(long)]
        strict: bool,
    },
    /// Show repository history, storage, trace, replay, and format stats.
    Stats,
    /// Garbage-collect unreachable snapshots and objects.
    #[command(
        long_about = "Garbage-collect unreachable Rewind snapshots and objects.\n\nWithout --yes this is a dry run. With --yes, Rewind deletes only unreachable snapshot manifests and objects; it never deletes events, checkpoints, reachable history, .rewind/events.db, or working-tree files. Refuses when repository integrity checks report reachable-storage errors."
    )]
    Gc {
        /// Actually delete unreachable storage; omitted means dry-run.
        #[arg(long)]
        yes: bool,
    },
    /// Inspect or finish an interrupted journaled operation.
    #[command(
        long_about = "Inspect or finish an interrupted undo, restore, or checkout transaction.\n\nDefault and --status are read-only. --complete restores the journal target and finishes metadata. --abort restores the old head only when metadata was not already committed; after commit, complete first and use normal undo if needed."
    )]
    Recover {
        /// Show active recovery status without mutating anything.
        #[arg(long)]
        status: bool,
        /// Complete the interrupted transaction.
        #[arg(long, conflicts_with = "abort")]
        complete: bool,
        /// Abort the interrupted transaction when safe.
        #[arg(long, conflicts_with = "complete")]
        abort: bool,
    },
    /// Open or render the read-only terminal timeline UI.
    Tui {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        selected: Option<i64>,
    },
    /// Show events that affected a file or directory path.
    Log {
        path: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        include_trace: bool,
    },
    /// Show captured process/file trace metadata for one event.
    #[command(
        long_about = "Show captured process/file trace metadata for one event.\n\nTrace data is optional observability metadata. Snapshot diffs and file_changes remain the source of truth. Raw traces are not shown, and outside-workspace paths are redacted in parsed trace metadata."
    )]
    Trace {
        event_id: i64,
        #[arg(long)]
        files: bool,
        #[arg(long)]
        processes: bool,
        #[arg(long)]
        summary: bool,
    },
    /// Explain one event using final changes and optional trace metadata.
    Explain {
        event_id: i64,
        #[arg(long)]
        summary: bool,
    },
    /// Explain why a path is in its current state.
    Why { path: String },
    /// Show trace-based events that may have depended on a path.
    Impact {
        path: String,
        #[arg(long)]
        since: Option<i64>,
        #[arg(long)]
        until: Option<i64>,
    },
    /// Print a text or Graphviz provenance graph for one event.
    Graph {
        event_id: i64,
        #[arg(long)]
        dot: bool,
    },
    /// Replay a historical run event in a temporary workspace; not a security sandbox.
    #[command(
        long_about = "Replay a historical run event in a temporary workspace and compare the result.\n\nReplay supports run events only. It never mutates the real workspace or real .rewind/ metadata, but it still executes a command on the host and is not a security sandbox. --keep preserves the temporary sandbox for inspection."
    )]
    Replay {
        event_id: i64,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        sandbox: bool,
        #[arg(long)]
        compare: bool,
        #[arg(long)]
        keep: bool,
    },
    /// Print historical file content from an event, snapshot, or checkpoint.
    Cat {
        path: String,
        #[arg(long, conflicts_with_all = ["after", "snapshot", "checkpoint"], required_unless_present_any = ["after", "snapshot", "checkpoint"])]
        before: Option<i64>,
        #[arg(long, conflicts_with_all = ["before", "snapshot", "checkpoint"], required_unless_present_any = ["before", "snapshot", "checkpoint"])]
        after: Option<i64>,
        #[arg(long, conflicts_with_all = ["before", "after", "checkpoint"], required_unless_present_any = ["before", "after", "checkpoint"])]
        snapshot: Option<String>,
        #[arg(long, conflicts_with_all = ["before", "after", "snapshot"], required_unless_present_any = ["before", "after", "snapshot"])]
        checkpoint: Option<String>,
        #[arg(long)]
        raw: bool,
    },
    /// List paths known to history that are missing at current head.
    Deleted {
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        path: Option<String>,
    },
    /// Search remembered UTF-8 text files in snapshots or history.
    Grep {
        pattern: String,
        #[arg(long, conflicts_with_all = ["checkpoint", "history"], required_unless_present_any = ["checkpoint", "history"])]
        snapshot: Option<String>,
        #[arg(long, conflicts_with_all = ["snapshot", "history"], required_unless_present_any = ["snapshot", "history"])]
        checkpoint: Option<String>,
        #[arg(long, conflicts_with_all = ["snapshot", "checkpoint"], required_unless_present_any = ["snapshot", "checkpoint"])]
        history: bool,
        #[arg(long)]
        ignore_case: bool,
        #[arg(long, default_value_t = 200)]
        max_results: usize,
    },
    /// Print compact event history.
    History,
    /// Print timeline view with event transitions and checkpoints.
    Timeline,
    /// Show the file and metadata diff for one event.
    Diff { event_id: i64 },
    /// Show one event in detail.
    Show { event_id: i64 },
    /// Show current worktree status relative to Rewind head.
    Status {
        #[arg(long)]
        ignored: bool,
    },
    /// Undo the latest undoable event.
    #[command(
        long_about = "Undo the latest undoable event by restoring its before snapshot.\n\nSafety: requires a clean worktree, writes a recovery journal before mutating files, and updates head_snapshot only after the restore succeeds. --dry-run prints the plan without mutating files or metadata."
    )]
    Undo {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true)]
        debug_stop_after_journal: bool,
        #[arg(long, hide = true)]
        debug_stop_after_commit: bool,
    },
    /// Restore one file or directory subtree from before/after an event.
    #[command(
        long_about = "Restore one workspace-relative file or directory subtree from before or after an event.\n\nSafety: rejects absolute paths, .., and .rewind/ paths; requires a clean worktree; writes a recovery journal before mutation; creates a restore event on success. --dry-run prints the plan without mutation."
    )]
    Restore {
        path: String,
        #[arg(long, conflicts_with = "after", required_unless_present = "after")]
        before: Option<i64>,
        #[arg(long, conflicts_with = "before", required_unless_present = "before")]
        after: Option<i64>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true)]
        debug_stop_after_journal: bool,
        #[arg(long, hide = true)]
        debug_stop_after_commit: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    #[value(name = "powershell")]
    PowerShell,
    Elvish,
}

impl From<CompletionShell> for Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::PowerShell => Self::PowerShell,
            CompletionShell::Elvish => Self::Elvish,
        }
    }
}

#[derive(Debug, Subcommand)]
enum CheckpointCommand {
    Create {
        #[arg(long)]
        force: bool,
        name: String,
        #[arg(short, long)]
        message: String,
    },
    List,
    Show {
        name: String,
    },
    Delete {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show effective repo-local config and ignore-rule status.
    #[command(
        long_about = "Show effective repo-local Rewind config and ignore-rule status.\n\nThis command is read-only. Missing .rewind/config.toml uses built-in defaults; invalid config or ignore syntax is reported clearly."
    )]
    Show,
}

fn main() {
    if let Err(error) = run_cli() {
        eprintln!("error: {error:?}");
        std::process::exit(1);
    }
}

fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let project_dir = env::current_dir().context("reading current directory")?;

    preflight_repo(&project_dir, &cli.command)?;

    match cli.command {
        Commands::Init => {
            init::init_project(&project_dir)?;
            println!("initialized .rewind");
        }
        Commands::Version => print_version_info(),
        Commands::Completions { shell } => print_completions(shell),
        Commands::Man => print_manpage(),
        Commands::Env => print_environment(&project_dir)?,
        Commands::SelfTest { keep } => run_self_test(keep)?,
        Commands::RepoInfo => print_repo_info(&project_dir)?,
        Commands::Doctor => handle_doctor(&project_dir)?,
        Commands::Migrate { check } => handle_migrate(&project_dir, check)?,
        Commands::Config { command } => handle_config(&project_dir, command)?,
        Commands::Run {
            command,
            allow_dirty,
            trace,
            trace_keep_raw,
        } => match run::run_command_with_trace(
            &project_dir,
            &command,
            allow_dirty,
            trace::parse_mode(&trace)?,
            trace_keep_raw,
        )? {
            run::RunOutcome::Ran {
                exit_code,
                trace_unavailable,
            } => {
                if let Some(reason) = trace_unavailable {
                    eprintln!("Warning: tracing unavailable: {reason}");
                }
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
            run::RunOutcome::Dirty { status } => {
                print_run_dirty_report(&status);
                std::process::exit(1);
            }
        },
        Commands::Commit { message, dry_run } => {
            match commit::commit_worktree(&project_dir, &message, dry_run)? {
                commit::CommitOutcome::Committed { event_id, .. } => {
                    println!("created commit event {event_id}");
                }
                commit::CommitOutcome::DryRun { diff } => {
                    println!("Would commit manual changes: {message}");
                    print_snapshot_diff_groups(&diff);
                }
                commit::CommitOutcome::Clean => {
                    println!("Nothing to commit. Rewind worktree clean.");
                }
            }
        }
        Commands::Checkpoint { command } => handle_checkpoint(&project_dir, command)?,
        Commands::Checkout {
            checkpoint,
            before,
            after,
            snapshot,
            dry_run,
            debug_stop_after_journal,
            debug_stop_after_commit,
        } => handle_checkout(
            &project_dir,
            checkpoint,
            before,
            after,
            snapshot,
            dry_run,
            debug_stop(debug_stop_after_journal, debug_stop_after_commit),
        )?,
        Commands::Verify { strict } => handle_verify(&project_dir, strict)?,
        Commands::Stats => print_stats(&project_dir)?,
        Commands::Gc { yes } => handle_gc(&project_dir, yes)?,
        Commands::Recover {
            status,
            complete,
            abort,
        } => handle_recover(&project_dir, status, complete, abort)?,
        Commands::Tui { once, selected } => handle_tui(&project_dir, once, selected)?,
        Commands::Log {
            path,
            limit,
            include_trace,
        } => {
            warn_active_transaction(&project_dir);
            print_path_history(&project_dir, &path, limit, include_trace)?;
        }
        Commands::Trace {
            event_id,
            files,
            processes,
            summary,
        } => {
            warn_active_transaction(&project_dir);
            print_trace(&project_dir, event_id, files, processes, summary)?;
        }
        Commands::Explain { event_id, summary } => {
            warn_active_transaction(&project_dir);
            print_explain(&project_dir, event_id, summary)?;
        }
        Commands::Why { path } => {
            warn_active_transaction(&project_dir);
            print_why(&project_dir, &path)?;
        }
        Commands::Impact { path, since, until } => {
            warn_active_transaction(&project_dir);
            print_impact(&project_dir, &path, since, until)?;
        }
        Commands::Graph { event_id, dot } => {
            warn_active_transaction(&project_dir);
            print_graph(&project_dir, event_id, dot)?;
        }
        Commands::Replay {
            event_id,
            dry_run,
            sandbox,
            compare,
            keep,
        } => handle_replay(&project_dir, event_id, dry_run, sandbox, compare, keep)?,
        Commands::Cat {
            path,
            before,
            after,
            snapshot,
            checkpoint,
            raw,
        } => {
            warn_active_transaction(&project_dir);
            print_cat(
                &project_dir,
                &path,
                before,
                after,
                snapshot,
                checkpoint,
                raw,
            )?;
        }
        Commands::Deleted { limit, path } => {
            warn_active_transaction(&project_dir);
            print_deleted(&project_dir, path.as_deref(), limit)?;
        }
        Commands::Grep {
            pattern,
            snapshot,
            checkpoint,
            history,
            ignore_case,
            max_results,
        } => {
            warn_active_transaction(&project_dir);
            print_grep(
                &project_dir,
                &pattern,
                snapshot,
                checkpoint,
                history,
                ignore_case,
                max_results,
            )?;
        }
        Commands::History => print_history(&project_dir)?,
        Commands::Timeline => print_timeline(&project_dir)?,
        Commands::Diff { event_id } => print_diff(&project_dir, event_id)?,
        Commands::Show { event_id } => print_event(&project_dir, event_id)?,
        Commands::Status { ignored } => print_status(&project_dir, ignored)?,
        Commands::Undo {
            dry_run,
            debug_stop_after_journal,
            debug_stop_after_commit,
        } => match restore::undo_latest_with_debug(
            &project_dir,
            dry_run,
            debug_stop(debug_stop_after_journal, debug_stop_after_commit),
        )? {
            restore::UndoOutcome::Applied { event_id } => println!("undid event {event_id}"),
            restore::UndoOutcome::DryRun { event_id, plan } => {
                println!("Dry run for event {event_id}.");
                print_restore_plan(&plan);
            }
            restore::UndoOutcome::Dirty { status } => {
                print!("{}", status::dirty_report(&status));
                std::process::exit(1);
            }
            restore::UndoOutcome::NothingToUndo => println!("Nothing to undo."),
        },
        Commands::Restore {
            path,
            before,
            after,
            dry_run,
            debug_stop_after_journal,
            debug_stop_after_commit,
        } => {
            let (source, event_id) = match (before, after) {
                (Some(event_id), None) => (restore::RestoreSource::Before, event_id),
                (None, Some(event_id)) => (restore::RestoreSource::After, event_id),
                _ => bail!("choose exactly one of --before or --after"),
            };
            match restore::targeted_restore_with_debug(
                &project_dir,
                &path,
                source,
                event_id,
                dry_run,
                debug_stop(debug_stop_after_journal, debug_stop_after_commit),
            )? {
                restore::TargetedRestoreOutcome::Applied { event_id, .. } => {
                    println!("created restore event {event_id}");
                }
                restore::TargetedRestoreOutcome::DryRun { plan } => {
                    println!(
                        "Restore plan for {path} from {} event {event_id}:",
                        source.as_str()
                    );
                    print_would_restore_plan(&plan);
                }
                restore::TargetedRestoreOutcome::Dirty { status } => {
                    print!("{}", status::dirty_report(&status));
                    std::process::exit(1);
                }
                restore::TargetedRestoreOutcome::NothingToRestore => {
                    println!("Nothing to restore.");
                }
            }
        }
    }

    Ok(())
}

fn preflight_repo(project_dir: &Path, command: &Commands) -> Result<()> {
    match command {
        Commands::Version
        | Commands::Completions { .. }
        | Commands::Man
        | Commands::Env
        | Commands::SelfTest { .. }
        | Commands::Config {
            command: ConfigCommand::Show,
        }
        | Commands::RepoInfo
        | Commands::Doctor
        | Commands::Migrate { .. }
        | Commands::Verify { .. } => Ok(()),
        Commands::Init => {
            let status = repo::inspect(project_dir);
            if status.status == repo::RepoStatus::NeedsMigration {
                bail!("This Rewind repo needs migration. Run: rewind migrate");
            }
            if status.status == repo::RepoStatus::IncompatibleFutureFormat {
                bail!("This Rewind repo uses a newer unsupported format.");
            }
            if status.status == repo::RepoStatus::Invalid {
                bail!("This Rewind repo has invalid format metadata. Run: rewind doctor");
            }
            Ok(())
        }
        _ => repo::ensure_current(project_dir),
    }
}

fn print_version_info() {
    println!("Rewind version");
    println!();
    println!("CLI version:              {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Supported repo format:    {}",
        repo::CURRENT_REPO_FORMAT_VERSION
    );
    println!(
        "Supported DB schema:      {}",
        repo::CURRENT_DB_SCHEMA_VERSION
    );
    println!("Rust target:              {}", build_target());
    println!("Build profile:            {}", build_profile());
    println!("Git commit:               {}", git_commit());
    println!("Git dirty:                {}", git_dirty());
}

fn print_completions(shell: CompletionShell) {
    let mut command = Cli::command();
    let bin_name = command.get_name().to_owned();
    clap_complete::generate(
        Shell::from(shell),
        &mut command,
        bin_name,
        &mut io::stdout(),
    );
}

fn print_manpage() {
    println!(".TH REWIND 1");
    println!(".SH NAME");
    println!("rewind \\- local time-travel tool for project directories");
    println!(".SH SYNOPSIS");
    println!(".B rewind");
    println!("[OPTIONS] <COMMAND>");
    println!(".SH DESCRIPTION");
    println!(
        "Rewind records command effects, stores content-addressed workspace snapshots, lets local project history be inspected and restored, and helps explain changes through optional traces, provenance, and replay."
    );
    println!(
        "It is a host-mode CLI, not a kernel, daemon, file watcher, GUI, or security sandbox."
    );
    println!(".SH COMMAND GROUPS");
    println!("Setup: init, migrate, repo-info, doctor, config show.");
    println!("Record: run, commit.");
    println!("Inspect: status, history, timeline, show, diff, log, cat, grep, deleted.");
    println!("Time travel: undo, restore, checkout, checkpoint.");
    println!("Explain: trace, explain, why, impact, graph, replay.");
    println!("Maintenance: verify, stats, gc, recover.");
    println!("Packaging: version, completions, man, env, self-test.");
    println!(".SH SAFETY NOTES");
    println!("The .rewind/ directory is always excluded from snapshots and restore plans.");
    println!("Read-only commands must not create events, snapshots, objects, journals, checkpoints, or head_snapshot updates.");
    println!("run refuses dirty worktrees unless --allow-dirty is explicit.");
    println!("undo, restore, and checkout are journaled and require clean worktrees.");
    println!("gc is dry-run by default; gc --yes deletes only unreachable Rewind storage.");
    println!("Replay is workspace-safe analysis, not a security sandbox for untrusted commands.");
    println!("Raw traces kept with --trace-keep-raw may contain absolute paths and sensitive process details.");
    println!("Legacy repositories require explicit migration with rewind migrate; ordinary commands do not silently migrate.");
    println!("Repository format 2 and DB schema 1 are the supported v1 RC storage contract.");
    println!(".SH EXAMPLES");
    println!("rewind init");
    println!("rewind run -- sh -c \"echo hello > notes.txt\"");
    println!("rewind status");
    println!("rewind restore notes.txt --before 2 --dry-run");
    println!("rewind replay 1 --compare");
    println!("rewind completions bash > rewind.bash");
    println!("rewind man > rewind.1");
}

fn print_environment(project_dir: &Path) -> Result<()> {
    let info = repo::repo_info(project_dir);

    println!("Rewind environment");
    println!();
    println!("CLI version:           {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Platform:              {} {}",
        env::consts::OS,
        env::consts::ARCH
    );
    println!(
        "Supported repo format: {}",
        repo::CURRENT_REPO_FORMAT_VERSION
    );
    println!("Supported DB schema:   {}", repo::CURRENT_DB_SCHEMA_VERSION);
    println!("Rust target:           {}", build_target());
    println!("Build profile:         {}", build_profile());
    println!("Git commit:            {}", git_commit());
    println!("Git dirty:             {}", git_dirty());
    println!("CWD:                   {}", project_dir.display());
    println!();
    println!("Repository:");
    println!(
        "  detected:            {}",
        yes_no(info.status.status != repo::RepoStatus::Uninitialized)
    );
    println!("  status:              {}", info.status.status.as_str());
    println!(
        "  repo format:         {}",
        info.status
            .manifest
            .as_ref()
            .map(|manifest| manifest.format_version.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "  DB schema:           {}",
        info.status
            .db_schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "  active journal:      {}",
        yes_no(info.status.active_journal)
    );
    println!(
        "  head snapshot:       {}",
        info.counts
            .as_ref()
            .and_then(|counts| counts.head_snapshot.as_deref())
            .unwrap_or("-")
    );
    if let Some(reason) = &info.status.reason {
        println!("  reason:              {reason}");
    }

    println!();
    println!("Config:");
    if project_dir.join(REWIND_DIR).is_dir() {
        match config::status(project_dir) {
            Ok(status) => {
                println!("  source:              {}", status.config_source);
                println!(
                    "  ignore enabled:      {}",
                    yes_no(status.config.ignore.enabled)
                );
                println!("  ignore file:         {}", status.config.ignore.file);
                println!(
                    "  ignore file exists:  {}",
                    yes_no(status.ignore_file_exists)
                );
                let ignore_status = if !status.config.ignore.enabled {
                    "disabled".to_owned()
                } else if status.ignore_file_exists {
                    "ok".to_owned()
                } else {
                    "none".to_owned()
                };
                println!("  ignore status:       {ignore_status}");
            }
            Err(error) => {
                println!("  status:              invalid");
                println!("  reason:              {error:#}");
            }
        }
    } else {
        println!("  status:              not in a Rewind repo");
    }

    println!();
    println!("Tools:");
    println!("  strace:              {}", strace_tool_status());
    println!(
        "  TERM:                {}",
        env::var("TERM").unwrap_or_else(|_| "-".to_owned())
    );
    println!(
        "  stdout tty:          {}",
        yes_no(io::stdout().is_terminal())
    );
    println!(
        "  stderr tty:          {}",
        yes_no(io::stderr().is_terminal())
    );
    Ok(())
}

fn run_self_test(keep: bool) -> Result<()> {
    let sandbox = tempfile::Builder::new()
        .prefix("rewind-self-test-")
        .tempdir()
        .context("creating self-test temp directory")?;
    let sandbox_path = sandbox.path().to_path_buf();
    let mut steps = Vec::new();

    init::init_project(&sandbox_path).context("self-test init")?;
    steps.push(("init", true));

    let command = self_test_command();
    match run::run_command(&sandbox_path, &command, false).context("self-test run")? {
        run::RunOutcome::Ran { exit_code: 0, .. } => steps.push(("run", true)),
        run::RunOutcome::Ran { exit_code, .. } => bail!("self-test command exited {exit_code}"),
        run::RunOutcome::Dirty { .. } => bail!("self-test worktree unexpectedly dirty before run"),
    }

    if status::worktree_status(&sandbox_path)?.is_clean() {
        steps.push(("status", true));
    } else {
        bail!("self-test status was dirty after run");
    }

    match restore::undo_latest_with_debug(&sandbox_path, false, transaction::DebugStop::None)
        .context("self-test undo")?
    {
        restore::UndoOutcome::Applied { .. } => steps.push(("undo", true)),
        restore::UndoOutcome::NothingToUndo => bail!("self-test had nothing to undo"),
        restore::UndoOutcome::Dirty { .. } => bail!("self-test undo found dirty worktree"),
        restore::UndoOutcome::DryRun { .. } => unreachable!("self-test undo is not dry-run"),
    }

    let report = integrity::verify(&sandbox_path).context("self-test verify")?;
    if report.errors.is_empty() {
        steps.push(("verify", true));
    } else {
        bail!("self-test verify reported {} error(s)", report.errors.len());
    }

    println!("Rewind self-test");
    println!();
    for (step, ok) in steps {
        println!("{step:<7} {}", if ok { "ok" } else { "failed" });
    }
    println!();
    println!("Result: ok");
    if keep {
        let kept = sandbox.keep();
        println!("Kept temp dir: {}", kept.display());
    } else {
        drop(sandbox);
    }
    Ok(())
}

fn self_test_command() -> Vec<String> {
    if cfg!(windows) {
        vec![
            "cmd".to_owned(),
            "/C".to_owned(),
            "echo hello>notes.txt".to_owned(),
        ]
    } else {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "echo hello > notes.txt".to_owned(),
        ]
    }
}

fn strace_tool_status() -> &'static str {
    if !cfg!(target_os = "linux") {
        return "unsupported on this platform";
    }
    match Command::new("strace").arg("-V").output() {
        Ok(output) if output.status.success() => "available",
        _ => "unavailable",
    }
}

fn build_target() -> &'static str {
    option_env!("REWIND_BUILD_TARGET").unwrap_or("unknown")
}

fn build_profile() -> &'static str {
    option_env!("REWIND_BUILD_PROFILE").unwrap_or("unknown")
}

fn git_commit() -> &'static str {
    option_env!("REWIND_GIT_COMMIT").unwrap_or("unknown")
}

fn git_dirty() -> &'static str {
    option_env!("REWIND_GIT_DIRTY").unwrap_or("unknown")
}

fn print_repo_info(project_dir: &Path) -> Result<()> {
    let info = repo::repo_info(project_dir);
    let config_status = config::status(project_dir);
    println!("Rewind repo info");
    println!();
    println!("Path:                 {}", info.rewind_dir.display());
    if let Some(manifest) = &info.status.manifest {
        println!("Repo ID:              {}", manifest.repo_id);
        println!("Format version:       {}", manifest.format_version);
        println!("DB schema version:    {}", manifest.db_schema_version);
        println!("Created at:           {}", manifest.created_at);
        println!("Created by:           {}", manifest.created_by_version);
        println!("Last migrated at:     {}", manifest.last_migrated_at);
        println!(
            "Last migrated by:     {}",
            manifest.last_migrated_by_version
        );
    } else {
        println!("Repo ID:              -");
        println!("Format version:       -");
        println!(
            "DB schema version:    {}",
            info.status
                .db_schema_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "-".to_owned())
        );
    }
    if let Some(counts) = &info.counts {
        println!(
            "Head snapshot:        {}",
            counts.head_snapshot.as_deref().unwrap_or("-")
        );
        println!(
            "Active journal:       {}",
            yes_no(info.status.active_journal)
        );
        println!("Events:               {}", counts.events);
        println!("Checkpoints:          {}", counts.checkpoints);
        println!("Snapshots:            {}", counts.snapshots);
        println!("Objects:              {}", counts.objects);
    } else {
        println!("Active journal:       no");
    }
    println!("App version:          {}", env!("CARGO_PKG_VERSION"));
    println!("Migration status:     {}", info.status.status.as_str());
    match config_status {
        Ok(config_status) => {
            println!("Config source:        {}", config_status.config_source);
            println!(
                "Ignore enabled:       {}",
                yes_no(config_status.config.ignore.enabled)
            );
            println!("Ignore file:          {}", config_status.config.ignore.file);
            println!(
                "Ignore file exists:   {}",
                yes_no(config_status.ignore_file_exists)
            );
        }
        Err(error) => println!("Config status:        invalid ({error:#})"),
    }
    if let Some(reason) = &info.status.reason {
        println!("Reason:               {reason}");
    }
    if info.status.status == repo::RepoStatus::NeedsMigration {
        println!("Suggested command:    rewind migrate");
    }
    Ok(())
}

fn handle_doctor(project_dir: &Path) -> Result<()> {
    let info = repo::repo_info(project_dir);
    let mut ok = info.status.status == repo::RepoStatus::Current;
    let mut verify_line = "skipped until migration".to_owned();
    let config_line = match config::status(project_dir) {
        Ok(status) => {
            if status.config.ignore.enabled && status.ignore_file_exists {
                format!("OK ({} rule(s))", status.ignore_rule_count)
            } else if status.config.ignore.enabled {
                "OK (defaults, no ignore file)".to_owned()
            } else {
                "OK (ignore disabled)".to_owned()
            }
        }
        Err(error) => {
            ok = false;
            format!("invalid: {error:#}")
        }
    };
    if info.status.active_journal {
        ok = false;
    }
    if ok {
        let report = integrity::verify(project_dir)?;
        if report.errors.is_empty() {
            verify_line = if report.warnings.is_empty() {
                "OK".to_owned()
            } else {
                format!("OK with {} warning(s)", report.warnings.len())
            };
        } else {
            ok = false;
            verify_line = format!("{} error(s)", report.errors.len());
        }
    }

    println!(
        "Rewind doctor: {}",
        if ok { "OK" } else { "attention needed" }
    );
    println!();
    println!("Repo format:        {}", info.status.status.as_str());
    println!(
        "DB schema:          {}",
        info.status
            .db_schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "legacy".to_owned())
    );
    println!(
        "Active journal:     {}",
        if info.status.active_journal {
            "present"
        } else {
            "none"
        }
    );
    println!("Verify:             {verify_line}");
    println!("Config:             {config_line}");
    if let Some(counts) = &info.counts {
        println!(
            "Head snapshot:      {}",
            counts.head_snapshot.as_deref().unwrap_or("-")
        );
        println!("Events:             {}", counts.events);
        println!("Checkpoints:        {}", counts.checkpoints);
    }
    if info.status.status == repo::RepoStatus::NeedsMigration {
        println!("Suggested action:   rewind migrate");
    } else if info.status.status == repo::RepoStatus::Invalid {
        println!("Suggested action:   inspect .rewind/repo.json or restore from backup");
    } else if info.status.status == repo::RepoStatus::IncompatibleFutureFormat {
        println!("Suggested action:   use a newer Rewind binary");
    }

    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn handle_migrate(project_dir: &Path, check: bool) -> Result<()> {
    let status = repo::inspect(project_dir);
    if check {
        match status.status {
            repo::RepoStatus::Current => {
                println!("This Rewind repo is current.");
                println!("No migration needed.");
                return Ok(());
            }
            repo::RepoStatus::NeedsMigration => {
                println!("This Rewind repo needs migration.");
                println!("Run: rewind migrate");
                std::process::exit(1);
            }
            repo::RepoStatus::IncompatibleFutureFormat => {
                println!("This Rewind repo uses a newer unsupported format.");
                println!(
                    "Supported format version: {}",
                    repo::CURRENT_REPO_FORMAT_VERSION
                );
                if let Some(manifest) = status.manifest {
                    println!("Repo format version: {}", manifest.format_version);
                }
                std::process::exit(1);
            }
            repo::RepoStatus::Invalid => {
                println!("This Rewind repo has invalid format metadata.");
                if let Some(reason) = status.reason {
                    println!("Reason: {reason}");
                }
                std::process::exit(1);
            }
            repo::RepoStatus::Uninitialized => {
                println!("This directory is not initialized; run `rewind init` first.");
                std::process::exit(1);
            }
        }
    }

    let summary = repo::migrate(project_dir)?;
    if !summary.changed {
        println!("Rewind repo is already current.");
        println!("No changes made.");
        return Ok(());
    }

    println!("Migrating Rewind repo...");
    println!();
    for step in &summary.steps {
        println!("- {step}");
    }
    println!();
    println!("Migration complete.");
    Ok(())
}

fn handle_config(project_dir: &Path, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => print_config(project_dir),
    }
}

fn print_config(project_dir: &Path) -> Result<()> {
    let status = config::status(project_dir)?;
    println!("Rewind config");
    println!();
    println!("Config file:        {}", config::CONFIG_PATH);
    println!("Config source:      {}", status.config_source);
    println!(
        "Ignore enabled:     {}",
        yes_no(status.config.ignore.enabled)
    );
    println!("Ignore file:        {}", status.config.ignore.file);
    println!("Ignore file exists: {}", yes_no(status.ignore_file_exists));
    println!(
        "Ignore rules:       {}",
        if !status.config.ignore.enabled {
            "disabled".to_owned()
        } else if !status.ignore_file_exists {
            "none".to_owned()
        } else {
            format!("parsed successfully ({} rule(s))", status.ignore_rule_count)
        }
    );
    Ok(())
}

fn handle_checkpoint(project_dir: &Path, command: CheckpointCommand) -> Result<()> {
    match command {
        CheckpointCommand::Create {
            force,
            name,
            message,
        } => match checkpoint::create_checkpoint(project_dir, &name, &message, force)? {
            checkpoint::CheckpointCreateOutcome::Created => {
                println!("created checkpoint {name}");
            }
            checkpoint::CheckpointCreateOutcome::Dirty { status } => {
                print_checkpoint_dirty_report(&status);
                std::process::exit(1);
            }
        },
        CheckpointCommand::List => print_checkpoints(project_dir)?,
        CheckpointCommand::Show { name } => print_checkpoint(project_dir, &name)?,
        CheckpointCommand::Delete { name } => {
            if checkpoint::delete_checkpoint(project_dir, &name)? {
                println!("deleted checkpoint {name}");
            } else {
                println!("checkpoint {name} not found");
            }
        }
    }
    Ok(())
}

fn handle_checkout(
    project_dir: &Path,
    checkpoint_name: Option<String>,
    before: Option<i64>,
    after: Option<i64>,
    snapshot: Option<String>,
    dry_run: bool,
    debug_stop: transaction::DebugStop,
) -> Result<()> {
    let target = resolve_checkout_target(project_dir, checkpoint_name, before, after, snapshot)?;
    let label = target.label();
    match checkout::checkout_with_debug(project_dir, target, dry_run, debug_stop)? {
        checkout::CheckoutOutcome::Applied { event_id, .. } => {
            println!("created checkout event {event_id}");
        }
        checkout::CheckoutOutcome::DryRun { plan } => {
            println!("Checkout plan for {label}:");
            print_would_restore_plan(&plan);
        }
        checkout::CheckoutOutcome::Dirty { status } => {
            print!("{}", status::dirty_report(&status));
            std::process::exit(1);
        }
        checkout::CheckoutOutcome::AlreadyAtTarget => {
            println!("worktree already at {label}");
        }
    }
    Ok(())
}

fn debug_stop(after_journal: bool, after_commit: bool) -> transaction::DebugStop {
    if after_journal {
        transaction::DebugStop::AfterJournal
    } else if after_commit {
        transaction::DebugStop::AfterCommit
    } else {
        transaction::DebugStop::None
    }
}

fn resolve_checkout_target(
    project_dir: &Path,
    checkpoint_name: Option<String>,
    before: Option<i64>,
    after: Option<i64>,
    snapshot: Option<String>,
) -> Result<checkout::CheckoutTarget> {
    let selected = checkpoint_name.is_some() as u8
        + before.is_some() as u8
        + after.is_some() as u8
        + snapshot.is_some() as u8;
    if selected != 1 {
        bail!("choose exactly one of --checkpoint, --before, --after, or --snapshot");
    }

    if let Some(name) = checkpoint_name {
        let checkpoint = checkpoint::get_checkpoint(project_dir, &name)?
            .ok_or_else(|| anyhow::anyhow!("checkpoint {name} not found"))?;
        return Ok(checkout::CheckoutTarget::Checkpoint {
            name,
            snapshot_id: checkpoint.snapshot_id,
        });
    }

    if let Some(event_id) = before {
        let conn = history::ensure_initialized(project_dir)?;
        let event = history::get_event(&conn, event_id)?
            .ok_or_else(|| anyhow::anyhow!("event {event_id} not found"))?;
        return Ok(checkout::CheckoutTarget::BeforeEvent {
            event_id,
            snapshot_id: event.before_snapshot,
        });
    }

    if let Some(event_id) = after {
        let conn = history::ensure_initialized(project_dir)?;
        let event = history::get_event(&conn, event_id)?
            .ok_or_else(|| anyhow::anyhow!("event {event_id} not found"))?;
        return Ok(checkout::CheckoutTarget::AfterEvent {
            event_id,
            snapshot_id: event.after_snapshot,
        });
    }

    let snapshot_id = snapshot.expect("exactly one checkout target checked");
    let snapshot_id = integrity::resolve_snapshot_prefix(project_dir, &snapshot_id)?;
    load_snapshot(project_dir, &snapshot_id)?;
    Ok(checkout::CheckoutTarget::Snapshot { snapshot_id })
}

fn handle_verify(project_dir: &Path, strict: bool) -> Result<()> {
    let report = integrity::verify(project_dir)?;
    let failed = !report.errors.is_empty() || (strict && !report.warnings.is_empty());

    if failed {
        println!("Rewind verify: problems found");
    } else {
        println!("Rewind verify: OK");
    }
    println!();
    print_integrity_issues("Errors", &report.errors);
    print_integrity_issues("Warnings", &report.warnings);
    if !report.errors.is_empty() || !report.warnings.is_empty() {
        println!();
    }
    print_verify_summary(&report);

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn print_stats(project_dir: &Path) -> Result<()> {
    let stats = integrity::stats(project_dir)?;

    println!("Repo:");
    println!(
        "  format version:   {}",
        stats
            .repo_format_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "  db schema:        {}",
        stats
            .db_schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!("  migration status: {}", stats.migration_status);
    println!("  config status:    {}", stats.config_status);
    println!("  ignore enabled:   {}", yes_no(stats.ignore_enabled));
    println!("  ignore rules:     {}", stats.ignore_rule_count);
    println!();
    println!("Events:");
    println!("  active journal: {}", yes_no(stats.active_journal));
    println!("  total:    {}", stats.event_count);
    for (kind, count) in &stats.event_counts_by_kind {
        println!("  {kind:<8} {count}", kind = format!("{kind}:"));
    }
    println!();
    println!("Snapshots:");
    println!("  reachable:    {}", stats.reachable_snapshots.len());
    println!("  unreferenced: {}", stats.unreferenced_snapshots.len());
    println!("  head:         {}", stats.head_snapshot);
    if !stats.manifest_versions.is_empty() {
        println!("  manifest versions:");
        for (version, count) in &stats.manifest_versions {
            println!("    v{version}: {count}");
        }
    }
    println!();
    println!("Entries:");
    println!("  symlinks:         {}", stats.symlink_entries);
    println!("  executable files: {}", stats.executable_file_entries);
    println!();
    println!("Objects:");
    println!("  reachable:          {}", stats.reachable_objects.len());
    println!(
        "  reachable bytes:    {}",
        format_bytes(stats.reachable_object_bytes)
    );
    println!(
        "  unreferenced:        {}",
        stats.unreferenced_objects.len()
    );
    println!(
        "  reclaimable bytes:   {}",
        format_bytes(stats.unreferenced_object_bytes)
    );
    println!();
    println!("Checkpoints:");
    println!("  total: {}", stats.checkpoint_count);
    println!();
    println!("Traces:");
    println!("  total:        {}", stats.trace_stats.total);
    println!("  captured:     {}", stats.trace_stats.captured);
    println!("  unavailable:  {}", stats.trace_stats.unavailable);
    println!("  failed:       {}", stats.trace_stats.failed);
    println!("  parse_error:  {}", stats.trace_stats.parse_error);
    println!("  file events:  {}", stats.trace_stats.file_events);
    println!("  process ops:  {}", stats.trace_stats.process_events);
    let provenance = provenance::provenance_stats(project_dir)?;
    println!();
    println!("Provenance:");
    println!(
        "  traced events with file access: {}",
        provenance.traced_events_with_file_access
    );
    println!(
        "  paths with trace access:        {}",
        provenance.paths_with_trace_access
    );
    println!(
        "  trace + final changes:          {}",
        provenance.events_with_trace_and_changes
    );
    println!(
        "  final changes without trace:    {}",
        provenance.events_with_changes_but_no_trace
    );
    let replay = replay::replay_stats(project_dir)?;
    println!();
    println!("Replay:");
    println!("  run events:       {}", replay.run_events);
    println!("  exact argv:       {}", replay.exact_argv);
    println!("  legacy fallback:  {}", replay.legacy_fallback);
    println!("  unsupported:      {}", replay.unsupported);
    Ok(())
}

fn handle_gc(project_dir: &Path, yes: bool) -> Result<()> {
    if yes {
        transaction::ensure_no_active(project_dir)?;
    }
    let (report, plan) = integrity::gc_plan(project_dir)?;
    if !report.errors.is_empty() {
        println!("refusing garbage collection: verification errors found");
        print_integrity_issues("Errors", &report.errors);
        std::process::exit(1);
    }

    if !yes {
        println!("Rewind garbage collection dry run.");
        println!();
        print_gc_plan(&plan);
        println!();
        println!("No files were deleted. Re-run with:");
        println!("  rewind gc --yes");
        return Ok(());
    }

    integrity::apply_gc(project_dir, &plan)?;
    println!("Rewind garbage collection complete.");
    println!();
    println!("Removed snapshots: {}", plan.snapshots.len());
    println!("Removed objects:   {}", plan.objects.len());
    println!(
        "Reclaimed:         {}",
        format_bytes(plan.reclaimable_bytes)
    );
    Ok(())
}

fn handle_recover(project_dir: &Path, _status: bool, complete: bool, abort: bool) -> Result<()> {
    if complete {
        match transaction::complete(project_dir)? {
            transaction::RecoverOutcome::NoActiveTransaction => {
                println!("No active Rewind transaction.");
            }
            transaction::RecoverOutcome::Completed { id } => {
                println!("Rewind transaction completed.");
                println!("ID: {id}");
            }
            transaction::RecoverOutcome::Aborted { .. } => unreachable!(),
        }
        return Ok(());
    }

    if abort {
        match transaction::abort(project_dir)? {
            transaction::RecoverOutcome::NoActiveTransaction => {
                println!("No active Rewind transaction.");
            }
            transaction::RecoverOutcome::Aborted { id } => {
                println!("Rewind transaction aborted.");
                println!("ID: {id}");
            }
            transaction::RecoverOutcome::Completed { .. } => unreachable!(),
        }
        return Ok(());
    }

    print_recovery_status(project_dir)
}

fn warn_active_transaction(project_dir: &Path) {
    if transaction::has_active(project_dir) {
        eprintln!(
            "Warning: active recovery transaction present. Results may reflect an in-progress restore. Run rewind recover --status."
        );
    }
}

fn print_path_history(
    project_dir: &Path,
    path: &str,
    limit: Option<usize>,
    include_trace: bool,
) -> Result<()> {
    let entries = forensics::path_history(project_dir, path, limit)?;
    let trace_entries = if include_trace {
        trace::trace_file_touches_for_path(project_dir, path)?
    } else {
        Vec::new()
    };
    if entries.is_empty() && trace_entries.is_empty() {
        println!("No history found for {path}.");
        return Ok(());
    }

    println!("Path history for {path}");
    println!();
    println!(
        "{:<4}{:<21}{:<9}{:<10}{:<8}{:<7}{:<8}COMMAND",
        "ID", "TIME", "KIND", "CHANGE", "STATE", "DIRTY", "SOURCE"
    );
    for entry in entries {
        println!(
            "{:<4}{:<21}{:<9}{:<10}{:<8}{:<7}{:<8}{}",
            entry.event_id,
            display_time(&entry.timestamp),
            entry.kind,
            entry.change_type,
            if entry.undone { "undone" } else { "active" },
            yes_no(entry.started_dirty),
            "diff",
            entry.command
        );
        if entry.path != path {
            println!("    {}", entry.path);
        }
    }
    if include_trace {
        let conn = history::ensure_initialized(project_dir)?;
        for (event_id, operation, touched_path) in trace_entries {
            let Some(event) = history::get_event(&conn, event_id)? else {
                continue;
            };
            println!(
                "{:<4}{:<21}{:<9}{:<10}{:<8}{:<7}{:<8}{}",
                event.id,
                display_time(&event.timestamp),
                event.kind,
                "touched",
                if event.undone { "undone" } else { "active" },
                yes_no(event.started_dirty),
                "trace",
                event.command
            );
            println!("    {operation} {touched_path}");
        }
    }
    println!();
    println!("Suggested commands:");
    println!("  rewind why {path}");
    println!("  rewind impact {path}");
    Ok(())
}

fn print_cat(
    project_dir: &Path,
    path: &str,
    before: Option<i64>,
    after: Option<i64>,
    snapshot: Option<String>,
    checkpoint: Option<String>,
    raw: bool,
) -> Result<()> {
    let selected = before.is_some() as u8
        + after.is_some() as u8
        + snapshot.is_some() as u8
        + checkpoint.is_some() as u8;
    if selected != 1 {
        bail!("choose exactly one of --before, --after, --snapshot, or --checkpoint");
    }
    let target = if let Some(event_id) = before {
        forensics::CatTarget::BeforeEvent(event_id)
    } else if let Some(event_id) = after {
        forensics::CatTarget::AfterEvent(event_id)
    } else if let Some(snapshot) = snapshot {
        forensics::CatTarget::Snapshot(snapshot)
    } else {
        forensics::CatTarget::Checkpoint(checkpoint.expect("selector checked"))
    };

    let file = forensics::cat_file(project_dir, path, target)?;
    if raw {
        io::stdout().write_all(&file.bytes)?;
        return Ok(());
    }
    let text = String::from_utf8(file.bytes)
        .map_err(|_| anyhow::anyhow!("File is binary or non-UTF8; use --raw"))?;
    print!("{text}");
    Ok(())
}

fn print_deleted(project_dir: &Path, path: Option<&str>, limit: Option<usize>) -> Result<()> {
    let entries = forensics::deleted_files(project_dir, path, limit)?;
    if entries.is_empty() {
        println!("No deleted files found in Rewind history.");
        return Ok(());
    }

    println!("Deleted files known to Rewind:");
    println!();
    println!("{:<24}{:<18}SUGGESTED RESTORE", "PATH", "DELETED BY EVENT");
    for entry in entries {
        println!(
            "{:<24}{:<18}{}",
            entry.path,
            entry
                .deleted_by_event_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            entry.suggested_restore.unwrap_or_else(|| "-".to_owned())
        );
    }
    Ok(())
}

fn print_grep(
    project_dir: &Path,
    pattern: &str,
    snapshot: Option<String>,
    checkpoint: Option<String>,
    history: bool,
    ignore_case: bool,
    max_results: usize,
) -> Result<()> {
    let selected = snapshot.is_some() as u8 + checkpoint.is_some() as u8 + history as u8;
    if selected != 1 {
        bail!("choose exactly one of --snapshot, --checkpoint, or --history");
    }
    let target = if let Some(snapshot) = snapshot {
        forensics::GrepTarget::Snapshot(snapshot)
    } else if let Some(checkpoint) = checkpoint {
        forensics::GrepTarget::Checkpoint(checkpoint)
    } else {
        forensics::GrepTarget::History
    };
    let result = forensics::grep(
        project_dir,
        pattern,
        target,
        forensics::GrepOptions {
            ignore_case,
            max_results,
            ..Default::default()
        },
    )?;

    for item in result.matches {
        if history {
            println!(
                "{} {}:{}: {}",
                short_snapshot(&item.snapshot_id),
                item.path,
                item.line_number,
                item.line
            );
        } else {
            println!("{}:{}: {}", item.path, item.line_number, item.line);
        }
    }
    if result.limit_reached {
        println!("Result limit reached. Re-run with --max-results <n>.");
    }
    Ok(())
}

fn print_trace(
    project_dir: &Path,
    event_id: i64,
    files_only: bool,
    processes_only: bool,
    summary_only: bool,
) -> Result<()> {
    if [files_only, processes_only, summary_only]
        .into_iter()
        .filter(|value| *value)
        .count()
        > 1
    {
        bail!("choose at most one of --files, --processes, or --summary");
    }

    let conn = history::ensure_initialized(project_dir)?;
    let event = history::get_event(&conn, event_id)?
        .ok_or_else(|| anyhow::anyhow!("event {event_id} not found"))?;
    let Some(details) = trace::trace_details(project_dir, event_id)? else {
        println!("No trace recorded for event {event_id}.");
        return Ok(());
    };

    println!("Trace for event {event_id}");
    println!("Command: {}", event.command);
    println!("Tracer:  {}", details.trace.tracer);
    println!("Status:  {}", details.trace.status);
    if let Some(error) = &details.trace.parse_error {
        println!("Detail:  {error}");
    }
    println!();

    if !files_only && !processes_only {
        println!("Summary:");
        println!("  process ops: {}", details.processes.len());
        println!("  file ops: {}", details.files.len());
        println!(
            "  outside workspace ops: {}",
            details.trace.outside_workspace_ops
        );
        let mut touched = details
            .files
            .iter()
            .filter(|event| event.within_workspace)
            .filter_map(|event| event.path.as_deref())
            .collect::<Vec<_>>();
        touched.sort_unstable();
        touched.dedup();
        if !touched.is_empty() {
            println!();
            println!("Touched paths:");
            for path in touched {
                println!("  {path}");
            }
        }
    }

    if summary_only {
        return Ok(());
    }

    if !processes_only {
        println!();
        println!("Workspace file operations:");
        for event in details.files.iter().filter(|event| event.within_workspace) {
            let path = event.path.as_deref().unwrap_or("-");
            let result = event
                .errno
                .as_deref()
                .or(event.result.as_deref())
                .unwrap_or("-");
            println!("  {:<10} {:<24} {}", event.operation, path, result);
            if let Some(path2) = &event.path2 {
                println!("             -> {path2}");
            }
        }
    }

    if !files_only {
        println!();
        println!("Processes:");
        for event in details.processes {
            let pid = event
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let executable = event.executable.unwrap_or_else(|| "-".to_owned());
            let result = event.result.unwrap_or_else(|| "-".to_owned());
            println!(
                "  pid {pid:<8} {:<10} {:<16} {result}",
                event.operation, executable
            );
        }
    }

    println!();
    println!("Suggested:");
    println!("  rewind explain {event_id}");

    Ok(())
}

fn print_explain(project_dir: &Path, event_id: i64, summary: bool) -> Result<()> {
    let explanation = provenance::explain_event(project_dir, event_id)?;
    let event = &explanation.event;
    println!("Event {event_id} explanation");
    println!();
    println!("Kind:      {}", event.kind);
    println!("Command:   {}", event.command);
    println!(
        "State:     {}",
        if event.undone { "undone" } else { "active" }
    );
    println!("Time:      {}", event.timestamp);
    println!("Started dirty: {}", yes_no(event.started_dirty));
    if let Some(transaction_id) = &event.transaction_id {
        println!("Transaction: {transaction_id}");
    }
    println!(
        "Snapshots: {} -> {}",
        short_snapshot(&event.before_snapshot),
        short_snapshot(&event.after_snapshot)
    );
    println!();
    println!("Final changes:");
    print_change_group("Created", &explanation.diff.changes, ChangeType::Created);
    print_change_group("Modified", &explanation.diff.changes, ChangeType::Modified);
    print_change_group("Deleted", &explanation.diff.changes, ChangeType::Deleted);
    print_string_group("Created directories", &explanation.diff.added_dirs);
    print_string_group("Deleted directories", &explanation.diff.deleted_dirs);

    println!();
    match &explanation.trace {
        Some(trace) => {
            println!("Trace:");
            println!("  Status: {}", trace.trace.status);
            println!("  Tracer: {}", trace.trace.tracer);
            println!("  File ops: {}", trace.files.len());
            println!("  Process ops: {}", trace.processes.len());
            println!(
                "  Outside workspace ops: {}",
                trace.trace.outside_workspace_ops
            );
            if !summary {
                print_process_summary(trace);
                print_workspace_access(trace);
            }
        }
        None => println!("Trace: missing; final changes are still shown from snapshots."),
    }

    println!();
    println!("Correlation:");
    print_string_group(
        "Changed and traced",
        &explanation.correlation.changed_and_traced,
    );
    print_string_group(
        "Traced but unchanged",
        &explanation.correlation.traced_but_unchanged,
    );
    print_string_group(
        "Changed but not traced",
        &explanation.correlation.changed_but_not_traced,
    );
    if explanation.correlation.changed_and_traced.is_empty()
        && explanation.correlation.traced_but_unchanged.is_empty()
        && explanation.correlation.changed_but_not_traced.is_empty()
    {
        println!("  none");
    }

    println!();
    println!("Suggested commands:");
    println!("  rewind diff {event_id}");
    println!("  rewind trace {event_id}");
    for change in &explanation.diff.changes {
        println!("  rewind log {}", change.path);
        println!("  rewind cat {} --before {event_id}", change.path);
        println!("  rewind cat {} --after {event_id}", change.path);
    }
    Ok(())
}

fn print_why(project_dir: &Path, path: &str) -> Result<()> {
    let why = provenance::why_path(project_dir, path)?;
    println!("Why {}?", why.path);
    println!();
    match &why.current_state {
        provenance::PathState::File { hash, size } => {
            println!("Current state: file at HEAD");
            println!("Current hash:  {hash}");
            println!("Size:          {size} bytes");
        }
        provenance::PathState::Directory => println!("Current state: directory at HEAD"),
        provenance::PathState::Missing => println!("Current state: missing at HEAD"),
    }
    println!();

    let Some(change) = &why.last_change else {
        println!("No event explains {}.", why.path);
        return Ok(());
    };
    if matches!(why.current_state, provenance::PathState::Missing)
        && change.change_type == "deleted"
    {
        println!("Last known deletion:");
        println!("  Event:   {}", change.event_id);
        println!("  Kind:    {}", change.kind);
        println!("  Command: {}", change.command);
    } else {
        println!("Last changed by event {}", change.event_id);
        println!("Kind:    {}", change.kind);
        println!("Command: {}", change.command);
        println!("Change:  {}", change.change_type);
    }
    if !why.trace_accesses.is_empty() {
        println!();
        println!("Trace:");
        for access in &why.trace_accesses {
            println!(
                "  {} was {} by traced command ({})",
                why.path,
                access_word(&access.access_kind),
                access.operation
            );
        }
    }
    println!();
    println!("Suggested commands:");
    println!("  rewind explain {}", change.event_id);
    println!("  rewind diff {}", change.event_id);
    println!("  rewind log {}", why.path);
    println!("  rewind cat {} --before {}", why.path, change.event_id);
    println!(
        "  rewind restore {} --before {} --dry-run",
        why.path, change.event_id
    );
    Ok(())
}

fn print_impact(
    project_dir: &Path,
    path: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<()> {
    let impact = provenance::impact_path(project_dir, path, since, until)?;
    println!("Trace-based impact for {}", impact.path);
    println!();
    if impact.entries.is_empty() {
        println!("No trace access found for {}.", impact.path);
    } else {
        println!(
            "{:<4}{:<21}{:<10}{:<13}COMMAND",
            "ID", "TIME", "ACCESS", "FINAL CHANGE"
        );
        for entry in impact.entries {
            println!(
                "{:<4}{:<21}{:<10}{:<13}{}",
                entry.event_id,
                display_time(&entry.timestamp),
                entry.access_kind,
                entry.final_change.unwrap_or_else(|| "no".to_owned()),
                entry.command
            );
        }
    }
    println!();
    println!("Trace-based results only. Untraced events may be missing.");
    Ok(())
}

fn print_graph(project_dir: &Path, event_id: i64, dot: bool) -> Result<()> {
    let explanation = provenance::explain_event(project_dir, event_id)?;
    if dot {
        print_graph_dot(&explanation);
    } else {
        print_graph_text(&explanation);
    }
    Ok(())
}

fn print_graph_text(explanation: &provenance::ProvenanceEvent) {
    println!(
        "Event {}: {}",
        explanation.event.id, explanation.event.command
    );
    println!("|");
    println!("+-- Processes");
    if let Some(trace) = &explanation.trace {
        for process in &trace.processes {
            println!(
                "|   +-- {}",
                process.executable.as_deref().unwrap_or(&process.operation)
            );
        }
    } else {
        println!("|   +-- trace graph unavailable");
    }
    println!("|");
    println!("+-- Inputs / reads");
    if let Some(trace) = &explanation.trace {
        for path in access_paths(trace, "read") {
            println!("|   +-- {path}");
        }
    }
    println!("|");
    println!("+-- Outputs / final changes");
    for change in &explanation.diff.changes {
        println!("|   +-- {} {}", change.path, change.change_type.as_str());
    }
    println!("|");
    println!("+-- Touched but unchanged");
    for path in &explanation.correlation.traced_but_unchanged {
        println!("    +-- {path}");
    }
}

fn print_graph_dot(explanation: &provenance::ProvenanceEvent) {
    let event_id = explanation.event.id;
    println!("digraph rewind_event_{event_id} {{");
    println!("  rankdir=LR;");
    println!(
        "  event_{event_id} [label=\"{}\", shape=oval];",
        dot_escape(&format!("event {event_id}\\n{}", explanation.event.kind))
    );
    if let Some(trace) = &explanation.trace {
        for (index, process) in trace.processes.iter().enumerate() {
            let process_id = format!("process_{index}");
            println!(
                "  {process_id} [label=\"{}\", shape=box];",
                dot_escape(process.executable.as_deref().unwrap_or(&process.operation))
            );
            println!("  event_{event_id} -> {process_id};");
        }
        for access in trace.files.iter().filter(|access| access.within_workspace) {
            let Some(path) = &access.path else {
                continue;
            };
            let node = dot_node_id(path);
            println!("  {node} [label=\"{}\", shape=note];", dot_escape(path));
            if access.access_kind == "read" || access.access_kind == "metadata" {
                println!(
                    "  {node} -> event_{event_id} [label=\"{}\"];",
                    access.access_kind
                );
            } else {
                println!(
                    "  event_{event_id} -> {node} [label=\"{}\"];",
                    access.access_kind
                );
            }
        }
    }
    for change in &explanation.diff.changes {
        let node = dot_node_id(&change.path);
        println!(
            "  {node} [label=\"{}\", shape=note];",
            dot_escape(&format!(
                "{}\\n{}",
                change.path,
                change.change_type.as_str()
            ))
        );
        println!(
            "  event_{event_id} -> {node} [label=\"final_change:{}\"];",
            change.change_type.as_str()
        );
    }
    println!("}}");
}

fn handle_replay(
    project_dir: &Path,
    event_id: i64,
    dry_run: bool,
    sandbox: bool,
    compare: bool,
    keep: bool,
) -> Result<()> {
    let selected = dry_run as u8 + sandbox as u8 + compare as u8;
    if selected > 1 {
        bail!("choose at most one of --dry-run, --sandbox, or --compare");
    }
    if keep && !(sandbox || compare) {
        bail!("--keep is only valid with --sandbox or --compare");
    }

    let mode = if sandbox {
        replay::ReplayMode::Sandbox
    } else if compare {
        replay::ReplayMode::Compare
    } else {
        replay::ReplayMode::DryRun
    };

    match mode {
        replay::ReplayMode::DryRun => {
            let plan = replay::plan(project_dir, event_id, keep, false)?;
            print_replay_plan(&plan);
        }
        replay::ReplayMode::Sandbox | replay::ReplayMode::Compare => {
            let outcome = replay::replay(project_dir, event_id, mode, keep)?;
            print_replay_outcome(&outcome, mode == replay::ReplayMode::Compare);
        }
    }
    Ok(())
}

fn print_replay_plan(plan: &replay::ReplayPlan) {
    if let Some(warning) = &plan.active_journal_warning {
        eprintln!("{warning}");
    }
    println!("Replay plan for event {}", plan.event_id);
    println!();
    println!("Kind:          run");
    println!("Command:       {}", plan.command);
    println!("Replay source: {}", plan.source.label());
    println!("Working dir:   {}", plan.working_dir);
    println!("Original exit: {}", plan.original_exit_code);
    println!(
        "Snapshots:     {} -> {}",
        short_snapshot(&plan.before_snapshot),
        short_snapshot(&plan.after_snapshot)
    );
    println!("Keep sandbox:  {}", yes_no(plan.keep_sandbox));
    println!("Detailed compare: {}", yes_no(plan.detailed_compare));
    if matches!(plan.source, replay::ReplaySource::LegacyShellFallback(_)) {
        println!("Warning: legacy event has no exact argv; replay uses shell fallback.");
    }
    println!();
    println!("This replay would:");
    println!("- create a temporary sandbox");
    println!(
        "- restore snapshot {} into sandbox/workspace",
        plan.before_snapshot
    );
    println!("- run the command in sandbox/workspace");
    println!(
        "- compare the replay result to snapshot {}",
        plan.after_snapshot
    );
    println!(
        "- {} the sandbox afterward",
        if plan.keep_sandbox { "keep" } else { "delete" }
    );
}

fn print_replay_outcome(outcome: &replay::ReplayOutcome, detailed: bool) {
    if let Some(warning) = &outcome.plan.active_journal_warning {
        eprintln!("{warning}");
    }
    println!("Replay result for event {}", outcome.plan.event_id);
    println!();
    println!("Replay source: {}", outcome.plan.source.label());
    println!("Original exit: {}", outcome.plan.original_exit_code);
    println!("Replay exit:   {}", outcome.exit_code);
    println!(
        "Exit match:    {}",
        yes_no(outcome.comparison.exit_code_match)
    );
    println!(
        "Filesystem match: {}",
        yes_no(outcome.comparison.filesystem_match)
    );
    println!(
        "Exact tree id match: {}",
        yes_no(outcome.comparison.original_tree_id == outcome.comparison.replay_tree_id)
    );
    println!("Stdout bytes:  {}", outcome.stdout_bytes);
    println!("Stderr bytes:  {}", outcome.stderr_bytes);
    println!();
    print_replay_group(
        "Only in original after snapshot",
        &outcome.comparison.only_in_original,
    );
    print_replay_group("Only in replay", &outcome.comparison.only_in_replay);
    print_replay_group("Content mismatches", &outcome.comparison.content_mismatches);
    print_replay_group(
        "File-vs-directory mismatches",
        &outcome.comparison.kind_mismatches,
    );
    if detailed {
        for diff in &outcome.comparison.text_diffs {
            println!("--- {} original", diff.path);
            println!("+++ {} replay", diff.path);
            for line in &diff.lines {
                println!("{line}");
            }
        }
    }
    println!("Summary:");
    println!(
        "  exact reproduction: {}",
        yes_no(outcome.comparison.exact_match)
    );
    if let Some(artifacts) = &outcome.artifacts {
        println!();
        println!("Sandbox kept:");
        println!("  root:      {}", artifacts.sandbox_root.display());
        println!("  workspace: {}", artifacts.workspace_root.display());
        println!("  stdout:    {}", artifacts.stdout_path.display());
        println!("  stderr:    {}", artifacts.stderr_path.display());
    }
}

fn print_replay_group(title: &str, paths: &[String]) {
    println!("{title}:");
    if paths.is_empty() {
        println!("  none");
    } else {
        for path in paths {
            println!("  {path}");
        }
    }
    println!();
}

fn print_process_summary(trace: &trace::TraceDetails) {
    if trace.processes.is_empty() {
        return;
    }
    println!();
    println!("Processes:");
    for process in &trace.processes {
        println!(
            "  {}",
            process.executable.as_deref().unwrap_or(&process.operation)
        );
    }
}

fn print_workspace_access(trace: &trace::TraceDetails) {
    println!();
    println!("Workspace access:");
    print_string_group("Read", &access_paths(trace, "read"));
    let mut written = access_paths(trace, "write");
    written.extend(access_paths(trace, "create"));
    print_string_group("Wrote", &written);
    print_string_group("Deleted", &access_paths(trace, "delete"));
    print_string_group("Renamed", &access_paths(trace, "rename"));
    print_string_group("Metadata-only", &access_paths(trace, "metadata"));
}

fn access_paths(trace: &trace::TraceDetails, access_kind: &str) -> Vec<String> {
    let mut paths = trace
        .files
        .iter()
        .filter(|access| access.within_workspace)
        .filter(|access| access.access_kind == access_kind)
        .filter_map(|access| access.path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn access_word(access_kind: &str) -> &'static str {
    match access_kind {
        "read" => "read",
        "write" | "create" => "written",
        "delete" => "deleted",
        "rename" => "renamed",
        "metadata" => "inspected",
        _ => "touched",
    }
}

fn dot_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn dot_node_id(path: &str) -> String {
    let mut value = String::from("file_");
    for ch in path.chars() {
        if ch.is_ascii_alphanumeric() {
            value.push(ch);
        } else {
            value.push('_');
        }
    }
    value
}

fn print_recovery_status(project_dir: &Path) -> Result<()> {
    match transaction::recovery_status(project_dir)? {
        transaction::RecoveryStatus::NoActiveTransaction => {
            println!("No active Rewind transaction.");
        }
        transaction::RecoveryStatus::Active(journal) => {
            println!("Active Rewind transaction found.");
            println!();
            println!("ID:        {}", journal.id);
            println!("Operation: {}", journal.operation);
            println!("Command:   {}", journal.command);
            println!("Phase:     {:?}", journal.phase);
            println!("Old HEAD:  {}", journal.old_head_snapshot);
            println!("Target:    {}", journal.target_snapshot);
            println!(
                "Affected:  {} dirs create, {} dirs remove, {} files write, {} files remove",
                journal.restore_plan.create_dirs.len(),
                journal.restore_plan.remove_dirs.len(),
                journal.restore_plan.write_files.len(),
                journal.restore_plan.remove_files.len()
            );
            println!();
            println!("Recovery options:");
            println!("  rewind recover --complete");
            println!("  rewind recover --abort");
        }
    }
    Ok(())
}

fn handle_tui(project_dir: &Path, once: bool, selected: Option<i64>) -> Result<()> {
    if once {
        let model = rewind_core::tui_model::build_model(project_dir, selected)?;
        print!("{}", render_tui_once(&model));
        return Ok(());
    }

    run_interactive_tui(project_dir, selected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiPanel {
    Changes,
    Checkpoints,
    Stats,
    Help,
}

impl TuiPanel {
    fn next(self) -> Self {
        match self {
            Self::Changes => Self::Checkpoints,
            Self::Checkpoints => Self::Stats,
            Self::Stats => Self::Help,
            Self::Help => Self::Changes,
        }
    }
}

fn run_interactive_tui(project_dir: &Path, selected: Option<i64>) -> Result<()> {
    enable_raw_mode().context("enabling terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let result = run_tui_loop(project_dir, selected, &mut terminal);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn run_tui_loop(
    project_dir: &Path,
    selected: Option<i64>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let mut selected_id = selected;
    let mut panel = TuiPanel::Changes;
    let mut model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
    selected_id = model.selected_event_id;

    loop {
        terminal
            .draw(|frame| render_tui_frame(frame, &model, panel))
            .context("drawing terminal UI")?;

        if !event::poll(Duration::from_millis(250)).context("polling terminal input")? {
            continue;
        }

        let TerminalEvent::Key(key) = event::read().context("reading terminal input")? else {
            continue;
        };

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('?') => panel = TuiPanel::Help,
            KeyCode::Tab => panel = panel.next(),
            KeyCode::Char('r') => {
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
                selected_id = model.selected_event_id;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected_id = move_selection(&model.events, selected_id, 1);
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                selected_id = move_selection(&model.events, selected_id, -1);
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
            }
            KeyCode::PageDown => {
                selected_id = move_selection(&model.events, selected_id, 10);
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
            }
            KeyCode::PageUp => {
                selected_id = move_selection(&model.events, selected_id, -10);
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
            }
            KeyCode::Home => {
                selected_id = model.events.first().map(|event| event.id);
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
            }
            KeyCode::End => {
                selected_id = model.events.last().map(|event| event.id);
                model = rewind_core::tui_model::build_model(project_dir, selected_id)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn move_selection(events: &[TuiEvent], selected_id: Option<i64>, delta: isize) -> Option<i64> {
    if events.is_empty() {
        return None;
    }
    let current = selected_id
        .and_then(|id| events.iter().position(|event| event.id == id))
        .unwrap_or(events.len() - 1);
    let next = (current as isize + delta).clamp(0, events.len() as isize - 1) as usize;
    Some(events[next].id)
}

fn render_tui_frame(frame: &mut Frame<'_>, model: &TuiModel, panel: TuiPanel) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(10),
        ])
        .split(frame.area());
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(outer[1]);

    frame.render_widget(
        Paragraph::new(status_bar(model)).block(Block::default().borders(Borders::ALL)),
        outer[0],
    );
    frame.render_widget(timeline_widget(model), main[0]);
    frame.render_widget(event_details_widget(model), main[1]);
    frame.render_widget(bottom_panel_widget(model, panel), outer[2]);
}

fn timeline_widget(model: &TuiModel) -> List<'_> {
    let items = model
        .events
        .iter()
        .map(|event| {
            let marker = if Some(event.id) == model.selected_event_id {
                ">"
            } else {
                " "
            };
            ListItem::new(format!(
                "{marker} {} {} {} trace:{}",
                event.id,
                event.kind,
                event_state(event),
                event.trace_status.as_deref().unwrap_or("none")
            ))
        })
        .collect::<Vec<_>>();
    List::new(items).block(Block::default().title("Timeline").borders(Borders::ALL))
}

fn event_details_widget(model: &TuiModel) -> Paragraph<'_> {
    Paragraph::new(selected_event_lines(model).join("\n"))
        .block(
            Block::default()
                .title("Event Details")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false })
}

fn bottom_panel_widget(model: &TuiModel, panel: TuiPanel) -> Paragraph<'_> {
    let (title, lines) = match panel {
        TuiPanel::Changes => ("Changes / Diff Preview", changes_panel_lines(model)),
        TuiPanel::Checkpoints => ("Checkpoints", checkpoint_panel_lines(model)),
        TuiPanel::Stats => ("Stats", stats_panel_lines(model)),
        TuiPanel::Help => ("Help", help_lines()),
    };
    Paragraph::new(lines.join("\n"))
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
}

fn render_tui_once(model: &TuiModel) -> String {
    let mut lines = Vec::new();
    lines.push(format!("HEAD: {}", model.head_snapshot));
    lines.push(format!(
        "Repo: format {} / schema {} ({})",
        model
            .stats
            .repo_format_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        model
            .stats
            .db_schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        model.stats.migration_status
    ));
    lines.push(format!(
        "Ignore: {} ({} rule(s))",
        if model.stats.ignore_enabled {
            "on"
        } else {
            "off"
        },
        model.stats.ignore_rule_count
    ));
    lines.push(format!("Worktree: {}", worktree_label(model)));
    if model.recovery_needed {
        lines.push("Recovery: active transaction".to_owned());
        lines.push("  rewind recover --status".to_owned());
        lines.push("  rewind recover --complete".to_owned());
        lines.push("  rewind recover --abort".to_owned());
    }
    lines.push("Verify: not run".to_owned());
    lines.push(String::new());
    lines.push("Timeline:".to_owned());
    if model.events.is_empty() {
        lines.push("  No events recorded.".to_owned());
    } else {
        for event in &model.events {
            let marker = if Some(event.id) == model.selected_event_id {
                ">"
            } else {
                " "
            };
            lines.push(format!(
                "{marker} {} {} {} trace:{}",
                event.id,
                event.kind,
                event_state(event),
                event.trace_status.as_deref().unwrap_or("none")
            ));
        }
    }
    lines.push(String::new());
    lines.push("Selected Event:".to_owned());
    lines.extend(selected_event_lines(model));
    lines.push(String::new());
    lines.push("Changes:".to_owned());
    lines.extend(changes_panel_lines(model));
    lines.push(String::new());
    lines.extend(checkpoint_panel_lines(model));
    lines.push(String::new());
    lines.push("Stats:".to_owned());
    lines.extend(stats_panel_lines(model));
    lines.push(String::new());
    lines.push("Help:".to_owned());
    lines.extend(help_lines());
    lines.push(String::new());
    lines.join("\n")
}

fn selected_event_lines(model: &TuiModel) -> Vec<String> {
    let Some(selected) = &model.selected_event else {
        return vec!["No event selected.".to_owned()];
    };
    let event = &selected.event;
    let mut lines = vec![
        format!("ID: {}", event.id),
        format!("Kind: {}", event.kind),
        format!("Command: {}", event.command),
        format!("Trace: {}", event.trace_status.as_deref().unwrap_or("none")),
        format!("Started dirty: {}", yes_no(event.started_dirty)),
        format!("Before: {}", event.before_snapshot),
        format!("After: {}", event.after_snapshot),
        format!("State: {}", event_state(event)),
        String::new(),
        "Suggested commands:".to_owned(),
        format!("  rewind show {}", event.id),
        format!("  rewind diff {}", event.id),
        format!("  rewind trace {}", event.id),
        format!("  rewind explain {}", event.id),
        format!("  rewind graph {}", event.id),
        format!("  rewind graph {} --dot", event.id),
        format!("  rewind checkout --before {} --dry-run", event.id),
        format!("  rewind checkout --after {} --dry-run", event.id),
    ];
    if event.kind == "run" {
        lines.push(format!(
            "Replayability: {} cwd {}",
            if event.command_argv_json.is_some() {
                "argv"
            } else if cfg!(unix) {
                "legacy"
            } else {
                "unavailable"
            },
            event.command_cwd_relative
        ));
        lines.push(format!("  rewind replay {} --dry-run", event.id));
        lines.push(format!("  rewind replay {} --compare", event.id));
    }
    if let Some(diff) = &selected.diff {
        if let Some(path) = diff.changes.first().map(|change| change.path.as_str()) {
            lines.push(format!("  rewind log {path}"));
            lines.push(format!("  rewind why {path}"));
            lines.push(format!("  rewind impact {path}"));
            lines.push(format!("  rewind cat {path} --before {}", event.id));
            lines.push(format!("  rewind cat {path} --after {}", event.id));
        }
    }
    lines
}

fn changes_panel_lines(model: &TuiModel) -> Vec<String> {
    let Some(selected) = &model.selected_event else {
        return vec!["No event selected.".to_owned()];
    };
    if let Some(error) = &selected.error {
        return vec![error.clone()];
    }
    if selected.preview_lines.is_empty() {
        vec!["No file changes.".to_owned()]
    } else {
        selected.preview_lines.clone()
    }
}

fn checkpoint_panel_lines(model: &TuiModel) -> Vec<String> {
    let mut lines = vec!["Checkpoints:".to_owned()];
    if model.checkpoints.is_empty() {
        lines.push("  No checkpoints.".to_owned());
        return lines;
    }

    for checkpoint in &model.checkpoints {
        let head = if checkpoint.points_to_head {
            " HEAD"
        } else {
            ""
        };
        lines.push(format!(
            "  {} -> {}  \"{}\"{}",
            checkpoint.name,
            short_snapshot(&checkpoint.snapshot_id),
            checkpoint.message,
            head
        ));
        lines.push(format!("    rewind checkpoint show {}", checkpoint.name));
        lines.push(format!(
            "    rewind checkout --checkpoint {} --dry-run",
            checkpoint.name
        ));
    }
    lines
}

fn stats_panel_lines(model: &TuiModel) -> Vec<String> {
    let mut lines = vec![
        format!(
            "repo format: {}",
            model
                .stats
                .repo_format_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
        format!(
            "db schema: {}",
            model
                .stats
                .db_schema_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
        format!("migration status: {}", model.stats.migration_status),
        format!("config status: {}", model.stats.config_status),
        format!("ignore enabled: {}", yes_no(model.stats.ignore_enabled)),
        format!("ignore rules: {}", model.stats.ignore_rule_count),
        format!("events: {}", model.stats.event_count),
        format!("active journal: {}", yes_no(model.stats.active_journal)),
        format!("checkpoints: {}", model.stats.checkpoint_count),
        format!(
            "reachable snapshots: {}",
            model.stats.reachable_snapshots.len()
        ),
        format!(
            "unreferenced snapshots: {}",
            model.stats.unreferenced_snapshots.len()
        ),
        format!("reachable objects: {}", model.stats.reachable_objects.len()),
        format!(
            "unreferenced objects: {}",
            model.stats.unreferenced_objects.len()
        ),
        format!(
            "reachable bytes: {}",
            format_bytes(model.stats.reachable_object_bytes)
        ),
        format!(
            "reclaimable bytes: {}",
            format_bytes(model.stats.unreferenced_object_bytes)
        ),
    ];
    for (kind, count) in &model.stats.event_counts_by_kind {
        lines.push(format!("{kind}: {count}"));
    }
    lines
}

fn help_lines() -> Vec<String> {
    vec![
        "Up/Down or j/k: select event".to_owned(),
        "PgUp/PgDn: jump".to_owned(),
        "Home/End: first/last".to_owned(),
        "Tab: switch panel".to_owned(),
        "r: reload".to_owned(),
        "?: help".to_owned(),
        "q/Esc: quit".to_owned(),
    ]
}

fn status_bar(model: &TuiModel) -> String {
    format!(
        "HEAD {} | repo v{} schema v{} | {} | {} | events {} | checkpoints {} | q quit | ? help",
        short_snapshot(&model.head_snapshot),
        model
            .stats
            .repo_format_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        model
            .stats
            .db_schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        worktree_label(model),
        if model.recovery_needed {
            "recovery needed"
        } else {
            "no recovery"
        },
        model.events.len(),
        model.checkpoints.len()
    )
}

fn worktree_label(model: &TuiModel) -> String {
    if model.worktree.clean {
        "clean".to_owned()
    } else {
        format!(
            "dirty: +{} ~{} -{} +{}d -{}d",
            model.worktree.added,
            model.worktree.modified,
            model.worktree.deleted,
            model.worktree.added_dirs,
            model.worktree.deleted_dirs
        )
    }
}

fn event_state(event: &TuiEvent) -> &'static str {
    if event.undone {
        "undone"
    } else {
        "active"
    }
}

fn print_integrity_issues(title: &str, issues: &[integrity::IntegrityIssue]) {
    if issues.is_empty() {
        return;
    }

    println!("{title}:");
    for issue in issues {
        println!("  {}", issue.message);
    }
}

fn print_verify_summary(report: &integrity::IntegrityReport) {
    println!("Repo:        {}", report.stats.migration_status);
    println!("Config:      {}", report.stats.config_status);
    println!("Events:      {}", report.stats.event_count);
    println!(
        "Snapshots:   {} reachable, {} unreferenced",
        report.stats.reachable_snapshots.len(),
        report.stats.unreferenced_snapshots.len()
    );
    println!(
        "Objects:     {} reachable, {} unreferenced",
        report.stats.reachable_objects.len(),
        report.stats.unreferenced_objects.len()
    );
    println!("Checkpoints: {}", report.stats.checkpoint_count);
    println!("Traces:      {}", report.stats.trace_stats.total);
    println!("Errors:      {}", report.errors.len());
    println!("Warnings:    {}", report.warnings.len());
}

fn print_gc_plan(plan: &integrity::GcPlan) {
    if !plan.snapshots.is_empty() {
        println!("Would remove snapshots:");
        for snapshot in &plan.snapshots {
            println!("  {snapshot}");
        }
        println!();
    }

    if !plan.objects.is_empty() {
        println!("Would remove objects:");
        for object in &plan.objects {
            println!("  {}  {} bytes", object.hash, object.size);
        }
        println!();
    }

    if plan.snapshots.is_empty() && plan.objects.is_empty() {
        println!("Nothing to remove.");
    }
    println!("Reclaimable: {}", format_bytes(plan.reclaimable_bytes));
}

fn print_checkpoint_dirty_report(status: &status::WorktreeStatus) {
    print!("{}", status::dirty_report(status));
    println!();
    println!("Run `rewind status` to inspect changes.");
    println!("Record changes first with:");
    println!("  rewind commit -m \"describe your changes\"");
}

fn print_run_dirty_report(status: &status::WorktreeStatus) {
    println!("Cannot run command: Rewind worktree is dirty.");
    print_status_groups(status);
    println!();
    println!("Record these changes first with:");
    println!("  rewind commit -m \"describe your changes\"");
    println!();
    println!("Or discard/restore them manually, then try again.");
}

fn print_status(project_dir: &std::path::Path, show_ignored: bool) -> Result<()> {
    let status = status::worktree_status(project_dir)?;
    if status.is_clean() {
        println!("Rewind worktree clean.");
        println!("Head snapshot: {}", status.head_snapshot);
    } else {
        print!("{}", status::dirty_report(&status));
    }
    if show_ignored {
        let mut ignored = String::new();
        status::append_ignored_report(&mut ignored, &status);
        print!("{ignored}");
    }
    Ok(())
}

fn print_checkpoints(project_dir: &Path) -> Result<()> {
    let checkpoints = checkpoint::list_checkpoints(project_dir)?;
    println!("{:<16}{:<11}{:<21}MESSAGE", "NAME", "SNAPSHOT", "CREATED");
    for checkpoint in checkpoints {
        println!(
            "{:<16}{:<11}{:<21}{}",
            checkpoint.name,
            short_snapshot(&checkpoint.snapshot_id),
            display_time(&checkpoint.created_at),
            checkpoint.message
        );
    }
    Ok(())
}

fn print_checkpoint(project_dir: &Path, name: &str) -> Result<()> {
    let checkpoint = checkpoint::get_checkpoint(project_dir, name)?
        .ok_or_else(|| anyhow::anyhow!("checkpoint {name} not found"))?;
    let conn = history::ensure_initialized(project_dir)?;
    let head = history::get_head_snapshot(&conn)?;
    println!("Name: {}", checkpoint.name);
    println!("Snapshot: {}", checkpoint.snapshot_id);
    println!("Created: {}", checkpoint.created_at);
    println!("Message: {}", checkpoint.message);
    println!(
        "Points to HEAD: {}",
        yes_no(head.as_deref() == Some(checkpoint.snapshot_id.as_str()))
    );
    Ok(())
}

fn print_status_groups(status: &status::WorktreeStatus) {
    print_str_group("Added", &status.added_files());
    print_str_group("Modified", &status.modified_files());
    print_str_group("Deleted", &status.deleted_files());
    print_string_group("Added directories", &status.diff.added_dirs);
    print_string_group("Deleted directories", &status.diff.deleted_dirs);
}

fn print_snapshot_diff_groups(diff: &SnapshotDiff) {
    let added = diff
        .changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Created)
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    let modified = diff
        .changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Modified)
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    let deleted = diff
        .changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Deleted)
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();

    print_str_group("Added", &added);
    print_str_group("Modified", &modified);
    print_str_group("Deleted", &deleted);
    print_string_group("Added directories", &diff.added_dirs);
    print_string_group("Deleted directories", &diff.deleted_dirs);
}

fn print_restore_plan(plan: &restore::RestorePlan) {
    println!("Restore plan:");
    print_path_group("Create directories", &plan.create_dirs);
    print_path_group("Remove files", &plan.remove_files);
    print_path_group("Write files", &plan.write_files);
    print_path_group("Remove symlinks", &plan.remove_symlinks);
    print_path_group("Write symlinks", &plan.write_symlinks);
    print_path_group("Remove directories", &plan.remove_dirs);
}

fn print_would_restore_plan(plan: &restore::RestorePlan) {
    print_path_group("Would create directories", &plan.create_dirs);
    print_path_group("Would remove", &plan.remove_files);
    print_path_group("Would write", &plan.write_files);
    print_path_group("Would remove symlinks", &plan.remove_symlinks);
    print_path_group("Would write symlinks", &plan.write_symlinks);
    print_path_group("Would remove directories", &plan.remove_dirs);
    if plan.is_empty() {
        println!("  No changes");
    }
}

fn print_path_group(title: &str, paths: &[std::path::PathBuf]) {
    if paths.is_empty() {
        return;
    }

    println!("{title}:");
    for path in paths {
        println!("  {}", path.display());
    }
}

fn print_timeline(project_dir: &Path) -> Result<()> {
    let conn = history::ensure_initialized(project_dir)?;
    let events = history::list_events(&conn)?;
    let trace_statuses = trace::trace_statuses(&conn)?;
    println!(
        "{:<4}{:<9}{:<12}{:<7}{:<21}{:<6}{:<11}{:<28}COMMAND",
        "ID", "KIND", "TRACE", "DIRTY", "TIME", "EXIT", "STATE", "SNAPSHOT TRANSITION"
    );
    for event in events {
        let state = if event.undone { "undone" } else { "active" };
        println!(
            "{:<4}{:<9}{:<12}{:<7}{:<21}{:<6}{:<11}{:<28}{}",
            event.id,
            event.kind,
            trace_statuses
                .get(&event.id)
                .map(String::as_str)
                .unwrap_or("none"),
            yes_no(event.started_dirty),
            display_time(&event.timestamp),
            event.exit_code,
            state,
            format!(
                "{} -> {}",
                short_snapshot(&event.before_snapshot),
                short_snapshot(&event.after_snapshot)
            ),
            event.command
        );
    }
    if let Some(head) = history::get_head_snapshot(&conn)? {
        println!("HEAD: {head}");
    }
    print_timeline_checkpoints(project_dir)?;
    Ok(())
}

fn print_timeline_checkpoints(project_dir: &Path) -> Result<()> {
    let checkpoints = checkpoint::list_checkpoints(project_dir)?;
    if checkpoints.is_empty() {
        return Ok(());
    }

    println!();
    println!("Checkpoints:");
    for checkpoint in checkpoints {
        println!(
            "  {:<16} -> {}  {}",
            checkpoint.name,
            short_snapshot(&checkpoint.snapshot_id),
            checkpoint.message
        );
    }
    Ok(())
}

fn print_diff(project_dir: &Path, event_id: i64) -> Result<()> {
    let conn = history::ensure_initialized(project_dir)?;
    let event = history::get_event(&conn, event_id)?
        .ok_or_else(|| anyhow::anyhow!("event {event_id} not found"))?;
    let before = load_snapshot(project_dir, &event.before_snapshot)?;
    let after = load_snapshot(project_dir, &event.after_snapshot)?;
    let diff = diff_snapshots(&before, &after);

    println!("Event {}", event.id);
    println!("Command: {}", event.command);
    print_change_group("Created", &diff.changes, ChangeType::Created);
    print_change_group("Modified", &diff.changes, ChangeType::Modified);
    print_change_group("Deleted", &diff.changes, ChangeType::Deleted);
    print_string_group("Created directories", &diff.added_dirs);
    print_string_group("Deleted directories", &diff.deleted_dirs);
    print_metadata_diff_groups(&before, &after, &diff);

    for change in diff
        .changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Modified)
    {
        print_content_diff(project_dir, event_id, change)?;
    }

    Ok(())
}

fn print_metadata_diff_groups(
    before: &rewind_core::snapshot::SnapshotManifest,
    after: &rewind_core::snapshot::SnapshotManifest,
    diff: &SnapshotDiff,
) {
    let mut symlink_lines = Vec::new();
    let mut mode_lines = Vec::new();
    let mut kind_lines = Vec::new();
    for change in &diff.changes {
        let before_kind = change.before_kind.as_deref();
        let after_kind = change.after_kind.as_deref();
        if before_kind != after_kind {
            kind_lines.push(format!(
                "{}: {} -> {}",
                change.path,
                before_kind.unwrap_or("missing"),
                after_kind.unwrap_or("missing")
            ));
        }
        match (
            before.symlinks.get(&change.path),
            after.symlinks.get(&change.path),
        ) {
            (None, Some(link)) => {
                symlink_lines.push(format!("created {} -> {}", change.path, link.target))
            }
            (Some(link), None) => {
                symlink_lines.push(format!("deleted {} -> {}", change.path, link.target))
            }
            (Some(left), Some(right)) if left.target != right.target => {
                symlink_lines.push(format!(
                    "modified {}: {} -> {}",
                    change.path, left.target, right.target
                ))
            }
            _ => {}
        }
        if let (Some(left), Some(right)) = (
            before.files.get(&change.path),
            after.files.get(&change.path),
        ) {
            if left.executable != right.executable {
                mode_lines.push(format!(
                    "{}: executable {} -> {}",
                    change.path,
                    yes_no(left.executable),
                    yes_no(right.executable)
                ));
            }
        }
    }
    print_string_group("Symlink changes", &symlink_lines);
    print_string_group("Mode changes", &mode_lines);
    print_string_group("Kind changes", &kind_lines);
}

fn print_content_diff(project_dir: &Path, event_id: i64, change: &FileChange) -> Result<()> {
    const MAX_TEXT_DIFF_BYTES: u64 = 1024 * 1024;
    let Some(before_hash) = &change.before_hash else {
        return Ok(());
    };
    let Some(after_hash) = &change.after_hash else {
        return Ok(());
    };
    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));
    let before_path = object_store.object_path(before_hash);
    let after_path = object_store.object_path(after_hash);
    let before_meta = fs::metadata(&before_path)?;
    let after_meta = fs::metadata(&after_path)?;
    if before_meta.len() > MAX_TEXT_DIFF_BYTES || after_meta.len() > MAX_TEXT_DIFF_BYTES {
        println!("{}: content too large; textual diff skipped.", change.path);
        return Ok(());
    }

    let before_bytes = fs::read(&before_path)?;
    let after_bytes = fs::read(&after_path)?;
    let (Ok(before_text), Ok(after_text)) = (
        String::from_utf8(before_bytes),
        String::from_utf8(after_bytes),
    ) else {
        println!("Binary or non-UTF8 content changed; textual diff skipped.");
        return Ok(());
    };

    println!("--- {} before event {}", change.path, event_id);
    println!("+++ {} after event {}", change.path, event_id);
    println!("@@");
    for line in before_text.lines() {
        if !after_text.lines().any(|after_line| after_line == line) {
            println!("-{line}");
        }
    }
    for line in after_text.lines() {
        if !before_text.lines().any(|before_line| before_line == line) {
            println!("+{line}");
        }
    }
    Ok(())
}

fn print_string_group(title: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }

    println!("{title}:");
    for path in paths {
        println!("  {path}");
    }
}

fn print_str_group(title: &str, paths: &[&str]) {
    if paths.is_empty() {
        return;
    }

    println!();
    println!("{title}:");
    for path in paths {
        println!("  {path}");
    }
}

fn print_history(project_dir: &std::path::Path) -> Result<()> {
    let conn = history::ensure_initialized(project_dir)?;
    let events = history::list_events(&conn)?;
    let trace_statuses = trace::trace_statuses(&conn)?;
    println!(
        "{:<4}{:<9}{:<12}{:<7}{:<21}{:<6}{:<9}{:<10}{:<9}COMMAND",
        "ID", "KIND", "TRACE", "DIRTY", "TIME", "EXIT", "CREATED", "MODIFIED", "DELETED"
    );
    for event in events {
        println!(
            "{:<4}{:<9}{:<12}{:<7}{:<21}{:<6}{:<9}{:<10}{:<9}{}{}",
            event.id,
            event.kind,
            trace_statuses
                .get(&event.id)
                .map(String::as_str)
                .unwrap_or("none"),
            yes_no(event.started_dirty),
            display_time(&event.timestamp),
            event.exit_code,
            event.created_count,
            event.modified_count,
            event.deleted_count,
            event.command,
            if event.undone { " [undone]" } else { "" }
        );
    }
    Ok(())
}

fn print_event(project_dir: &std::path::Path, event_id: i64) -> Result<()> {
    let conn = history::ensure_initialized(project_dir)?;
    let event = history::get_event(&conn, event_id)?
        .ok_or_else(|| anyhow::anyhow!("event {event_id} not found"))?;
    let changes = history::list_changes(&conn, event_id)?;

    println!("ID: {}", event.id);
    println!("Kind: {}", event.kind);
    println!("Command: {}", event.command);
    println!("Timestamp: {}", event.timestamp);
    println!("Exit code: {}", event.exit_code);
    println!(
        "Started from dirty worktree: {}",
        yes_no(event.started_dirty)
    );
    println!("Before snapshot: {}", event.before_snapshot);
    println!("After snapshot: {}", event.after_snapshot);
    println!("Undone: {}", event.undone);
    if event.kind == "run" {
        println!("Replay:");
        println!(
            "  exact argv captured: {}",
            yes_no(event.command_argv_json.is_some())
        );
        println!("  working dir: {}", event.command_cwd_relative);
        println!(
            "  replay source: {}",
            if event.command_argv_json.is_some() {
                "argv"
            } else if cfg!(unix) {
                "legacy-shell-fallback"
            } else {
                "unsupported"
            }
        );
    }
    print_change_group("Created files", &changes, ChangeType::Created);
    print_change_group("Modified files", &changes, ChangeType::Modified);
    print_change_group("Deleted files", &changes, ChangeType::Deleted);
    if let Some(details) = trace::trace_details(project_dir, event_id)? {
        println!("Trace:");
        println!("  status: {}", details.trace.status);
        println!("  tracer: {}", details.trace.tracer);
        println!("  file ops: {}", details.files.len());
        println!("  process ops: {}", details.processes.len());
        println!(
            "  outside workspace ops: {}",
            details.trace.outside_workspace_ops
        );
        println!("Suggested:");
        println!("  rewind trace {event_id}");
        println!("  rewind explain {event_id}");
    } else {
        println!("Suggested:");
        println!("  rewind explain {event_id}");
    }
    if event.kind == "run" {
        println!("  rewind replay {event_id} --dry-run");
        println!("  rewind replay {event_id} --compare");
    }
    Ok(())
}

fn print_change_group(
    title: &str,
    changes: &[rewind_core::diff::FileChange],
    change_type: ChangeType,
) {
    println!("{title}:");
    for change in changes
        .iter()
        .filter(|change| change.change_type == change_type)
    {
        println!("  {}", change.path);
    }
}

fn short_snapshot(snapshot: &str) -> String {
    snapshot.chars().take(6).collect()
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn display_time(timestamp: &str) -> String {
    chrono_display(timestamp).unwrap_or_else(|| timestamp.chars().take(16).collect())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{:.1} MiB", bytes_f / KIB / KIB)
    }
}

fn chrono_display(timestamp: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some(parsed.format("%Y-%m-%d %H:%M").to_string())
}

#[allow(dead_code)]
fn require_command(command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!("missing command after `rewind run --`");
    }
    Ok(())
}
