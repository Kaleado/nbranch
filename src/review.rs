use crate::config::{Reviewer, Stage};
use crate::paths::Root;
use crate::ticket::{Comment, ReviewResult, ReviewerId, Severity, Store, Verdict};
use crate::{git, util};
use anyhow::Result;
use serde::Deserialize;
use std::fs::File;
use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How much reviewer output becomes the comment when a non-JSON reviewer rejects.
const OUTPUT_COMMENT_LINES: usize = 40;

pub struct StageRun<'a> {
    pub root: &'a Root,
    pub store: &'a Store,
    pub ticket: u64,
    pub agent: &'a str,
    pub stage: Stage,
    pub attempt: u32,
    pub worktree: &'a Path,
    pub base: &'a str,
    pub head: &'a str,
}

pub enum StageOutcome {
    Done(Vec<ReviewResult>),
    Cancelled,
}

struct Running<'a> {
    reviewer: &'a Reviewer,
    child: Option<Child>,
    started: Instant,
    stdout: PathBuf,
    stderr: PathBuf,
    result: Option<ReviewResult>,
}

impl Running<'_> {
    fn complete(&mut self, run: &StageRun, status: Option<ExitStatus>, timed_out: bool) {
        let stdout = std::fs::read(&self.stdout).unwrap_or_default();
        let stderr = std::fs::read(&self.stderr).unwrap_or_default();
        let code = status.and_then(|s| s.code());
        let signal = status.and_then(|s| s.signal());
        let (verdict, comments, reviewer, error) = interpret(
            code,
            signal,
            timed_out.then_some(self.reviewer.timeout),
            &String::from_utf8_lossy(&stdout),
            &String::from_utf8_lossy(&stderr),
        );
        self.result = Some(ReviewResult {
            name: self.reviewer.name.clone(),
            stage: run.stage,
            attempt: run.attempt,
            verdict,
            exit_code: code,
            duration_ms: self.started.elapsed().as_millis() as u64,
            reviewer,
            comments,
            error,
            stdout: Some(self.stdout.clone()),
            stderr: Some(self.stderr.clone()),
        });
    }

    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // Each reviewer leads its own process group; take its children too.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
        }
    }
}

/// Run every reviewer in a stage in parallel and collect all of their results.
pub fn run_stage(run: &StageRun, reviewers: &[&Reviewer]) -> Result<StageOutcome> {
    if reviewers.is_empty() {
        return Ok(StageOutcome::Done(Vec::new()));
    }
    let diff = git::output(&run.root.golden(), &["diff", "--no-color", "--no-ext-diff", run.base, run.head])?.stdout;
    let dir = run.root.reviews_dir(run.ticket);
    std::fs::create_dir_all(&dir)?;

    let mut running = Vec::new();
    for reviewer in reviewers {
        let stem = format!("{}-{}-{}", run.attempt, run.stage.as_str(), util::sanitize(&reviewer.name));
        let stdout = dir.join(format!("{stem}.out"));
        let stderr = dir.join(format!("{stem}.err"));
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&reviewer.command)
            .current_dir(run.worktree)
            .env("NBRANCH_STAGE", run.stage.as_str())
            .env("NBRANCH_BASE", run.base)
            .env("NBRANCH_HEAD", run.head)
            .env("NBRANCH_AGENT", run.agent)
            .env("NBRANCH_TICKET", run.ticket.to_string())
            .env("NBRANCH_REVIEWER", &reviewer.name)
            .env("NBRANCH_ROOT", &run.root.dir)
            .stdin(Stdio::piped())
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?)
            .process_group(0);
        for var in git::GIT_ENV {
            cmd.env_remove(var);
        }
        let mut entry = Running { reviewer, child: None, started: Instant::now(), stdout, stderr, result: None };
        match cmd.spawn() {
            Ok(mut child) => {
                let mut stdin = child.stdin.take().expect("piped stdin");
                let diff = diff.clone();
                // Reviewers may never read stdin; a broken pipe here is fine.
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&diff);
                });
                entry.child = Some(child);
            }
            Err(e) => {
                let _ = std::fs::write(&entry.stderr, format!("failed to start reviewer: {e}\n"));
                entry.complete(run, None, false);
            }
        }
        running.push(entry);
    }

    set_detail(run, &running);
    let mut last_check = Instant::now();
    loop {
        let mut pending = false;
        for r in running.iter_mut().filter(|r| r.result.is_none()) {
            let status = r.child.as_mut().expect("spawned").try_wait()?;
            match status {
                Some(status) => r.complete(run, Some(status), false),
                None if r.started.elapsed() > r.reviewer.timeout => {
                    r.kill();
                    r.complete(run, None, true);
                }
                None => pending = true,
            }
        }
        if !pending {
            break;
        }
        if last_check.elapsed() > Duration::from_millis(300) {
            last_check = Instant::now();
            if run.store.load(run.ticket)?.cancel_requested {
                for r in running.iter_mut().filter(|r| r.result.is_none()) {
                    r.kill();
                }
                return Ok(StageOutcome::Cancelled);
            }
            set_detail(run, &running);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(StageOutcome::Done(running.into_iter().filter_map(|r| r.result).collect()))
}

fn set_detail(run: &StageRun, running: &[Running]) {
    let names: Vec<&str> = running
        .iter()
        .filter(|r| r.result.is_none())
        .map(|r| r.reviewer.name.as_str())
        .collect();
    let detail = format!("{}: running {}", run.stage.as_str(), names.join(", "));
    let _ = run.store.update(run.ticket, |t| t.detail = Some(detail));
}

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    reviewer: Option<ReviewerId>,
    #[serde(default)]
    comments: Vec<Comment>,
}

