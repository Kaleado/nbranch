//! Agents on other machines. An nbranch clone is an ordinary git clone of a
//! hub's golden repository, reached over SSH. The agent works on its branch
//! in the clone; landing pushes the branch to golden and runs `nbranch land`
//! on the hub, where the queue and reviewers live. Results reach the agent
//! the usual way, over nchat/nqueue (e.g. through `nqueue tunnel`).

use crate::config::ConfigFile;
use crate::{agent, git, identity, util};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The clone's name for the hub.
pub const REMOTE: &str = "hub";

/// `$NBRANCH_SSH`, else `ssh` from `PATH`.
pub fn ssh_binary() -> String {
    std::env::var("NBRANCH_SSH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "ssh".into())
}

/// Quote `s` for a POSIX shell (or fish).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What the hub tells a clone about itself (`nbranch __hub-info`).
#[derive(Serialize, Deserialize, Debug)]
pub struct HubInfo {
    pub root: PathBuf,
    pub trunk: String,
    /// Whether the caller already has a worktree on the hub.
    pub has_worktree: bool,
    /// Whether the caller's branch exists in golden.
    pub has_branch: bool,
}

/// Where a hub is: `host:/path/to/root` (the host as for ssh: `host`,
/// `user@host`, or a `~/.ssh/config` alias). A relative path is relative to
/// the home directory there.
#[derive(Clone, Debug, PartialEq)]
pub struct HubAddr {
    pub dest: String,
    pub root: String,
}

impl std::str::FromStr for HubAddr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<HubAddr> {
        match s.split_once(':') {
            Some((dest, root)) if !dest.is_empty() && !root.is_empty() && !dest.starts_with('-') => {
                Ok(HubAddr { dest: dest.into(), root: root.into() })
            }
            _ => bail!("expected host:/path/to/root, got '{s}'"),
        }
    }
}

/// The hub end of a clone: how to reach it and run nbranch there.
#[derive(Clone, Debug)]
pub struct Hub {
    pub dest: String,
    pub root: String,
    /// The nbranch command on the hub (non-interactive SSH sessions often
    /// have a shorter PATH, so it can be a full path).
    pub command: String,
}

impl Hub {
    /// `ssh <dest> <command> --root <root> <args>`, acting as `nick`.
    fn ssh(&self, nick: &str, args: &[String], tty: bool) -> Command {
        let mut remote = format!(
            "env NBRANCH_AGENT={} {} --root {}",
            shell_quote(nick),
            self.command,
            shell_quote(&self.root)
        );
        for a in args {
            remote.push(' ');
            remote.push_str(&shell_quote(a));
        }
        let mut cmd = Command::new(ssh_binary());
        if tty {
            cmd.arg("-t");
        }
        cmd.arg("--").arg(&self.dest).arg(remote);
        cmd
    }

    /// Run nbranch on the hub with this terminal, returning its exit code.
    pub fn forward(&self, nick: &str, args: &[String], tty: bool) -> Result<i32> {
        let status = self
            .ssh(nick, args, tty)
            .status()
            .with_context(|| format!("running {}", ssh_binary()))?;
        match status.code() {
            Some(255) => bail!("ssh to {} failed", self.dest),
            Some(code) => Ok(code),
            None => bail!("ssh to {} was killed", self.dest),
        }
    }

