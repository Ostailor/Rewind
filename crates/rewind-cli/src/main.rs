use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rewind_core::diff::{diff_snapshots, ChangeType, FileChange, SnapshotDiff};
use rewind_core::object_store::ObjectStore;
use rewind_core::snapshot::load_snapshot;
use rewind_core::{commit, history, init, restore, run, status, REWIND_DIR};
use std::env;
use std::fs;
use std::path::Path;

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
    },
    Restore {
        path: String,
        #[arg(long, conflicts_with = "after", required_unless_present = "after")]
        before: Option<i64>,
        #[arg(long, conflicts_with = "before", required_unless_present = "before")]
        after: Option<i64>,
        #[arg(long)]
        dry_run: bool,
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
        Commands::History => print_history(&project_dir)?,
        Commands::Timeline => print_timeline(&project_dir)?,
        Commands::Diff { event_id } => print_diff(&project_dir, event_id)?,
        Commands::Show { event_id } => print_event(&project_dir, event_id)?,
        Commands::Status => print_status(&project_dir)?,
        Commands::Undo { dry_run } => match restore::undo_latest(&project_dir, dry_run)? {
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
        } => {
            let (source, event_id) = match (before, after) {
                (Some(event_id), None) => (restore::RestoreSource::Before, event_id),
                (None, Some(event_id)) => (restore::RestoreSource::After, event_id),
                _ => bail!("choose exactly one of --before or --after"),
            };
            match restore::targeted_restore(&project_dir, &path, source, event_id, dry_run)? {
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
