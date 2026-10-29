use crate::config::Config;
use crate::paths::Root;
use crate::ticket::{State, Store};
use crate::{git, util};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn branch_name(nick: &str) -> String {
    format!("nbranch/{nick}")
}

pub fn worktree(root: &Root, nick: &str) -> Result<PathBuf> {
    let path = root.agent_worktree(nick);
    if !path.join(".git").exists() {
        bail!("{nick} has no worktree; run `nbranch new` first");
    }
    Ok(path)
}

/// Create the agent's worktree and branch at the trunk HEAD. Idempotent.
pub fn new(root: &Root, nick: &str) -> Result<(PathBuf, bool)> {
    let path = root.agent_worktree(nick);
    if path.join(".git").exists() {
        return Ok((path, false));
    }
    let golden = root.golden();
    let trunk = root.trunk()?;
    std::fs::create_dir_all(root.agents_dir())?;
    let branch = branch_name(nick);
    let p = path.to_string_lossy();
    if git::try_rev_parse(&golden, &format!("refs/heads/{branch}")).is_some() {
        git::run(&golden, &["worktree", "add", "--quiet", &p, &branch])?;
    } else {
        git::run(&golden, &["worktree", "add", "--quiet", "-b", &branch, &p, &trunk])?;
    }
    run_setup(&Config::load(root, &trunk)?, root, &path, "agent")
        .map_err(|e| e.context(format!("created {} but its setup failed", path.display())))?;
    Ok((path, true))
}

/// Run `setup_command` in a worktree nbranch just created. `kind` is `agent`
/// or `review`.
pub fn run_setup(cfg: &Config, root: &Root, wt: &Path, kind: &str) -> Result<()> {
    let Some(setup) = &cfg.setup else { return Ok(()) };
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(setup)
        .current_dir(wt)
        .env("NBRANCH_WORKTREE", wt)
        .env("NBRANCH_WORKTREE_KIND", kind)
        .env("NBRANCH_ROOT", &root.dir)
        .stdin(Stdio::null());
    for var in git::GIT_ENV {
        cmd.env_remove(var);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        let output = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        bail!("setup_command failed ({}): {}", out.status, util::tail(output.trim_end(), 10));
    }
    Ok(())
}

fn rebase_in_progress(wt: &Path) -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|d| {
        git::run(wt, &["rev-parse", "--git-path", d])
            .map(|p| wt.join(p).exists())
            .unwrap_or(false)
    })
}

/// Rebase the agent's branch onto the latest trunk. On conflict the rebase is
/// left in progress for the agent to resolve.
pub fn sync(root: &Root, nick: &str) -> Result<i32> {
    let wt = worktree(root, nick)?;
    let trunk = root.trunk()?;
    if rebase_in_progress(&wt) {
        bail!("a rebase is already in progress in {}; finish it with `git rebase --continue` or `git rebase --abort`", wt.display());
    }
    if git::is_dirty(&wt)? {
        bail!("commit or stash your changes in {} before syncing", wt.display());
    }
    let out = git::output(&wt, &["rebase", &format!("refs/heads/{trunk}")])?;
    if out.status.success() {
        let head = git::rev_parse(&wt, "HEAD")?;
        println!("Rebased {} onto {trunk}; now at {}.", branch_name(nick), git::short(&head));
        return Ok(0);
    }
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    println!(
        "\nThe rebase stopped on conflicts. Resolve them in {}, `git add` the files and run `git rebase --continue` \
         (or `git rebase --abort` to give up), then run `nbranch land` again.",
        wt.display()
    );
    Ok(1)
}

/// Commits on the agent's branch that haven't reached the trunk, as "sha subject".
pub fn unlanded(root: &Root, nick: &str, wt: &Path, trunk: &str) -> Result<Vec<String>> {
    let cherry = git::run(wt, &["cherry", &format!("refs/heads/{trunk}"), "HEAD"])?;
    let candidates: Vec<&str> = cherry.lines().filter_map(|l| l.strip_prefix("+ ")).collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    // Patch ids change when a pick had to merge around nearby edits, so also
    // count anything contained in one of this agent's landed snapshots.
    let landed_heads: Vec<String> = Store::new(root)
        .all()?
        .into_iter()
        .filter(|t| t.agent == nick && t.state == State::Landed)
        .map(|t| t.head)
        .collect();
    let mut out = Vec::new();
    for c in candidates {
        let landed = landed_heads.iter().any(|h| {
            git::output(wt, &["merge-base", "--is-ancestor", c, h])
                .map(|o| o.status.success())
                .unwrap_or(false)
        });
        if !landed {
            out.push(git::run(wt, &["log", "-1", "--format=%h %s", c])?);
        }
    }
    Ok(out)
}

