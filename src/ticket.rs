use crate::{config::Stage, paths::Root, util};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Running pre-queue reviewers.
    Reviewing,
    /// Waiting for the queue lock.
    Queued,
    /// Cherry-picking onto the trunk.
    Picking,
    /// Running in-queue reviewers on the picked result.
    Testing,
    Landed,
    Rejected,
    Conflict,
    ReviewerError,
    Cancelled,
    /// The worker process died before finishing.
    Abandoned,
    /// An internal error, e.g. bad config.
    Error,
}

impl State {
    pub fn is_terminal(self) -> bool {
        !matches!(self, State::Reviewing | State::Queued | State::Picking | State::Testing)
    }

    pub fn is_failure(self) -> bool {
        matches!(self, State::Rejected | State::Conflict | State::ReviewerError)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Reviewing => "reviewing",
            State::Queued => "queued",
            State::Picking => "picking",
            State::Testing => "testing",
            State::Landed => "landed",
            State::Rejected => "rejected",
            State::Conflict => "conflict",
            State::ReviewerError => "reviewer-error",
            State::Cancelled => "cancelled",
            State::Abandoned => "abandoned",
            State::Error => "error",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Error,
    Warning,
    Note,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }

    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim() {
            "error" => Some(Severity::Error),
            "warning" => Some(Severity::Warning),
            "note" => Some(Severity::Note),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Comment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(default)]
    pub severity: Severity,
    pub body: String,
}

impl Comment {
    pub fn general(severity: Severity, body: impl Into<String>) -> Comment {
        Comment { file: None, line: None, severity, body: body.into() }
    }
}

impl fmt::Display for Comment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => write!(f, "{file}:{line}: ")?,
            (Some(file), None) => write!(f, "{file}: ")?,
            _ => {}
        }
        write!(f, "{}: {}", self.severity.as_str(), self.body)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReviewerId {
    pub nick: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Accepted,
    Rejected,
    Error,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReviewResult {
    pub name: String,
    pub stage: Stage,
    pub attempt: u32,
    pub verdict: Verdict,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub reviewer: Option<ReviewerId>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub stdout: Option<PathBuf>,
    #[serde(default)]
    pub stderr: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Conflict {
    pub commit: String,
    pub subject: String,
    pub files: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ticket {
    pub id: u64,
    pub agent: String,
    pub branch: String,
    /// Snapshot of the branch when `nbranch land` ran.
    pub head: String,
    /// Merge-base of `head` with the trunk at enqueue time.
    pub base: String,
    pub commits: usize,
    pub state: State,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub queue_seq: Option<u64>,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub trunk_before: Option<String>,
    #[serde(default)]
    pub trunk_after: Option<String>,
    #[serde(default)]
    pub landed: Vec<String>,
    #[serde(default)]
    pub conflict: Option<Conflict>,
    #[serde(default)]
    pub reviews: Vec<ReviewResult>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Ticket {
    pub fn new(id: u64, agent: &str, branch: &str, head: &str, base: &str, commits: usize) -> Ticket {
        Ticket {
            id,
            agent: agent.into(),
            branch: branch.into(),
            head: head.into(),
            base: base.into(),
            commits,
            state: State::Reviewing,
            detail: None,
            pid: None,
            created: util::now(),
            updated: util::now(),
            queue_seq: None,
            cancel_requested: false,
            attempts: 0,
            trunk_before: None,
            trunk_after: None,
            landed: Vec::new(),
            conflict: None,
            reviews: Vec::new(),
            error: None,
        }
    }
}

/// An exclusive `flock` held until dropped (or until the process dies).
pub struct FileLock {
    _file: File,
}

pub fn lock_file(path: &Path, blocking: bool) -> Result<Option<FileLock>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let op = libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return Ok(Some(FileLock { _file: file }));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) if !blocking => return Ok(None),
            _ => return Err(err).with_context(|| format!("locking {}", path.display())),
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    // A zombie still answers kill(0) but will never make progress.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(state) = stat.rsplit(')').next().and_then(|s| s.trim().chars().next()) {
            return state != 'Z' && state != 'X';
        }
    }
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Ticket files live in `golden.git/nbranch/tickets/<id>.json`. Writes are
/// atomic renames; read-modify-write sequences go through `locked`.
pub struct Store {
    root: Root,
}

impl Store {
    pub fn new(root: &Root) -> Store {
        Store { root: root.clone() }
    }

    fn path(&self, id: u64) -> PathBuf {
        self.root.tickets_dir().join(format!("{id}.json"))
    }

    pub fn load(&self, id: u64) -> Result<Ticket> {
        let path = self.path(id);
        let data = std::fs::read(&path).with_context(|| format!("no such land #{id}"))?;
        serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, t: &mut Ticket) -> Result<()> {
        t.updated = util::now();
        std::fs::create_dir_all(self.root.tickets_dir())?;
        util::write_atomic(&self.path(t.id), &serde_json::to_vec_pretty(t)?)
    }

    pub fn all(&self) -> Result<Vec<Ticket>> {
        let mut tickets = Vec::new();
        let dir = match std::fs::read_dir(self.root.tickets_dir()) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(tickets),
            Err(e) => return Err(e.into()),
        };
        for entry in dir {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|d| Ok(serde_json::from_slice(&d)?)) {
                Ok(t) => tickets.push(t),
                Err(e) => eprintln!("warning: skipping {}: {e:#}", path.display()),
            }
        }
        tickets.sort_by_key(|t: &Ticket| t.id);
        Ok(tickets)
    }

    /// Run `f` while holding the short-lived state lock. Don't call `locked`
    /// or `update` from inside `f`.
    pub fn locked<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        std::fs::create_dir_all(self.root.state())?;
        let _lock = lock_file(&self.root.state().join("state.lock"), true)?;
        f()
    }

    pub fn update(&self, id: u64, f: impl FnOnce(&mut Ticket)) -> Result<Ticket> {
        self.locked(|| {
            let mut t = self.load(id)?;
            f(&mut t);
            self.save(&mut t)?;
            Ok(t)
        })
    }

    /// Increment and return a named counter. Call with the state lock held.
    pub fn next_counter(&self, name: &str) -> Result<u64> {
        let path = self.root.state().join(name);
        let n = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        util::write_atomic(&path, n.to_string().as_bytes())?;
        Ok(n)
    }

    /// Mark unfinished tickets whose worker has died as abandoned (or
    /// cancelled, if that was requested). Call with the state lock held.
    pub fn reap(&self) -> Result<Vec<u64>> {
        let mut reaped = Vec::new();
        for mut t in self.all()? {
            if t.state.is_terminal() {
                continue;
            }
            let dead = match t.pid {
                Some(pid) => !pid_alive(pid),
                // The worker records its pid immediately; allow for a slow start.
                None => util::now().saturating_sub(t.created) > 60,
            };
            if dead {
                if t.cancel_requested {
                    t.state = State::Cancelled;
                } else {
                    t.state = State::Abandoned;
                    t.error = Some("the worker process exited before the land finished".into());
                }
                t.detail = None;
                self.save(&mut t)?;
                reaped.push(t.id);
            }
        }
        Ok(reaped)
    }
}
