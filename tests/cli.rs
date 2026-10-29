//! End-to-end tests driving the real binary against throwaway repositories.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_nbranch");

struct Env {
    tmp: TempDir,
    root: PathBuf,
    notify_log: PathBuf,
    clones: AtomicUsize,
}

fn configure(cmd: &mut Command, home: &Path) {
    let bin_dir = Path::new(BIN).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    cmd.env("PATH", path)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .env("GIT_EDITOR", "true")
        .env_remove("NBRANCH_ROOT")
        .env_remove("NBRANCH_AGENT")
        .env_remove("NBRANCH_MODEL")
        .env_remove("NCHAT_NICK")
        .env_remove("NCHAT_SESSION")
        .env_remove("NQUEUE_ID")
        .env_remove("NQUEUE_HOME")
        .env_remove("NQUEUE_VALKEY");
}

fn check(what: &str, out: &Output) -> String {
    assert!(
        out.status.success(),
        "{what} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let notify_log = home.join("notify.log");
        std::fs::create_dir_all(home.join("xdg/nbranch")).unwrap();
        std::fs::write(
            home.join("xdg/nbranch/config"),
            format!(
                "notify_command = 'printf \"%s\\t%s\\n\" \"$NBRANCH_TO\" \"$NBRANCH_MESSAGE\" >> {}'\n",
                notify_log.display()
            ),
        )
        .unwrap();
        let env = Env { root: home.join("root"), tmp, notify_log, clones: AtomicUsize::new(0) };
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        env.git(&src, &["init", "-q", "-b", "master"]);
        std::fs::write(src.join("README"), "hello\n").unwrap();
        std::fs::write(src.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        env.git(&src, &["add", "."]);
        env.git(&src, &["commit", "-qm", "Initial commit"]);
        let out = env.cmd_in(&home, None, &["init", env.root.to_str().unwrap(), "--from", src.to_str().unwrap()]);
        check("nbranch init", &out);
        env
    }

    fn home(&self) -> &Path {
        self.tmp.path()
    }

    fn golden(&self) -> PathBuf {
        self.root.join("golden.git")
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        configure(&mut cmd, self.home());
        check(&format!("git {}", args.join(" ")), &cmd.output().unwrap()).trim_end().to_string()
    }

    fn cmd_in(&self, dir: &Path, agent: Option<&str>, args: &[&str]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args).current_dir(dir);
        configure(&mut cmd, self.home());
        if let Some(a) = agent {
            cmd.env("NBRANCH_AGENT", a);
        }
        cmd.output().unwrap()
    }

    fn nb(&self, agent: &str, args: &[&str]) -> Output {
        self.cmd_in(&self.root, Some(agent), args)
    }

    fn nb_ok(&self, agent: &str, args: &[&str]) -> String {
        check(&format!("nbranch {}", args.join(" ")), &self.nb(agent, args))
    }

    fn worktree(&self, nick: &str) -> PathBuf {
        self.root.join("agents").join(nick)
    }

    /// Create the agent's worktree if needed and commit a file in it.
    fn commit(&self, nick: &str, file: &str, content: &str, msg: &str) {
        if !self.worktree(nick).exists() {
            self.nb_ok(nick, &["new"]);
        }
        let wt = self.worktree(nick);
        std::fs::write(wt.join(file), content).unwrap();
        self.git(&wt, &["add", file]);
        self.git(&wt, &["commit", "-qm", msg]);
    }

    fn land_wait(&self, nick: &str) -> (i32, Value) {
        let out = self.nb(nick, &["land", "--wait", "--json"]);
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "land output wasn't JSON:\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap(), json)
    }

    fn show(&self, id: u64) -> Value {
        serde_json::from_str(&self.nb_ok("observer", &["show", &id.to_string(), "--json"])).unwrap()
    }

    fn wait_for(&self, id: u64, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let t = self.show(id);
            if done(&t) {
                return t;
            }
            assert!(Instant::now() < deadline, "timed out waiting on land #{id}: {t:#}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_terminal(&self, id: u64) -> Value {
        self.wait_for(id, |t| {
            !matches!(t["state"].as_str(), Some("reviewing" | "queued" | "picking" | "testing"))
        })
    }

    fn trunk(&self) -> String {
        self.git(&self.golden(), &["rev-parse", "refs/heads/master"])
    }

    fn trunk_subjects(&self) -> Vec<String> {
        self.git(&self.golden(), &["log", "--format=%s", "master"]).lines().map(String::from).collect()
    }

    fn trunk_file(&self, file: &str) -> Option<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.golden()).args(["show", &format!("master:{file}")]);
        configure(&mut cmd, self.home());
        let out = cmd.output().unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// A human-style clone of golden, for committing straight to the trunk.
    fn human_clone(&self) -> PathBuf {
        let n = self.clones.fetch_add(1, Ordering::SeqCst);
        let dir = self.home().join(format!("human{n}"));
        self.git(self.home(), &["clone", "-q", self.golden().to_str().unwrap(), dir.to_str().unwrap()]);
        dir
    }

    fn human_commit(&self, file: &str, content: &str, msg: &str) {
        let dir = self.human_clone();
        std::fs::write(dir.join(file), content).unwrap();
        self.git(&dir, &["add", file]);
        self.git(&dir, &["commit", "-qm", msg]);
        self.git(&dir, &["push", "-q", "origin", "HEAD:master"]);
    }

    /// Start a private nqueue server (in $HOME/.nqueue) with the review policy.
    fn start_nqueue(&self) {
        let dir = self.home().join(".nqueue");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[policies.nbranch-review]\nmode = \"compete\"\ndelivery = \"at-least-once\"\nlease-secs = 600\nmax-in-flight = 1\n",
        )
        .unwrap();
        let mut cmd = Command::new(nqueue_bin());
        cmd.arg("daemon").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        configure(&mut cmd, self.home());
        cmd.spawn().expect("starting nqueue daemon (is nqueue installed?)");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.nq(None, &["status"]).status.success() {
            assert!(Instant::now() < deadline, "nqueue daemon didn't start");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn nq(&self, id: Option<&str>, args: &[&str]) -> Output {
        let mut cmd = Command::new(nqueue_bin());
        cmd.args(args).current_dir(self.home());
        configure(&mut cmd, self.home());
        if let Some(id) = id {
            cmd.env("NQUEUE_ID", id);
        }
        cmd.output().unwrap()
    }

    fn nq_ok(&self, id: Option<&str>, args: &[&str]) -> String {
        check(&format!("nqueue {}", args.join(" ")), &self.nq(id, args)).trim_end().to_string()
    }

    /// Wait for a message to be delivered to the agent `id` and lease it.
    fn dequeue(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let out = self.nq_ok(Some(id), &["dequeue", "--max", "1"]);
            if let Some(line) = out.lines().next() {
                return serde_json::from_str(line).unwrap();
            }
            assert!(Instant::now() < deadline, "nothing was delivered to {id}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn set_repo_config(&self, toml: &str) {
        self.human_commit(".nbranch", toml, "Configure nbranch");
    }

    fn notifications(&self) -> String {
        std::fs::read_to_string(&self.notify_log).unwrap_or_default()
    }

    /// Notifications are sent just after a land finishes, so poll for them.
    fn wait_notification(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let notes = self.notifications();
            if notes.contains(needle) {
                return notes;
            }
            assert!(Instant::now() < deadline, "no notification containing {needle:?}:\n{notes}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let sock = self.home().join(".nqueue/valkey.sock");
        if sock.exists() {
            let _ = Command::new("pkill").arg("-f").arg(sock).status();
        }
    }
}

fn nqueue_bin() -> String {
    std::env::var("NQUEUE_BIN").unwrap_or_else(|_| "nqueue".into())
}

fn reviews(t: &Value) -> &Vec<Value> {
    t["reviews"].as_array().unwrap()
}

#[test]
fn lands_a_stack_with_trailers_and_resets() {
    let env = Env::new();
    env.set_repo_config(
        "[[reviewer]]\nname = \"lint\"\nstage = \"pre-queue\"\ncommand = \"true\"\n\
         [[reviewer]]\nname = \"tests\"\nstage = \"in-queue\"\ncommand = \"true\"\n",
    );
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.commit("alice", "b.txt", "b\n", "Add b");
    let branch_head = env.git(&env.worktree("alice"), &["rev-parse", "HEAD"]);

    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 0, "{t:#}");
    assert_eq!(t["state"], "landed");
    assert_eq!(t["landed"].as_array().unwrap().len(), 2);
    assert_eq!(env.trunk(), t["trunk_after"].as_str().unwrap());
    assert_eq!(&env.trunk_subjects()[..3], ["Add b", "Add a", "Configure nbranch"]);

    let msg = env.git(&env.golden(), &["log", "-1", "--format=%B", "master"]);
    assert!(msg.contains("Nbranch-Agent: alice"), "{msg}");
    assert!(msg.contains("Nbranch-Ticket: 1"), "{msg}");
    assert!(msg.contains("Reviewed-by: lint\n"), "{msg}");
    assert!(msg.contains("Reviewed-by: tests"), "{msg}");
    let author = env.git(&env.golden(), &["log", "-1", "--format=%an <%ae> / %cn", "master"]);
    assert_eq!(author, "Test Author <author@example.com> / nbranch");

    // The agent's worktree doesn't change under its feet...
    assert_eq!(env.git(&env.worktree("alice"), &["rev-parse", "HEAD"]), branch_head);
    // ...until it resets, which succeeds because everything landed.
    env.nb_ok("alice", &["reset"]);
    assert_eq!(env.git(&env.worktree("alice"), &["rev-parse", "HEAD"]), env.trunk());

    let out = env.nb("alice", &["land", "--wait"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to land"));
}

#[test]
fn conflicting_stack_is_rejected_atomically_and_sync_recovers() {
    let env = Env::new();
    env.nb_ok("bob", &["new"]);
    env.commit("alice", "shared.txt", "one\nALICE\nthree\n", "Alice edits shared");
    env.commit("bob", "g.txt", "g\n", "Bob adds g");
    env.commit("bob", "shared.txt", "one\nBOB\nthree\n", "Bob edits shared");

    assert_eq!(env.land_wait("alice").0, 0);
    let trunk_before = env.trunk();
    let (code, t) = env.land_wait("bob");
    assert_eq!(code, 1, "{t:#}");
    assert_eq!(t["state"], "conflict");
    assert_eq!(t["conflict"]["subject"], "Bob edits shared");
    assert_eq!(t["conflict"]["files"], serde_json::json!(["shared.txt"]));
    // Bob's first commit applied cleanly but must not land on its own.
    assert_eq!(env.trunk(), trunk_before);
    assert!(env.trunk_file("g.txt").is_none());
    assert!(env.notifications().is_empty(), "--wait doesn't notify");

    let out = env.nb("bob", &["sync"]);
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stdout));
    let wt = env.worktree("bob");
    std::fs::write(wt.join("shared.txt"), "one\nALICE AND BOB\nthree\n").unwrap();
    env.git(&wt, &["add", "shared.txt"]);
    env.git(&wt, &["rebase", "--continue"]);

    let (code, t) = env.land_wait("bob");
    assert_eq!(code, 0, "{t:#}");
    assert_eq!(env.trunk_file("shared.txt").unwrap(), "one\nALICE AND BOB\nthree\n");
    assert_eq!(env.trunk_file("g.txt").unwrap(), "g\n");
}

#[test]
fn sync_rebases_onto_new_trunk() {
    let env = Env::new();
    env.nb_ok("bob", &["new"]);
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.commit("bob", "b.txt", "b\n", "Add b");
    assert_eq!(env.land_wait("alice").0, 0);
    env.nb_ok("bob", &["sync"]);
    let wt = env.worktree("bob");
    assert!(wt.join("a.txt").exists());
    assert_eq!(env.git(&wt, &["rev-parse", "HEAD~1"]), env.trunk());
}

#[test]
fn changes_to_nbranch_config_are_rejected() {
    let env = Env::new();
    env.commit("alice", ".nbranch", "[[reviewer]]\nname=\"x\"\nstage=\"in-queue\"\ncommand=\"true\"\n", "Sneaky");
    let trunk = env.trunk();
    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 1);
    assert_eq!(t["state"], "rejected");
    assert_eq!(reviews(&t)[0]["name"], "nbranch");
    assert!(reviews(&t)[0]["comments"][0]["body"].as_str().unwrap().contains(".nbranch"));
    assert_eq!(env.trunk(), trunk);
}

#[test]
fn rejections_collect_all_reviewer_comments_and_are_logged() {
    let env = Env::new();
    env.set_repo_config(
        r#"
[[reviewer]]
name = "style"
stage = "pre-queue"
command = '''echo '{"reviewer":{"nick":"styler"},"comments":[{"file":"a.txt","line":1,"severity":"warning","body":"shouty"}]}'; exit 1'''

[[reviewer]]
name = "ok"
stage = "pre-queue"
command = "true"

[[reviewer]]
name = "plain"
stage = "pre-queue"
command = "echo 'running checks'; echo 'check foo FAILED' >&2; exit 1"

[[reviewer]]
name = "never"
stage = "in-queue"
command = "touch $HOME/in-queue-ran"
"#,
    );
    env.commit("alice", "a.txt", "A\n", "Add a");
    let trunk = env.trunk();
    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 1, "{t:#}");
    assert_eq!(t["state"], "rejected");
    assert_eq!(env.trunk(), trunk);
    assert!(!env.home().join("in-queue-ran").exists(), "in-queue must not run after a pre-queue rejection");

    let by_name = |n: &str| reviews(&t).iter().find(|r| r["name"] == n).unwrap().clone();
    assert_eq!(by_name("ok")["verdict"], "accepted");
    let style = by_name("style");
    assert_eq!(style["verdict"], "rejected");
    assert_eq!(style["reviewer"]["nick"], "styler");
    assert_eq!(style["comments"][0]["severity"], "warning");
    let plain = by_name("plain");
    assert_eq!(plain["verdict"], "rejected");
    assert!(plain["comments"][0]["body"].as_str().unwrap().contains("check foo FAILED"));

    // The rejection log keeps the comments and the reviewers' raw output.
    let log = env.nb_ok("observer", &["log"]);
    assert!(log.contains("#1 alice  rejected"), "{log}");
    assert!(log.contains("a.txt:1: warning: shouty"), "{log}");
    assert!(log.contains("| check foo FAILED"), "{log}");
    let stderr_path = plain["stderr"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(stderr_path).unwrap(), "check foo FAILED\n");
}