    /// Run nbranch on the hub and return its stdout.
    pub fn output(&self, nick: &str, args: &[&str]) -> Result<String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let out = self
            .ssh(nick, &args, false)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", ssh_binary()))?;
        if !out.status.success() {
            bail!(
                "nbranch {} on {} failed: {}",
                args.join(" "),
                self.dest,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn golden_url(&self) -> String {
        format!("{}:{}/golden.git", self.dest, self.root)
    }
}

/// git, with `$NBRANCH_SSH` as its ssh when that's set.
fn git_cmd(dir: &Path) -> Command {
    let mut cmd = git::cmd(dir);
    if let Ok(ssh) = std::env::var("NBRANCH_SSH") {
        if !ssh.is_empty() && std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
    }
    cmd.stdin(Stdio::null());
    cmd
}

fn git_run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_cmd(dir).args(args).output().with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// An agent's clone of a hub.
#[derive(Clone, Debug)]
pub struct Clone {
    pub dir: PathBuf,
    pub hub: Hub,
    pub nick: String,
    pub trunk: String,
}

impl Clone {
    /// The clone containing the current directory, if it is one.
    pub fn discover() -> Result<Option<Clone>> {
        let cwd = std::env::current_dir()?;
        let Ok(top) = git::run(&cwd, &["rev-parse", "--show-toplevel"]) else { return Ok(None) };
        let dir = PathBuf::from(top);
        let get = |key: &str| git::run(&dir, &["config", "--get", &format!("nbranch.{key}")]).ok();
        let Some(dest) = get("hub") else { return Ok(None) };
        let (Some(root), Some(nick), Some(trunk)) = (get("root"), get("agent"), get("trunk")) else {
            bail!("{} is an incomplete nbranch clone (nbranch.* git config missing)", dir.display());
        };
        let command = get("command").unwrap_or_else(|| "nbranch".into());
        Ok(Some(Clone { dir, hub: Hub { dest, root, command }, nick, trunk }))
    }

    /// The clone's agent, checked against the caller's identity: the branch
    /// and notifications belong to the nick the clone was made for.
    pub fn check_identity(&self) -> Result<()> {
        let me = identity::resolve()?;
        if me != self.nick {
            bail!(
                "this clone belongs to {}, but you are {me}; make your own with `nbranch clone {}:{} <dir>`",
                self.nick,
                self.hub.dest,
                self.hub.root
            );
        }
        Ok(())
    }

    pub fn branch(&self) -> String {
        agent::branch_name(&self.nick)
    }

    /// The hub's trunk as last fetched.
    pub fn trunk_ref(&self) -> String {
        format!("refs/remotes/{REMOTE}/{}", self.trunk)
    }

    pub fn fetch_trunk(&self) -> Result<()> {
        let spec = format!("+refs/heads/{}:{}", self.trunk, self.trunk_ref());
        git_run(&self.dir, &["fetch", "--quiet", REMOTE, &spec]).map(|_| ())
    }

    /// Publish the agent's branch to golden, replacing whatever was there:
    /// the branch is the agent's own, and lands snapshot it on submission.
    pub fn push_branch(&self) -> Result<()> {
        let b = self.branch();
        let spec = format!("+refs/heads/{b}:refs/heads/{b}");
        git_run(&self.dir, &["push", "--quiet", REMOTE, &spec]).map(|_| ())
    }

    /// Fetch `refspec` from the hub and return the commit it fetched.
    pub fn fetch(&self, refspec: &str) -> Result<String> {
        git_run(&self.dir, &["fetch", "--quiet", REMOTE, refspec])?;
        git::rev_parse(&self.dir, "FETCH_HEAD")
    }

    /// Commits of this agent's lands that reached the trunk.
    pub fn landed_heads(&self) -> Result<Vec<String>> {
        Ok(serde_json::from_str(&self.hub.output(&self.nick, &["__landed-heads"])?)?)
    }

    pub fn git_dir(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(git::run(&self.dir, &["rev-parse", "--absolute-git-dir"])?))
    }
}

/// Clone `hub` into `dir` as `nick` and check out the agent's branch.
pub fn clone(hub: &HubAddr, dir: &Path, command: &str, nick: &str) -> Result<Clone> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} already exists and isn't empty", dir.display());
    }
    let h = Hub { dest: hub.dest.clone(), root: hub.root.clone(), command: command.into() };
    let info: HubInfo = serde_json::from_str(&h.output(nick, &["__hub-info"])?)
        .with_context(|| format!("reading nbranch's answer from {}", hub.dest))?;
    if info.has_worktree {
        bail!(
            "{nick} already has a worktree on {} ({}/agents/{nick}); work there, or clone as another agent",
            hub.dest,
            info.root.display()
        );
    }
    let hub = Hub { root: info.root.to_string_lossy().into_owned(), ..h };
    let dir = &std::env::current_dir()?.join(dir);
    let parent = dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let dir_s = dir.to_string_lossy();
    git_run(
        parent,
        &["clone", "--quiet", "--origin", REMOTE, "--single-branch", "--branch", &info.trunk, &hub.golden_url(), &dir_s],
    )?;
    let dir = dir.canonicalize()?;
    let c = Clone { dir: dir.clone(), hub, nick: nick.into(), trunk: info.trunk };
    let branch = c.branch();
    let start = if info.has_branch {
        // pick up where this agent left off
        c.fetch(&format!("refs/heads/{branch}"))?
    } else {
        c.trunk_ref()
    };
    git::run(&dir, &["checkout", "--quiet", "--no-track", "-B", &branch, &start])?;
    for (k, v) in [
        ("hub", c.hub.dest.as_str()),
        ("root", c.hub.root.as_str()),
        ("command", c.hub.command.as_str()),
        ("agent", nick),
        ("trunk", c.trunk.as_str()),
    ] {
        git::run(&dir, &["config", &format!("nbranch.{k}"), v])?;
    }
    // The hub's setup_command is for the hub's paths, so a clone runs this
    // machine's own (from its global config).
    let global = match crate::config::global_path() {
        Some(p) if p.is_file() => ConfigFile::parse(&std::fs::read_to_string(&p)?, &p.display().to_string())?,
        _ => ConfigFile::default(),
    };
    if let Some(setup) = &global.setup_command {
        agent::run_setup_command(setup, &dir, &dir, "clone")
            .map_err(|e| e.context(format!("cloned {} but its setup failed", dir.display())))?;
    }
    Ok(c)
}

