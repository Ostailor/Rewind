use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
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
    checkout, checkpoint, commit, forensics, history, init, integrity, restore, run, status,
    transaction, REWIND_DIR,
};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "rewind",
    version,
    about = "Host-mode reversible command runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init,
    Run {
        #[arg(long)]
        allow_dirty: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    Commit {
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        dry_run: bool,
    },
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
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
    Verify {
        #[arg(long)]
        strict: bool,
    },
    Stats,
    Gc {
        #[arg(long)]
        yes: bool,
    },
    Recover {
        #[arg(long)]
        status: bool,
        #[arg(long, conflicts_with = "abort")]
        complete: bool,
        #[arg(long, conflicts_with = "complete")]
        abort: bool,
    },
    Tui {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        selected: Option<i64>,
    },
    Log {
        path: String,
        #[arg(long)]
        limit: Option<usize>,
    },
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
    Deleted {
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        path: Option<String>,
    },
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
    History,
    Timeline,
    Diff {
        event_id: i64,
    },
    Show {
        event_id: i64,
    },
    Status,
    Undo {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true)]
        debug_stop_after_journal: bool,
        #[arg(long, hide = true)]
        debug_stop_after_commit: bool,
    },
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

fn main() {
    if let Err(error) = run_cli() {
        eprintln!("error: {error:?}");
        std::process::exit(1);
    }
}

fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let project_dir = env::current_dir().context("reading current directory")?;

    match cli.command {
        Commands::Init => {
            init::init_project(&project_dir)?;
            println!("initialized .rewind");
        }
        Commands::Run {
            command,
            allow_dirty,
        } => match run::run_command(&project_dir, &command, allow_dirty)? {
            run::RunOutcome::Ran { exit_code } => {
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
        Commands::Log { path, limit } => {
            warn_active_transaction(&project_dir);
            print_path_history(&project_dir, &path, limit)?;
        }
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
        Commands::Status => print_status(&project_dir)?,
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

fn print_path_history(project_dir: &Path, path: &str, limit: Option<usize>) -> Result<()> {
    let entries = forensics::path_history(project_dir, path, limit)?;
    if entries.is_empty() {
        println!("No history found for {path}.");
        return Ok(());
    }

    println!("Path history for {path}");
    println!();
    println!(
        "{:<4}{:<21}{:<9}{:<10}{:<8}{:<7}COMMAND",
        "ID", "TIME", "KIND", "CHANGE", "STATE", "DIRTY"
    );
    for entry in entries {
        println!(
            "{:<4}{:<21}{:<9}{:<10}{:<8}{:<7}{}",
            entry.event_id,
            display_time(&entry.timestamp),
            entry.kind,
            entry.change_type,
            if entry.undone { "undone" } else { "active" },
            yes_no(entry.started_dirty),
            entry.command
        );
        if entry.path != path {
            println!("    {}", entry.path);
        }
    }
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
                "{marker} {} {} {}",
                event.id,
                event.kind,
                event_state(event)
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
                "{marker} {} {} {}",
                event.id,
                event.kind,
                event_state(event)
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
        format!("Started dirty: {}", yes_no(event.started_dirty)),
        format!("Before: {}", event.before_snapshot),
        format!("After: {}", event.after_snapshot),
        format!("State: {}", event_state(event)),
        String::new(),
        "Suggested commands:".to_owned(),
        format!("  rewind show {}", event.id),
        format!("  rewind diff {}", event.id),
        format!("  rewind checkout --before {} --dry-run", event.id),
        format!("  rewind checkout --after {} --dry-run", event.id),
    ];
    if let Some(diff) = &selected.diff {
        if let Some(path) = diff.changes.first().map(|change| change.path.as_str()) {
            lines.push(format!("  rewind log {path}"));
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
        "HEAD {} | {} | {} | events {} | checkpoints {} | verify: not run | q quit | ? help",
        short_snapshot(&model.head_snapshot),
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

fn print_status(project_dir: &std::path::Path) -> Result<()> {
    let status = status::worktree_status(project_dir)?;
    if status.is_clean() {
        println!("Rewind worktree clean.");
        println!("Head snapshot: {}", status.head_snapshot);
    } else {
        print!("{}", status::dirty_report(&status));
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
    print_path_group("Remove directories", &plan.remove_dirs);
}

fn print_would_restore_plan(plan: &restore::RestorePlan) {
    print_path_group("Would create directories", &plan.create_dirs);
    print_path_group("Would remove", &plan.remove_files);
    print_path_group("Would write", &plan.write_files);
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
    println!(
        "{:<4}{:<9}{:<7}{:<21}{:<6}{:<11}{:<28}COMMAND",
        "ID", "KIND", "DIRTY", "TIME", "EXIT", "STATE", "SNAPSHOT TRANSITION"
    );
    for event in events {
        let state = if event.undone { "undone" } else { "active" };
        println!(
            "{:<4}{:<9}{:<7}{:<21}{:<6}{:<11}{:<28}{}",
            event.id,
            event.kind,
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

    for change in diff
        .changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Modified)
    {
        print_content_diff(project_dir, event_id, change)?;
    }

    Ok(())
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
    println!(
        "{:<4}{:<9}{:<7}{:<21}{:<6}{:<9}{:<10}{:<9}COMMAND",
        "ID", "KIND", "DIRTY", "TIME", "EXIT", "CREATED", "MODIFIED", "DELETED"
    );
    for event in events {
        println!(
            "{:<4}{:<9}{:<7}{:<21}{:<6}{:<9}{:<10}{:<9}{}{}",
            event.id,
            event.kind,
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
    print_change_group("Created files", &changes, ChangeType::Created);
    print_change_group("Modified files", &changes, ChangeType::Modified);
    print_change_group("Deleted files", &changes, ChangeType::Deleted);
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
