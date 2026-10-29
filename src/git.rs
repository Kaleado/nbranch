use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Variables that would redirect git away from the repository we name with -C,
/// e.g. when nbranch is run from inside a git hook.
pub const GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

pub fn cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).env("LC_ALL", "C");
    for var in GIT_ENV {
        c.env_remove(var);
    }
    c
}

pub fn output(dir: &Path, args: &[&str]) -> Result<Output> {
    cmd(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))
}

fn check(args: &[&str], out: &Output) -> Result<()> {
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run git and return its stdout with trailing whitespace removed.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = output(dir, args)?;
    check(args, &out)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run git with `input` on stdin. Only for small inputs such as commit messages.
pub fn run_with_input(dir: &Path, args: &[&str], input: &[u8], envs: &[(&str, &str)]) -> Result<String> {
    let mut child = cmd(dir)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    child.stdin.take().expect("piped stdin").write_all(input)?;
    let out = child.wait_with_output()?;
    check(args, &out)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    run(dir, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
}

pub fn try_rev_parse(dir: &Path, rev: &str) -> Option<String> {
    rev_parse(dir, rev).ok()
}

pub fn is_dirty(worktree: &Path) -> Result<bool> {
    Ok(!run(worktree, &["status", "--porcelain"])?.is_empty())
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

pub fn add_detached_worktree(golden: &Path, path: &Path, commit: &str) -> Result<()> {
    if path.exists() {
        remove_worktree(golden, path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    run(golden, &["worktree", "add", "--quiet", "--detach", &path.to_string_lossy(), commit])?;
    Ok(())
}

/// Best-effort removal of a worktree and its administrative files.
pub fn remove_worktree(golden: &Path, path: &Path) {
    let _ = output(golden, &["worktree", "remove", "--force", &path.to_string_lossy()]);
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = output(golden, &["worktree", "prune"]);
}