/// A review checkout in a clone, for a review agent on another machine.
pub fn review_checkout(c: &Clone, request: &str) -> Result<(PathBuf, String, String)> {
    if !util::valid_nick(request) {
        bail!("invalid request ID '{request}'");
    }
    let reqs: Vec<crate::review_agent::Request> = serde_json::from_str(&c.hub.output(&c.nick, &["requests", "--json"])?)?;
    let reqs_ids: Vec<String> = reqs.iter().map(|r| r.id.clone()).collect();
    let req = reqs
        .into_iter()
        .find(|r| r.id == request)
        .with_context(|| format!("no review request {request} waiting for {}", c.nick))?;
    let head = c.fetch(&crate::review_agent::review_ref(&req.id))?;
    if head != req.head {
        bail!("the hub's review ref for {request} is at {head}, expected {}", req.head);
    }
    // base is on the trunk, an ancestor of head, so it came with it
    let dir = c.git_dir()?.join("nbranch").join("reviews");
    // checkouts for finished reviews
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if !reqs_ids.iter().any(|id| *id == e.file_name().to_string_lossy()) {
                git::remove_worktree(&c.dir, &e.path());
            }
        }
    }
    let wt = dir.join(&req.id);
    git::add_detached_worktree(&c.dir, &wt, &head)?;
    Ok((wt, req.base, req.head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("plain-1.0"), "plain-1.0");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(x)"), "'$(x)'");
    }

    #[test]
    fn hub_addresses() {
        let h: HubAddr = "me@box:/srv/proj".parse().unwrap();
        assert_eq!((h.dest.as_str(), h.root.as_str()), ("me@box", "/srv/proj"));
        let h: HubAddr = "box:work/proj".parse().unwrap();
        assert_eq!(h.root, "work/proj");
        assert!("box".parse::<HubAddr>().is_err());
        assert!(":/x".parse::<HubAddr>().is_err());
        assert!("-oProxyCommand=x:/y".parse::<HubAddr>().is_err());
    }
}
