use std::process::Command;

fn main() {
    emit_git_rerun_hints();

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=REWIND_BUILD_TARGET={target}");
    println!("cargo:rustc-env=REWIND_BUILD_PROFILE={profile}");

    let git_commit =
        git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let git_dirty = match git_output(&["status", "--porcelain"]) {
        Some(output) if output.trim().is_empty() => "no",
        Some(_) => "yes",
        None => "unknown",
    };
    println!("cargo:rustc-env=REWIND_GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=REWIND_GIT_DIRTY={git_dirty}");
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_owned())
}

fn emit_git_rerun_hints() {
    let Some(git_dir) = git_output(&["rev-parse", "--git-dir"]) else {
        return;
    };
    if git_dir.is_empty() {
        return;
    }
    println!("cargo:rerun-if-changed={git_dir}/HEAD");
    println!("cargo:rerun-if-changed={git_dir}/index");
}
