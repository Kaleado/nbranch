use crate::agent;
use crate::config::{Config, Notify, Stage};
use crate::paths::Root;
use crate::review::{self, StageOutcome, StageRun};
use crate::ticket::{self, Comment, Conflict, FileLock, ReviewResult, Severity, State, Store, Ticket, Verdict};
use crate::{git, notify};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How many times a land is re-picked because the trunk moved under it.
const MAX_ATTEMPTS: u32 = 10;

/// Snapshot the agent's branch and record a new land ticket.
pub fn enqueue(root: &Root, nick: &str) -> Result<Ticket> {
    let golden = root.golden();
    let trunk = root.trunk()?;
    let branch = agent::branch_name(nick);
    let head = git::try_rev_parse(&golden, &format!("refs/heads/{branch}"))
        .with_context(|| format!("no branch {branch}; run `nbranch new` first"))?;
    let trunk_sha = git::rev_parse(&golden, &format!("refs/heads/{trunk}"))?;
    let base = git::run(&golden, &["merge-base", &trunk_sha, &head])?;
    let commits: usize = git::run(&golden, &["rev-list", "--count", &format!("{base}..{head}")])?.parse()?;
    if commits == 0 {
        bail!("nothing to land: {branch} has no commits that aren't on {trunk}");
    }
    let store = Store::new(root);
    store.locked(|| {
        store.reap()?;
        if let Some(t) = store.all()?.into_iter().find(|t| t.agent == nick && !t.state.is_terminal()) {
            bail!(
                "land #{} is already in progress ({}); wait for its result or run `nbranch cancel {}`",
                t.id,
                t.state,
                t.id
            );
        }
        let id = store.next_counter("next-ticket")?;
        // Keeps the snapshot's objects alive whatever happens to the branch.
        git::run(&golden, &["update-ref", &format!("refs/nbranch/tickets/{id}"), &head])?;
        let mut t = Ticket::new(id, nick, &branch, &head, &base, commits);
        store.save(&mut t)?;
        Ok(t)
    })
}

/// Start a detached worker process for a ticket and return its pid.
pub fn spawn_worker(root: &Root, id: u64) -> Result<u32> {
    std::fs::create_dir_all(root.logs_dir())?;
    let log = File::create(root.logs_dir().join(format!("{id}.log")))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("--root")
        .arg(&root.dir)
        .arg("__worker")
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Survive the lander's shell or tool call going away.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let pid = cmd.spawn().context("starting land worker")?.id();
    Store::new(root).update(id, |t| t.pid = Some(pid))?;
    Ok(pid)
}

/// Process a ticket to completion. Notifies the lander unless they're
/// waiting for the result in the foreground.
pub fn run_worker(root: &Root, id: u64, send_notification: bool) -> Result<Ticket> {
    let store = Store::new(root);
    let result = pipeline(root, &store, id);
    for suffix in ["pre", "pick"] {
        let wt = root.tmp_dir().join(format!("{id}-{suffix}"));
        if wt.exists() {
            git::remove_worktree(&root.golden(), &wt);
        }
    }
    let t = match result {
        Ok(()) => store.load(id)?,
        Err(e) => store.update(id, |t| {
            if !t.state.is_terminal() {
                t.state = State::Error;
                t.detail = None;
                t.error = Some(format!("{e:#}"));
            }
        })?,
    };
    if send_notification {
        let trunk = root.trunk().unwrap_or_else(|_| "trunk".into());
        let notify = Config::load(root, &trunk)
            .map(|c| c.notify)
            .unwrap_or(Notify::Nchat { as_nick: "nbranch".into() });
        let extra = [
            ("NBRANCH_EVENT", "land-result".to_string()),
            ("NBRANCH_TICKET", t.id.to_string()),
            ("NBRANCH_STATE", t.state.to_string()),
        ];
        if let Err(e) = notify::send(&notify, &t.agent, &summary(&t, &trunk), &extra) {
            eprintln!("warning: could not notify {}: {e:#}", t.agent);
        }
    }
    Ok(t)
}

fn finish(store: &Store, id: u64, state: State, f: impl FnOnce(&mut Ticket)) -> Result<()> {
    store.update(id, |t| {
        t.state = state;
        t.detail = None;
        f(t);
    })?;
    Ok(())
}

fn synthetic_rejection(body: String) -> ReviewResult {
    ReviewResult {
        name: "nbranch".into(),
        stage: Stage::PreQueue,
        attempt: 1,
        verdict: Verdict::Rejected,
        exit_code: None,
        duration_ms: 0,
        reviewer: None,
        comments: vec![Comment::general(Severity::Error, body)],
        error: None,
        stdout: None,
        stderr: None,
    }
}

