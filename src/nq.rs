//! Review requests travel over nqueue: each goes on a compete queue whose
//! subscribers are review agents, leased to one of them until it answers
//! with `nbranch respond`.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

/// The agent review requests are sent from.
pub const PUBLISHER: &str = "nbranch";

/// The queue reviewers subscribe to when `review-agent` isn't given one.
pub const DEFAULT_QUEUE: &str = "nbranch.review";

/// The queue for requests addressed to one review agent.
pub fn agent_queue(nick: &str) -> String {
    format!("{DEFAULT_QUEUE}.{nick}")
}

/// Run `nqueue` as the agent `id`, or as nobody, and return its stdout.
fn run(id: Option<&str>, args: &[&str]) -> Result<String> {
    let bin = std::env::var("NQUEUE_BIN").unwrap_or_else(|_| "nqueue".into());
    let mut cmd = Command::new(&bin);
    cmd.args(args).stdin(Stdio::null());
    match id {
        Some(id) => cmd.env("NQUEUE_ID", id),
        None => cmd.env_remove("NQUEUE_ID"),
    };
    let out = cmd.output().with_context(|| format!("running {bin} (is nqueue installed?)"))?;
    if !out.status.success() {
        bail!("nqueue {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Make sure nqueue knows the agent `id`, without changing whether it's online.
fn ensure_agent(id: &str) -> Result<()> {
    match run(None, &["register", id]) {
        Ok(_) => run(Some(id), &["unregister"]).map(|_| ()),
        Err(e) if e.to_string().contains("already in use") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Create `queue` with `policy` from the nqueue config unless it exists.
fn ensure_queue(queue: &str, policy: &str) -> Result<()> {
    match run(None, &["create", queue, "--policy", policy]) {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("already exists") => Ok(()),
        Err(e) if e.to_string().contains("unknown policy") => Err(e.context(format!(
            "add a [policies.{policy}] compete queue policy to ~/.nqueue/config.toml (see the nbranch README)"
        ))),
        Err(e) => Err(e),
    }
}

/// Put a review request on `queue` and return its message ID. With
/// `reviewer`, that agent is subscribed to the queue first.
pub fn send_request(queue: &str, policy: &str, reviewer: Option<&str>, body: &str) -> Result<String> {
    ensure_queue(queue, policy)?;
    if let Some(nick) = reviewer {
        ensure_agent(nick)?;
        run(Some(nick), &["subscribe", queue])?;
    }
    ensure_agent(PUBLISHER)?;
    run(Some(PUBLISHER), &["enqueue", queue, body])
}

/// Mark a request handled for the agent `id` holding its lease.
pub fn ack(id: &str, message: &str) -> Result<()> {
    run(Some(id), &["ack", message]).map(|_| ())
}