#[test]
fn reviewer_failures_are_distinct_from_rejections() {
    let env = Env::new();
    env.set_repo_config(
        "[[reviewer]]\nname = \"crashy\"\nstage = \"in-queue\"\ncommand = \"echo oops >&2; exit 3\"\n\
         [[reviewer]]\nname = \"slow\"\nstage = \"in-queue\"\ncommand = \"sleep 30\"\ntimeout = \"1s\"\n",
    );
    env.commit("alice", "a.txt", "a\n", "Add a");
    let started = Instant::now();
    let (code, t) = env.land_wait("alice");
    assert!(started.elapsed() < Duration::from_secs(15), "the timeout should kill the slow reviewer");
    assert_eq!(code, 2, "{t:#}");
    assert_eq!(t["state"], "reviewer-error");
    let errors: Vec<&str> = reviews(&t).iter().map(|r| r["error"].as_str().unwrap()).collect();
    assert!(errors.contains(&"reviewer exited with status 3: oops"), "{errors:?}");
    assert!(errors.contains(&"timed out after 1s"), "{errors:?}");
}

#[test]
fn in_queue_reviewers_see_the_merged_result() {
    let env = Env::new();
    let record = env.home().join("record");
    env.set_repo_config(&format!(
        "[[reviewer]]\nname = \"inspect\"\nstage = \"in-queue\"\n\
         command = 'echo \"$NBRANCH_STAGE $NBRANCH_AGENT $NBRANCH_TICKET $NBRANCH_BASE $NBRANCH_HEAD\" > {r}; \
         ls >> {r}; cat > {r}.diff'\n",
        r = record.display()
    ));
    env.nb_ok("bob", &["new"]);
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.commit("bob", "b.txt", "b\n", "Add b");
    assert_eq!(env.land_wait("alice").0, 0);
    let trunk_after_alice = env.trunk();
    let bob_head = env.git(&env.worktree("bob"), &["rev-parse", "HEAD"]);
    let (code, t) = env.land_wait("bob");
    assert_eq!(code, 0, "{t:#}");

    let rec = std::fs::read_to_string(&record).unwrap();
    let fields: Vec<&str> = rec.lines().next().unwrap().split(' ').collect();
    assert_eq!(&fields[..4], ["in-queue", "bob", "2", trunk_after_alice.as_str()]);
    // HEAD is the picked commit on the new trunk, not bob's branch.
    assert_ne!(fields[4], bob_head);
    assert_eq!(env.git(&env.golden(), &["rev-parse", &format!("{}^", fields[4])]), trunk_after_alice);
    assert!(rec.contains("a.txt") && rec.contains("b.txt"), "{rec}");
    let diff = std::fs::read_to_string(format!("{}.diff", record.display())).unwrap();
    assert!(diff.contains("+++ b/b.txt") && !diff.contains("a.txt"), "{diff}");
}