/// A rejection outranks a reviewer error: it's actionable feedback.
fn stage_failure(results: &[ReviewResult]) -> Option<State> {
    if results.iter().any(|r| r.verdict == Verdict::Rejected) {
        Some(State::Rejected)
    } else if results.iter().any(|r| r.verdict == Verdict::Error) {
        Some(State::ReviewerError)
    } else {
        None
    }
}

enum Advance {
    Landed,
    Moved,
    Cancelled,
}

fn pipeline(root: &Root, store: &Store, id: u64) -> Result<()> {
    let t = store.update(id, |t| t.pid = Some(std::process::id()))?;
    if t.state.is_terminal() {
        return Ok(());
    }
    let golden = root.golden();
    let trunk = root.trunk()?;
    let trunk_ref = format!("refs/heads/{trunk}");
    let cfg = Config::load(root, &trunk)?;
    let range = format!("{}..{}", t.base, t.head);

    // 1. Config protection: .nbranch may only change directly on the trunk.
    let touched = git::run(&golden, &["log", "--format=%h %s", &range, "--", ".nbranch"])?;
    if !touched.is_empty() {
        let body = format!("This stack changes .nbranch, which may only be changed directly on the trunk:\n{touched}");
        return finish(store, id, State::Rejected, |t| t.reviews.push(synthetic_rejection(body)));
    }
    if !git::run(&golden, &["rev-list", "--merges", &range])?.is_empty() {
        let body = "This stack contains merge commits. Run `nbranch sync` to rebase it onto the trunk, then land again.";
        return finish(store, id, State::Rejected, |t| t.reviews.push(synthetic_rejection(body.into())));
    }

    // 2. Pre-queue review of the snapshot, outside the queue lock.
    let mut accepted = Vec::new();
    let pre = cfg.stage(Stage::PreQueue);
    if !pre.is_empty() {
        if store.load(id)?.cancel_requested {
            return finish(store, id, State::Cancelled, |_| {});
        }
        let wt = root.tmp_dir().join(format!("{id}-pre"));
        git::add_detached_worktree(&golden, &wt, &t.head)?;
        agent::run_setup(&cfg, root, &wt, "review")?;
        let run = StageRun {
            root,
            store,
            ticket: id,
            agent: &t.agent,
            stage: Stage::PreQueue,
            attempt: 1,
            worktree: &wt,
            base: &t.base,
            head: &t.head,
        };
        let outcome = review::run_stage(&run, &pre);
        git::remove_worktree(&golden, &wt);
        match outcome? {
            StageOutcome::Cancelled => return finish(store, id, State::Cancelled, |_| {}),
            StageOutcome::Done(results) => {
                let failed = stage_failure(&results);
                store.update(id, |t| t.reviews.extend(results.iter().cloned()))?;
                if let Some(state) = failed {
                    return finish(store, id, state, |_| {});
                }
                accepted.extend(results);
            }
        }
    }

    // 3. Join the queue and wait for our turn.
    let queued = store.locked(|| {
        let mut t = store.load(id)?;
        if t.cancel_requested {
            return Ok(false);
        }
        t.queue_seq = Some(store.next_counter("next-queue-seq")?);
        t.state = State::Queued;
        t.detail = None;
        store.save(&mut t)?;
        Ok(true)
    })?;
    if !queued {
        return finish(store, id, State::Cancelled, |_| {});
    }
    let Some(_queue_lock) = wait_turn(root, store, id)? else {
        return finish(store, id, State::Cancelled, |_| {});
    };

    let in_queue = cfg.stage(Stage::InQueue);
    let mut attempt = 0;
    loop {
        attempt += 1;
        if attempt > MAX_ATTEMPTS {
            bail!("the trunk moved {MAX_ATTEMPTS} times while this land was in progress; giving up");
        }

        // 4. Pick the stack onto the current trunk, without touching it.
        let onto = git::rev_parse(&golden, &trunk_ref)?;
        store.update(id, |t| {
            t.state = State::Picking;
            t.attempts = attempt;
            t.trunk_before = Some(onto.clone());
            t.detail = Some(format!("picking {} commit(s) onto {}", t.commits, git::short(&onto)));
        })?;
        let picks = match pick(&golden, &t.base, &t.head, &onto)? {
            Picked::Conflict(c) => return finish(store, id, State::Conflict, |t| t.conflict = Some(c)),
            Picked::Commits(p) => p,
        };
        if picks.is_empty() {
            return finish(store, id, State::Landed, |t| {
                t.trunk_after = Some(onto.clone());
                t.landed.clear();
            });
        }
        let picked_head = picks.last().expect("non-empty").commit.clone();

        // 5. In-queue review of the merged result.
        let mut in_results = Vec::new();
        if !in_queue.is_empty() {
            store.update(id, |t| t.state = State::Testing)?;
            let wt = root.tmp_dir().join(format!("{id}-pick"));
            git::add_detached_worktree(&golden, &wt, &picked_head)?;
            agent::run_setup(&cfg, root, &wt, "review")?;
            let run = StageRun {
                root,
                store,
                ticket: id,
                agent: &t.agent,
                stage: Stage::InQueue,
                attempt,
                worktree: &wt,
                base: &onto,
                head: &picked_head,
            };
            let outcome = review::run_stage(&run, &in_queue);
            git::remove_worktree(&golden, &wt);
            match outcome? {
                StageOutcome::Cancelled => return finish(store, id, State::Cancelled, |_| {}),
                StageOutcome::Done(results) => {
                    let failed = stage_failure(&results);
                    store.update(id, |t| t.reviews.extend(results.iter().cloned()))?;
                    if let Some(state) = failed {
                        return finish(store, id, state, |_| {});
                    }
                    in_results = results;
                }
            }
        }

        // 6. Recommit with provenance trailers (same trees as reviewed) and
        //    advance the trunk with a compare-and-swap.
        let trailers = trailers(&t.agent, id, accepted.iter().chain(in_results.iter()));
        let mut new_head = onto.clone();
        let mut landed = Vec::new();
        for p in &picks {
            new_head = commit_tree(&golden, &p.tree, &new_head, &p.source, &trailers)?;
            landed.push(new_head.clone());
        }
        let advance = store.locked(|| {
            let mut t = store.load(id)?;
            if t.cancel_requested {
                return Ok(Advance::Cancelled);
            }
            let msg = format!("nbranch: land #{id} from {}", t.agent);
            let out = git::output(&golden, &["update-ref", "-m", &msg, &trunk_ref, &new_head, &onto])?;
            if out.status.success() {
                t.state = State::Landed;
                t.detail = None;
                t.trunk_after = Some(new_head.clone());
                t.landed = landed.clone();
                store.save(&mut t)?;
                return Ok(Advance::Landed);
            }
            if git::rev_parse(&golden, &trunk_ref)? != onto {
                return Ok(Advance::Moved);
            }
            bail!("updating {trunk_ref} failed: {}", String::from_utf8_lossy(&out.stderr).trim())
        })?;
        match advance {
            Advance::Landed => return Ok(()),
            Advance::Cancelled => return finish(store, id, State::Cancelled, |_| {}),
            Advance::Moved => {
                store.update(id, |t| t.detail = Some("trunk moved; picking again".into()))?;
            }
        }
    }
}