/// Turn a reviewer's exit status and output into a verdict.
///
/// Exit 0 accepts and 1 rejects; anything else, a signal or a timeout is a
/// reviewer error. Output is JSON (`{reviewer, comments}`) when the reviewer
/// speaks the protocol. Plain commands are supported too: when one rejects,
/// the tail of its output becomes the comment.
pub fn interpret(
    code: Option<i32>,
    signal: Option<i32>,
    timed_out: Option<Duration>,
    stdout: &str,
    stderr: &str,
) -> (Verdict, Vec<Comment>, Option<ReviewerId>, Option<String>) {
    if let Some(timeout) = timed_out {
        return (Verdict::Error, Vec::new(), None, Some(format!("timed out after {}s", timeout.as_secs())));
    }
    let trimmed = stdout.trim();
    let parsed: Option<Output> = if trimmed.starts_with('{') { serde_json::from_str(trimmed).ok() } else { None };
    match code {
        Some(0) => {
            let (comments, reviewer) = parsed.map(|o| (o.comments, o.reviewer)).unwrap_or_default();
            (Verdict::Accepted, comments, reviewer, None)
        }
        Some(1) => match parsed {
            Some(o) if !o.comments.is_empty() => (Verdict::Rejected, o.comments, o.reviewer, None),
            Some(o) => (Verdict::Error, Vec::new(), o.reviewer, Some("reviewer rejected without any comments".into())),
            None => {
                let output = [stdout.trim_end(), stderr.trim_end()]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                if output.trim().is_empty() {
                    (Verdict::Error, Vec::new(), None, Some("reviewer rejected without any comments or output".into()))
                } else {
                    let body = util::tail(&output, OUTPUT_COMMENT_LINES);
                    (Verdict::Rejected, vec![Comment::general(Severity::Error, body)], None, None)
                }
            }
        },
        Some(c) => {
            let detail = util::tail(stderr.trim_end(), 10);
            let msg = if detail.is_empty() {
                format!("reviewer exited with status {c}")
            } else {
                format!("reviewer exited with status {c}: {detail}")
            };
            (Verdict::Error, Vec::new(), None, Some(msg))
        }
        None => (
            Verdict::Error,
            Vec::new(),
            None,
            Some(format!("reviewer was killed by signal {}", signal.unwrap_or(0))),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_with_and_without_json() {
        let (v, c, id, e) = interpret(Some(0), None, None, "", "");
        assert_eq!((v, c.len(), id, e), (Verdict::Accepted, 0, None, None));
        let (v, _, id, _) = interpret(Some(0), None, None, "build ok\n", "");
        assert_eq!((v, id), (Verdict::Accepted, None));
        let json = r#"{"reviewer":{"nick":"otter","model":"m1"},"comments":[{"severity":"note","body":"nice"}]}"#;
        let (v, c, id, _) = interpret(Some(0), None, None, json, "");
        assert_eq!(v, Verdict::Accepted);
        assert_eq!(c[0].severity, Severity::Note);
        assert_eq!(id.unwrap().model.as_deref(), Some("m1"));
    }

    #[test]
    fn reject_with_json_comments() {
        let json = r#"{"comments":[{"file":"a.rs","line":3,"body":"bug"}]}"#;
        let (v, c, _, e) = interpret(Some(1), None, None, json, "");
        assert_eq!(v, Verdict::Rejected);
        assert_eq!(c[0].to_string(), "a.rs:3: error: bug");
        assert!(e.is_none());
    }

    #[test]
    fn reject_without_comments_is_an_error() {
        let (v, _, _, e) = interpret(Some(1), None, None, r#"{"comments":[]}"#, "");
        assert_eq!(v, Verdict::Error);
        assert!(e.unwrap().contains("without any comments"));
        let (v, _, _, _) = interpret(Some(1), None, None, "", "  ");
        assert_eq!(v, Verdict::Error);
    }

    #[test]
    fn plain_command_rejection_uses_output() {
        let (v, c, _, _) = interpret(Some(1), None, None, "running tests\n", "test foo failed\n");
        assert_eq!(v, Verdict::Rejected);
        assert_eq!(c[0].body, "running tests\ntest foo failed");
    }

    #[test]
    fn other_exits_signals_and_timeouts_are_errors() {
        let (v, _, _, e) = interpret(Some(3), None, None, "", "boom");
        assert_eq!(v, Verdict::Error);
        assert_eq!(e.unwrap(), "reviewer exited with status 3: boom");
        let (v, _, _, e) = interpret(None, Some(9), None, "", "");
        assert_eq!(v, Verdict::Error);
        assert!(e.unwrap().contains("signal 9"));
        let (v, _, _, e) = interpret(None, None, Some(Duration::from_secs(5)), "", "");
        assert_eq!(v, Verdict::Error);
        assert_eq!(e.unwrap(), "timed out after 5s");
    }
}