/// Move the agent's branch and worktree to the trunk HEAD.
pub fn reset(root: &Root, nick: &str, force: bool) -> Result<()> {
    let wt = worktree(root, nick)?;
    let trunk = root.trunk()?;
    if rebase_in_progress(&wt) {
        bail!("a rebase is in progress in {}; finish or abort it first", wt.display());
    }
    if !force {
        if git::is_dirty(&wt)? {
            bail!("uncommitted changes in {}; commit and land them, or use --force to discard them", wt.display());
        }
        let pending = unlanded(root, nick, &wt, &trunk)?;
        if !pending.is_empty() {
            bail!(
                "{} commit(s) haven't landed on {trunk} yet:\n  {}\nLand them first, or use --force to discard them",
                pending.len(),
                pending.join("\n  ")
            );
        }
    }
    git::run(&wt, &["reset", "--quiet", "--hard", &format!("refs/heads/{trunk}")])?;
    let head = git::rev_parse(&wt, "HEAD")?;
    println!("Reset {} to {trunk} at {}.", branch_name(nick), git::short(&head));
    Ok(())
}

/// Latest sign of git activity in a worktree, as a unix time.
fn last_activity(wt: &Path) -> u64 {
    let mut latest = git::run(wt, &["log", "-1", "--format=%ct"])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    for f in ["index", "HEAD", "logs/HEAD"] {
        let Ok(p) = git::run(wt, &["rev-parse", "--git-path", f]) else { continue };
        if let Ok(modified) = std::fs::metadata(wt.join(p)).and_then(|m| m.modified()) {
            let t = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            latest = latest.max(t);
        }
    }
    latest
}

/// Remove idle, clean agent worktrees whose work has all landed (or, with
/// `force`, idle clean ones with unlanded commits), plus leftover temporary
/// worktrees from finished lands.
pub fn gc(root: &Root, idle: Option<Duration>, force: bool, dry_run: bool) -> Result<()> {
    let golden = root.golden();
    let trunk = root.trunk()?;
    let idle = match idle {
        Some(d) => d,
        None => Config::load(root, &trunk)?.gc_idle,
    };
    let store = Store::new(root);
    let tickets = store.locked(|| {
        store.reap()?;
        store.all()
    })?;
    let verb = if dry_run { "would remove" } else { "removed" };

    if let Ok(entries) = std::fs::read_dir(root.tmp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let id: Option<u64> = name.split('-').next().and_then(|s| s.parse().ok());
            let active = id.is_some_and(|id| tickets.iter().any(|t| t.id == id && !t.state.is_terminal()));
            if !active {
                if !dry_run {
                    git::remove_worktree(&golden, &entry.path());
                }
                println!("{verb} temporary worktree {name}");
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(root.agents_dir()) {
        for entry in entries.flatten() {
            let nick = entry.file_name().to_string_lossy().to_string();
            let wt = entry.path();
            if !wt.join(".git").exists() {
                continue;
            }
            let skip = |why: String| println!("kept {nick}: {why}");
            if tickets.iter().any(|t| t.agent == nick && !t.state.is_terminal()) {
                skip("land in progress".into());
                continue;
            }
            if git::is_dirty(&wt)? {
                skip("uncommitted changes".into());
                continue;
            }
            let idle_for = util::now().saturating_sub(last_activity(&wt));
            if idle_for < idle.as_secs() {
                skip(format!("active {} ago", util::age(idle_for)));
                continue;
            }
            let pending = unlanded(root, &nick, &wt, &trunk)?;
            if !pending.is_empty() && !force {
                skip(format!("{} unlanded commit(s); use --force to remove anyway", pending.len()));
                continue;
            }
            if !dry_run {
                git::run(&golden, &["worktree", "remove", "--force", &wt.to_string_lossy()])?;
                git::run(&golden, &["branch", "-D", "--quiet", &branch_name(&nick)])?;
            }
            println!("{verb} {nick}");
        }
    }
    if !dry_run {
        git::run(&golden, &["worktree", "prune"])?;
    }
    Ok(())
}