#[test]
fn land_is_repicked_when_a_human_moves_the_trunk() {
    let env = Env::new();
    let human = env.human_clone();
    let marker = env.home().join("moved");
    let count = env.home().join("runs");
    env.set_repo_config(&format!(
        "[[reviewer]]\nname = \"tests\"\nstage = \"in-queue\"\ncommand = '''\n\
         echo run >> {count}\n\
         if [ ! -e {marker} ]; then\n  touch {marker}\n  cd {human} && git pull -q && echo hi > human.txt && git add human.txt \
         && git commit -qm 'Human commit' && git push -q origin HEAD:master\nfi\n'''\n",
        count = count.display(),
        marker = marker.display(),
        human = human.display(),
    ));
    env.commit("alice", "a.txt", "a\n", "Add a");
    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 0, "{t:#}");
    assert_eq!(t["attempts"], 2);
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
    assert_eq!(&env.trunk_subjects()[..2], ["Add a", "Human commit"]);
    assert!(env.trunk_file("human.txt").is_some());
}

#[test]
fn async_lands_are_processed_in_fifo_order_and_notify() {
    let env = Env::new();
    env.set_repo_config("[[reviewer]]\nname = \"tests\"\nstage = \"in-queue\"\ncommand = \"sleep 2\"\n");
    env.nb_ok("bob", &["new"]);
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.commit("bob", "b.txt", "b\n", "Add b");
    let out = env.nb_ok("alice", &["land"]);
    assert!(out.contains("Queued land #1"), "{out}");
    env.wait_for(1, |t| t["state"] == "testing");
    env.nb_ok("bob", &["land"]);

    let queued = env.wait_for(2, |t| t["state"] == "queued");
    assert_eq!(queued["state"], "queued");
    let queue = env.nb_ok("observer", &["queue"]);
    assert!(queue.contains("#1    alice") && queue.contains("testing"), "{queue}");
    assert!(queue.contains("#2    bob") && queue.contains("position 1"), "{queue}");

    assert_eq!(env.wait_terminal(1)["state"], "landed");
    assert_eq!(env.wait_terminal(2)["state"], "landed");
    assert_eq!(&env.trunk_subjects()[..2], ["Add b", "Add a"]);

    env.wait_notification("alice\tnbranch: land #1 LANDED 1 commit(s) onto master");
    env.wait_notification("bob\tnbranch: land #2 LANDED");

    // One land per agent at a time.
    env.commit("alice", "c.txt", "c\n", "Add c");
    env.nb_ok("alice", &["land"]);
    let out = env.nb("alice", &["land"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("already in progress"));
    env.wait_terminal(3);
}

#[test]
fn rejection_notification_includes_comments() {
    let env = Env::new();
    env.set_repo_config(
        "[[reviewer]]\nname = \"lint\"\nstage = \"pre-queue\"\ncommand = \"echo 'trailing whitespace on line 3'; exit 1\"\n",
    );
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.nb_ok("alice", &["land"]);
    assert_eq!(env.wait_terminal(1)["state"], "rejected");
    let notes = env.wait_notification("alice\tnbranch: land #1 REJECTED by lint.");
    assert!(notes.contains("[lint] error: trailing whitespace on line 3"), "{notes}");
}

#[test]
fn cancel_stops_a_running_review() {
    let env = Env::new();
    env.set_repo_config("[[reviewer]]\nname = \"slow\"\nstage = \"pre-queue\"\ncommand = \"sleep 60\"\n");
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.nb_ok("alice", &["land"]);
    env.wait_for(1, |t| t["detail"].as_str().unwrap_or("").contains("running slow"));
    let started = Instant::now();
    env.nb_ok("alice", &["cancel", "1"]);
    let t = env.wait_terminal(1);
    assert_eq!(t["state"], "cancelled");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(env.nb("alice", &["cancel", "1"]).status.code() == Some(2));
}

#[test]
fn dead_workers_are_reaped_as_abandoned() {
    let env = Env::new();
    env.set_repo_config("[[reviewer]]\nname = \"slow\"\nstage = \"pre-queue\"\ncommand = \"sleep 5\"\n");
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.nb_ok("alice", &["land"]);
    let t = env.wait_for(1, |t| t["detail"].as_str().unwrap_or("").contains("running slow"));
    let pid = t["pid"].as_u64().unwrap();
    assert!(Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
    let t = env.wait_for(1, |t| t["state"] != "reviewing");
    assert_eq!(t["state"], "abandoned");
    // The agent can land again.
    env.nb_ok("alice", &["land"]);
    env.wait_terminal(2);
}

/// Wait for a review request `nick` may answer.
fn wait_request(env: &Env, nick: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reqs: Value = serde_json::from_str(&env.nb_ok(nick, &["requests", "--json"])).unwrap();
        if let Some(r) = reqs.as_array().unwrap().iter().find(|r| r["message"].is_string()) {
            return r.clone();
        }
        assert!(Instant::now() < deadline, "no review request arrived for {nick}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn review_agent_round_trip() {
    let env = Env::new();
    env.start_nqueue();
    env.set_repo_config(
        "[[reviewer]]\nname = \"review-agent\"\nstage = \"pre-queue\"\ncommand = \"nbranch review-agent --nick rev\"\ntimeout = \"60s\"\n",
    );
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.nb_ok("alice", &["land"]);

    let req = wait_request(&env, "rev");
    let id = req["id"].as_str().unwrap().to_string();
    assert_eq!(id, "1-pre-queue-rev");
    assert_eq!(req["queue"], "nbranch.review.rev");
    assert!(Path::new(req["worktree"].as_str().unwrap()).join("a.txt").exists());
    // The request reaches rev over nqueue, leased until it's answered.
    let d = env.dequeue("rev");
    assert_eq!(d["message"]["id"], req["message"]);
    assert_eq!(d["message"]["from"], "nbranch");
    let body = d["message"]["body"].as_str().unwrap();
    assert!(body.starts_with(&format!("nbranch review request {id}:")), "{body}");
    assert!(d["lease_expires_at"].is_number());

    // Only the addressed reviewer can answer, and it must say which model it is.
    assert!(!env.nb("mallory", &["respond", &id, "accept", "--model", "m"]).status.success());
    let out = env.nb("rev", &["respond", &id, "reject"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--model"));
    assert!(!env.nb("rev", &["respond", &id, "reject", "--model", "test-model"]).status.success());
    // The reviewer's session identity comes from NQUEUE_ID, as the nqueue plugins export it.
    let mut cmd = Command::new(BIN);
    cmd.args(["respond", &id, "reject", "--model", "test-model", "--comment", "a.txt:1: error: needs more a"])
        .current_dir(&env.root);
    configure(&mut cmd, env.home());
    cmd.env("NQUEUE_ID", "rev");
    check("respond", &cmd.output().unwrap());
    // Responding acked the message.
    let again = env.nq(Some("rev"), &["ack", req["message"].as_str().unwrap()]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("not leased"), "{again:?}");

    let t = env.wait_terminal(1);
    assert_eq!(t["state"], "rejected");
    let r = &reviews(&t)[0];
    assert_eq!(r["reviewer"], serde_json::json!({"nick": "rev", "model": "test-model"}));
    assert_eq!(r["comments"][0]["body"], "needs more a");

    env.commit("alice", "a.txt", "aaaa\n", "More a");
    env.nb_ok("alice", &["land"]);
    let id = wait_request(&env, "rev")["id"].as_str().unwrap().to_string();
    env.dequeue("rev");
    let mut cmd = Command::new(BIN);
    cmd.args(["respond", &id, "accept"]).current_dir(&env.root);
    configure(&mut cmd, env.home());
    cmd.env("NBRANCH_AGENT", "rev").env("NBRANCH_MODEL", "claude-opus-5-5");
    check("respond", &cmd.output().unwrap());
    assert_eq!(env.wait_terminal(2)["state"], "landed");
    let msg = env.git(&env.golden(), &["log", "-1", "--format=%B", "master"]);
    assert!(msg.contains("Reviewed-by: review-agent [rev, claude-opus-5-5]"), "{msg}");
}

#[test]
fn shared_review_queue_goes_to_one_reviewer() {
    let env = Env::new();
    env.start_nqueue();
    env.set_repo_config(
        "[[reviewer]]\nname = \"llm\"\nstage = \"pre-queue\"\ncommand = \"nbranch review-agent --queue reviews\"\ntimeout = \"60s\"\n",
    );
    env.nq_ok(None, &["create", "reviews", "--policy", "nbranch-review"]);
    for nick in ["rev1", "rev2"] {
        env.nq_ok(None, &["register", nick]);
        env.nq_ok(Some(nick), &["subscribe", "reviews"]);
    }
    env.commit("alice", "a.txt", "a\n", "Add a");
    env.nb_ok("alice", &["land"]);

    let req = wait_request(&env, "rev1");
    let id = req["id"].as_str().unwrap().to_string();
    assert_eq!(id, "1-pre-queue-llm");
    assert!(req["reviewer_nick"].is_null());
    // The lander can't review its own land.
    assert!(env.nb("observer", &["requests", "--json"]).status.success());
    let mine: Value = serde_json::from_str(&env.nb_ok("alice", &["requests", "--json"])).unwrap();
    assert!(mine.as_array().unwrap().is_empty());
    let out = env.nb("alice", &["respond", &id, "accept", "--model", "m"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("someone else must review it"));

    // One reviewer gets it; the other doesn't.
    let d = env.dequeue("rev2");
    assert_eq!(d["message"]["id"], req["message"]);
    assert_eq!(env.nq_ok(Some("rev1"), &["dequeue"]), "");
    env.nb_ok("rev2", &["respond", &id, "accept", "--model", "m2"]);
    assert_eq!(env.wait_terminal(1)["state"], "landed");
    let msg = env.git(&env.golden(), &["log", "-1", "--format=%B", "master"]);
    assert!(msg.contains("Reviewed-by: llm [rev2, m2]"), "{msg}");
    // Answering again, or after the land finished, is refused.
    assert!(!env.nb("rev1", &["respond", &id, "accept", "--model", "m"]).status.success());
}

#[test]
fn reviewer_errors_when_the_review_policy_is_missing() {
    let env = Env::new();
    env.start_nqueue();
    env.set_repo_config(
        "review_policy = \"nope\"\n[[reviewer]]\nname = \"llm\"\nstage = \"pre-queue\"\ncommand = \"nbranch review-agent --nick rev\"\n",
    );
    env.commit("alice", "a.txt", "a\n", "Add a");
    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 2);
    assert_eq!(t["state"], "reviewer-error");
    let err = reviews(&t)[0]["error"].as_str().unwrap();
    assert!(err.contains("[policies.nope]"), "{err}");
}

#[test]
fn review_agent_must_not_be_the_lander() {
    let env = Env::new();
    env.set_repo_config(
        "[[reviewer]]\nname = \"review-agent\"\nstage = \"pre-queue\"\ncommand = \"nbranch review-agent --nick alice\"\n",
    );
    env.commit("alice", "a.txt", "a\n", "Add a");
    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 2);
    assert_eq!(t["state"], "reviewer-error");
}

#[test]
fn reset_refuses_to_discard_unlanded_work() {
    let env = Env::new();
    env.commit("alice", "a.txt", "a\n", "Add a");
    let out = env.nb("alice", &["reset"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("haven't landed"));
    std::fs::write(env.worktree("alice").join("dirty.txt"), "x").unwrap();
    assert!(!env.nb("alice", &["reset"]).status.success());
    env.nb_ok("alice", &["reset", "--force"]);
    assert_eq!(env.git(&env.worktree("alice"), &["rev-parse", "HEAD"]), env.trunk());
}

#[test]
fn gc_removes_only_idle_landed_worktrees() {
    let env = Env::new();
    env.commit("alice", "a.txt", "a\n", "Add a");
    assert_eq!(env.land_wait("alice").0, 0);
    env.nb_ok("alice", &["reset"]);
    env.commit("bob", "b.txt", "b\n", "Add b");
    env.nb_ok("carol", &["new"]);
    std::fs::write(env.worktree("carol").join("wip.txt"), "x").unwrap();

    let out = env.nb_ok("observer", &["gc"]);
    assert!(out.contains("kept alice: active"), "{out}");
    assert!(env.worktree("alice").exists());

    let out = env.nb_ok("observer", &["gc", "--idle", "0", "--dry-run"]);
    assert!(out.contains("would remove alice"), "{out}");
    assert!(env.worktree("alice").exists());

    let out = env.nb_ok("observer", &["gc", "--idle", "0"]);
    assert!(out.contains("removed alice"), "{out}");
    assert!(out.contains("kept bob: 1 unlanded commit(s)"), "{out}");
    assert!(out.contains("kept carol: uncommitted changes"), "{out}");
    assert!(!env.worktree("alice").exists());
    assert!(env.git(&env.golden(), &["branch", "--list", "nbranch/alice"]).is_empty());

    let out = env.nb_ok("observer", &["gc", "--idle", "0", "--force"]);
    assert!(out.contains("removed bob"), "{out}");
}

#[test]
fn identity_and_root_discovery() {
    let env = Env::new();
    env.nb_ok("alice", &["new"]);
    // From inside the worktree, without naming the root.
    let out = check("whoami", &env.cmd_in(&env.worktree("alice"), Some("alice"), &["whoami"]));
    assert!(out.starts_with("alice\n") && out.contains("nbranch/alice"), "{out}");
    // NCHAT_NICK is used when NBRANCH_AGENT isn't set.
    let mut cmd = Command::new(BIN);
    cmd.arg("whoami").current_dir(&env.root);
    configure(&mut cmd, env.home());
    cmd.env("NCHAT_NICK", "from-nchat");
    assert!(check("whoami", &cmd.output().unwrap()).starts_with("from-nchat\n"));
    // Outside any root.
    let out = env.cmd_in(env.home(), Some("alice"), &["queue"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside an nbranch root"));
    assert!(!env.nb("../evil", &["new"]).status.success());
}

#[test]
fn setup_command_runs_in_agent_and_review_worktrees() {
    let env = Env::new();
    let assets = env.home().join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("game.dat"), "data").unwrap();
    // Machine-specific settings go in the root's own config, not the repo.
    std::fs::write(
        env.golden().join("nbranch/config"),
        format!(
            "setup_command = 'ln -s {} game.dat && echo \"$NBRANCH_WORKTREE_KIND\" >> {}'\n",
            assets.join("game.dat").display(),
            env.home().join("kinds").display()
        ),
    )
    .unwrap();
    env.git(&env.golden(), &["config", "core.excludesFile", "/dev/null"]);
    std::fs::write(env.golden().join("info/exclude"), "/game.dat\n").unwrap();
    env.set_repo_config(
        "[[reviewer]]\nname = \"needs-assets\"\nstage = \"in-queue\"\ncommand = \"test \\\"$(cat game.dat)\\\" = data\"\n",
    );

    env.commit("alice", "a.txt", "a\n", "Add a");
    assert_eq!(std::fs::read_to_string(env.worktree("alice").join("game.dat")).unwrap(), "data");
    let (code, t) = env.land_wait("alice");
    assert_eq!(code, 0, "{t:#}");
    assert_eq!(std::fs::read_to_string(env.home().join("kinds")).unwrap(), "agent\nreview\n");
    // The excluded symlink doesn't make the worktree dirty.
    env.nb_ok("alice", &["reset"]);

    std::fs::write(env.golden().join("nbranch/config"), "setup_command = 'echo broken >&2; exit 1'\n").unwrap();
    let out = env.nb("bob", &["new"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("setup failed"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("broken"));
}
