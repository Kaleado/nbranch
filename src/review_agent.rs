//! The standard reviewer that hands a land to an LLM review agent over the
//! notification channel and waits for its verdict.

use crate::config::Config;
use crate::paths::Root;
use crate::ticket::{Comment, ReviewerId, Severity, Store};
use crate::{git, identity, notify, util};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Request {
    pub id: String,
    pub ticket: u64,
    pub stage: String,
    pub reviewer: String,
    pub reviewer_nick: String,
    pub agent: String,
    pub base: String,
    pub head: String,
    pub worktree: PathBuf,
    pub created: u64,
}

#[derive(Serialize, Deserialize)]
struct Response {
    accept: bool,
    reviewer: ReviewerId,
    comments: Vec<Comment>,
}

fn request_path(root: &Root, id: &str) -> PathBuf {
    root.requests_dir().join(format!("{id}.json"))
}

fn response_path(root: &Root, id: &str) -> PathBuf {
    root.requests_dir().join(format!("{id}.response"))
}

/// Run as a reviewer command: ask `nick` to review and wait for its response.
/// The reviewer timeout in the config bounds the wait.
pub fn run(root: &Root, nick: &str) -> Result<i32> {
    let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set; review-agent must run as an nbranch reviewer"));
    let ticket: u64 = var("NBRANCH_TICKET")?.parse()?;
    let stage = var("NBRANCH_STAGE")?;
    let base = var("NBRANCH_BASE")?;
    let head = var("NBRANCH_HEAD")?;
    let agent = var("NBRANCH_AGENT")?;
    let reviewer = var("NBRANCH_REVIEWER").unwrap_or_else(|_| "review-agent".into());
    if !util::valid_nick(nick) {
        bail!("invalid reviewer nick '{nick}'");
    }
    if nick == agent {
        bail!("the review agent ({nick}) must be a different session from the lander");
    }

    let id = format!("{ticket}-{stage}-{nick}");
    std::fs::create_dir_all(root.requests_dir())?;
    let _ = std::fs::remove_file(response_path(root, &id));
    let req = Request {
        id: id.clone(),
        ticket,
        stage,
        reviewer,
        reviewer_nick: nick.into(),
        agent,
        base,
        head,
        worktree: std::env::current_dir()?,
        created: util::now(),
    };
    util::write_atomic(&request_path(root, &id), &serde_json::to_vec_pretty(&req)?)?;

    let trunk = root.trunk()?;
    let cfg = Config::load(root, &trunk)?;
    let commits = git::run(&root.golden(), &["rev-list", "--count", &format!("{}..{}", req.base, req.head)])?;
    let extra = [
        ("NBRANCH_EVENT", "review-request".to_string()),
        ("NBRANCH_TICKET", ticket.to_string()),
        ("NBRANCH_REQUEST", id.clone()),
    ];
    notify::send(&cfg.notify, nick, &request_message(&req, &commits), &extra)?;
    eprintln!("waiting for {nick} to respond to review request {id}");

    loop {
        if let Ok(data) = std::fs::read(response_path(root, &id)) {
            let resp: Response = serde_json::from_slice(&data).context("parsing review response")?;
            let _ = std::fs::remove_file(request_path(root, &id));
            let _ = std::fs::remove_file(response_path(root, &id));
            println!("{}", serde_json::json!({ "reviewer": resp.reviewer, "comments": resp.comments }));
            return Ok(if resp.accept { 0 } else { 1 });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn request_message(req: &Request, commits: &str) -> String {
    format!(
        "nbranch review request {id}: please review land #{ticket} by {agent} ({commits} commit(s), {base}..{head}). \
         It is checked out at {wt} (don't modify it); see the change with `git -C {wt} diff {base} {head}`. \
         When you're done, run `nbranch respond {id} accept` or \
         `nbranch respond {id} reject --comment 'path:line: error: what is wrong'` (repeat --comment as needed), \
         passing --model <your model id> or setting NBRANCH_MODEL.",
        id = req.id,
        ticket = req.ticket,
        agent = req.agent,
        base = git::short(&req.base),
        head = git::short(&req.head),
        wt = req.worktree.display(),
    )
}

/// Record a review agent's verdict for a pending request.
pub fn respond(root: &Root, id: &str, accept: bool, comments: &[String], model: Option<String>) -> Result<()> {
    let nick = identity::resolve()?;
    let data = std::fs::read(request_path(root, id)).with_context(|| format!("no pending review request {id}"))?;
    let req: Request = serde_json::from_slice(&data)?;
    if req.reviewer_nick != nick {
        bail!("request {id} is addressed to {}, but you are {nick}", req.reviewer_nick);
    }
    if response_path(root, id).exists() {
        bail!("request {id} already has a response");
    }
    let model = model
        .filter(|m| !m.trim().is_empty())
        .context("--model (or NBRANCH_MODEL) is required so landed commits record which model reviewed them")?;
    let default = if accept { Severity::Note } else { Severity::Error };
    let comments: Vec<Comment> = comments.iter().map(|c| parse_comment(c, default)).collect();
    if !accept && comments.is_empty() {
        bail!("a rejection needs at least one --comment");
    }
    let resp = Response { accept, reviewer: ReviewerId { nick, model: Some(model) }, comments };
    util::write_atomic(&response_path(root, id), &serde_json::to_vec_pretty(&resp)?)?;
    println!("Recorded {} for land #{}.", if accept { "acceptance" } else { "rejection" }, req.ticket);
    Ok(())
}

/// Pending requests addressed to `nick` for lands that are still running.
pub fn pending(root: &Root, nick: &str) -> Result<Vec<Request>> {
    let store = Store::new(root);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.requests_dir()) else { return Ok(out) };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(req) = std::fs::read(&path).map_err(anyhow::Error::from).and_then(|d| Ok(serde_json::from_slice::<Request>(&d)?))
        else {
            continue;
        };
        let live = store.load(req.ticket).map(|t| !t.state.is_terminal()).unwrap_or(false);
        if req.reviewer_nick == nick && live && !response_path(root, &req.id).exists() {
            out.push(req);
        }
    }
    out.sort_by_key(|r| r.created);
    Ok(out)
}

/// Parse `path:line: severity: body`, `path:line: body`, `path: severity: body`,
/// `severity: body` or a bare body.
pub fn parse_comment(spec: &str, default: Severity) -> Comment {
    let parts: Vec<&str> = spec.splitn(4, ':').collect();
    let rest = |n: usize| spec.splitn(n + 1, ':').nth(n).unwrap_or("").trim().to_string();
    let line = |s: &str| s.trim().parse::<u64>().ok();
    if parts.len() == 4 {
        if let (Some(l), Some(sev)) = (line(parts[1]), Severity::parse(parts[2])) {
            return Comment { file: Some(parts[0].trim().into()), line: Some(l), severity: sev, body: rest(3) };
        }
    }
    if parts.len() >= 3 {
        if let Some(l) = line(parts[1]) {
            return Comment { file: Some(parts[0].trim().into()), line: Some(l), severity: default, body: rest(2) };
        }
        if let Some(sev) = Severity::parse(parts[1]) {
            return Comment { file: Some(parts[0].trim().into()), line: None, severity: sev, body: rest(2) };
        }
    }
    if parts.len() >= 2 {
        if let Some(sev) = Severity::parse(parts[0]) {
            return Comment::general(sev, rest(1));
        }
    }
    Comment::general(default, spec.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_specs() {
        let c = parse_comment("src/a.rs:12: warning: unused: x", Severity::Error);
        assert_eq!((c.file.as_deref(), c.line, c.severity, c.body.as_str()), (Some("src/a.rs"), Some(12), Severity::Warning, "unused: x"));
        let c = parse_comment("src/a.rs:12: off by one", Severity::Error);
        assert_eq!((c.line, c.severity, c.body.as_str()), (Some(12), Severity::Error, "off by one"));
        let c = parse_comment("README.md: note: typo", Severity::Error);
        assert_eq!((c.file.as_deref(), c.line, c.severity), (Some("README.md"), None, Severity::Note));
        let c = parse_comment("warning: missing tests", Severity::Error);
        assert_eq!((c.file, c.severity, c.body.as_str()), (None, Severity::Warning, "missing tests"));
        let c = parse_comment("Looks risky: please split it up", Severity::Error);
        assert_eq!((c.file, c.body.as_str()), (None, "Looks risky: please split it up"));
    }
}