/// Wait until this ticket is the oldest queued one and holds the queue lock.
/// Returns `None` if the land was cancelled while waiting.
fn wait_turn(root: &Root, store: &Store, id: u64) -> Result<Option<FileLock>> {
    let lock_path = root.state().join("queue.lock");
    loop {
        let my_turn = store.locked(|| {
            store.reap()?;
            let all = store.all()?;
            let me = all.iter().find(|t| t.id == id).context("ticket vanished")?;
            if me.cancel_requested {
                return Ok(None);
            }
            let first = all.iter().filter(|t| t.state == State::Queued).filter_map(|t| t.queue_seq).min();
            Ok(Some(first == me.queue_seq))
        })?;
        match my_turn {
            None => return Ok(None),
            Some(true) => {
                if let Some(lock) = ticket::lock_file(&lock_path, false)? {
                    return Ok(Some(lock));
                }
            }
            Some(false) => {}
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

struct Pick {
    source: String,
    tree: String,
    commit: String,
}

enum Picked {
    Commits(Vec<Pick>),
    Conflict(Conflict),
}

/// Cherry-pick `base..head` onto `onto` in memory (no worktree, no ref
/// changes). Commits that become empty are dropped.
fn pick(golden: &Path, base: &str, head: &str, onto: &str) -> Result<Picked> {
    let list = git::run(golden, &["rev-list", "--reverse", &format!("{base}..{head}")])?;
    let mut cur = onto.to_string();
    let mut picks = Vec::new();
    for c in list.lines() {
        let merge_base = format!("--merge-base={c}^");
        let out = git::output(
            golden,
            &["merge-tree", "--write-tree", "--name-only", "--no-messages", &merge_base, &cur, c],
        )?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        let tree = lines.next().unwrap_or("").trim().to_string();
        match out.status.code() {
            Some(0) => {}
            Some(1) => {
                let mut files: Vec<String> =
                    lines.map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
                files.dedup();
                let subject = git::run(golden, &["log", "-1", "--format=%s", c])?;
                return Ok(Picked::Conflict(Conflict { commit: c.to_string(), subject, files }));
            }
            _ => bail!("git merge-tree failed for {c}: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }
        if tree == git::run(golden, &["rev-parse", &format!("{cur}^{{tree}}")])? {
            continue;
        }
        cur = commit_tree(golden, &tree, &cur, c, &[])?;
        picks.push(Pick { source: c.to_string(), tree, commit: cur.clone() });
    }
    Ok(Picked::Commits(picks))
}

/// Create a commit with `tree` on `parent`, keeping `source`'s author and
/// message and appending `trailers`.
fn commit_tree(golden: &Path, tree: &str, parent: &str, source: &str, trailers: &[String]) -> Result<String> {
    let meta = git::run(golden, &["log", "-1", "--date=raw", "--format=%an%x00%ae%x00%ad", source])?;
    let mut parts = meta.split('\0');
    let (name, email, date) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    let mut message = git::run(golden, &["log", "-1", "--format=%B", source])?;
    if !trailers.is_empty() {
        let mut args = vec!["interpret-trailers"];
        for t in trailers {
            args.push("--trailer");
            args.push(t);
        }
        message = git::run_with_input(golden, &args, format!("{message}\n").as_bytes(), &[])?;
    }
    message.push('\n');
    git::run_with_input(
        golden,
        &["commit-tree", tree, "-p", parent],
        message.as_bytes(),
        &[
            ("GIT_AUTHOR_NAME", name),
            ("GIT_AUTHOR_EMAIL", email),
            ("GIT_AUTHOR_DATE", date),
            ("GIT_COMMITTER_NAME", "nbranch"),
            ("GIT_COMMITTER_EMAIL", "nbranch@localhost"),
        ],
    )
}

fn trailers<'a>(agent: &str, id: u64, accepted: impl Iterator<Item = &'a ReviewResult>) -> Vec<String> {
    let mut out = vec![format!("Nbranch-Agent: {agent}"), format!("Nbranch-Ticket: {id}")];
    for r in accepted {
        out.push(match &r.reviewer {
            Some(who) => match &who.model {
                Some(model) => format!("Reviewed-by: {} [{}, {}]", r.name, who.nick, model),
                None => format!("Reviewed-by: {} [{}]", r.name, who.nick),
            },
            None => format!("Reviewed-by: {}", r.name),
        });
    }
    out
}

/// The notification sent to the lander when a land finishes.
pub fn summary(t: &Ticket, trunk: &str) -> String {
    let id = t.id;
    match t.state {
        State::Landed if t.landed.is_empty() => format!(
            "nbranch: land #{id} had nothing to land; all of its changes are already on {trunk}. Run `nbranch reset` before starting new work."
        ),
        State::Landed => format!(
            "nbranch: land #{id} LANDED {} commit(s) onto {trunk} (now at {}). Run `nbranch reset` before starting new work.",
            t.landed.len(),
            git::short(t.trunk_after.as_deref().unwrap_or("")),
        ),
        State::Rejected => {
            let failed: Vec<&ReviewResult> = t.reviews.iter().filter(|r| r.verdict == Verdict::Rejected).collect();
            let names: Vec<&str> = failed.iter().map(|r| r.name.as_str()).collect();
            let mut msg = format!("nbranch: land #{id} REJECTED by {}.\n", names.join(", "));
            let comments: Vec<String> = failed
                .iter()
                .flat_map(|r| r.comments.iter().map(move |c| format!("[{}] {c}", r.name)))
                .collect();
            for c in comments.iter().take(20) {
                msg.push_str(c);
                msg.push('\n');
            }
            if comments.len() > 20 {
                msg.push_str(&format!("... and {} more\n", comments.len() - 20));
            }
            msg.push_str(&format!(
                "Address the comments, commit, and run `nbranch land` again. Full reviewer output: `nbranch show {id}`."
            ));
            msg
        }
        State::Conflict => {
            let c = t.conflict.as_ref();
            format!(
                "nbranch: land #{id} CONFLICT: commit {} ({}) conflicts with {trunk} in: {}. The trunk is unchanged. Run `nbranch sync`, resolve the conflicts, then `nbranch land` again.",
                c.map(|c| git::short(&c.commit)).unwrap_or("?"),
                c.map(|c| c.subject.as_str()).unwrap_or(""),
                c.map(|c| c.files.join(", ")).unwrap_or_default(),
            )
        }
        State::ReviewerError => {
            let errors: Vec<String> = t
                .reviews
                .iter()
                .filter(|r| r.verdict == Verdict::Error)
                .map(|r| format!("{}: {}", r.name, r.error.as_deref().unwrap_or("error")))
                .collect();
            format!(
                "nbranch: land #{id} failed with a REVIEWER ERROR ({}). This is not a judgement on your change; report it rather than changing your code.",
                errors.join("; ")
            )
        }
        State::Cancelled => format!("nbranch: land #{id} was cancelled."),
        _ => format!(
            "nbranch: land #{id} failed: {}",
            t.error.as_deref().unwrap_or(t.state.as_str())
        ),
    }
}
