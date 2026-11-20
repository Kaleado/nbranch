mod agent;
mod config;
mod git;
mod identity;
mod land;
mod notify;
mod nq;
mod paths;
mod remote;
mod report;
mod review;
mod review_agent;
mod ticket;
mod tui;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use paths::Root;
use std::path::PathBuf;
use ticket::{State, Store};

#[derive(Parser)]
#[command(name = "nbranch", version, about = "Independent branches and a local merge queue for agents")]
struct Cli {
    /// The nbranch root (default: search upwards from the current directory)
    #[arg(long, global = true, env = "NBRANCH_ROOT")]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an nbranch root holding a golden bare clone of a repository
    Init {
        dir: PathBuf,
        /// Repository to clone
        #[arg(long)]
        from: String,
        /// Branch that lands are applied to
        #[arg(long, default_value = "master")]
        trunk: String,
    },
    /// Create your worktree and branch at the trunk HEAD
    New,
    /// Clone an nbranch root on another machine over SSH, to work and land
    /// from here
    Clone {
        /// The hub: host:/path/to/root (host as for ssh; a relative path is
        /// relative to your home directory there)
        hub: String,
        /// Where to put the clone (default: the root's directory name)
        dir: Option<PathBuf>,
        /// The nbranch command on the hub, e.g. ~/.cargo/bin/nbranch if it
        /// isn't on PATH for non-interactive SSH sessions
        #[arg(long, default_value = "nbranch")]
        hub_command: String,
    },
    /// Submit your branch to be landed on the trunk
    Land {
        /// Block until the land finishes instead of being notified
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show queued and in-progress lands
    Queue {
        #[arg(long)]
        json: bool,
    },
    /// Show a land, including reviewer comments and output
    Show {
        ticket: u64,
        #[arg(long)]
        json: bool,
    },
    /// Show recent rejected lands with reviewer comments and output
    Log {
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Include complete reviewer output instead of the last lines
        #[arg(long)]
        full: bool,
        #[arg(long)]
        json: bool,
    },
    /// Withdraw a land that hasn't finished
    Cancel { ticket: u64 },
    /// Rebase your branch onto the latest trunk
    Sync,
    /// Reset your branch and worktree to the latest trunk
    Reset {
        /// Discard uncommitted changes and unlanded commits
        #[arg(long)]
        force: bool,
    },
    /// Remove idle worktrees whose work has landed
    Gc {
        /// Minimum idle time before removal (default: gc_idle from the config)
        #[arg(long)]
        idle: Option<String>,
        /// Also remove idle worktrees with unlanded commits
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Live view of the queue, the trunk and recent results
    Tui,
    /// Show your nick and worktree
    Whoami,
    /// Reviewer command: queue a request for a review agent and wait for its verdict
    ReviewAgent {
        /// A review agent to ask (its own queue, nbranch.review.<nick>)
        #[arg(long, conflicts_with = "queue")]
        nick: Option<String>,
        /// A queue any subscribed review agent can take the request from
        /// (default nbranch.review)
        #[arg(long)]
        queue: Option<String>,
    },
    /// Answer a review request (run by the review agent)
    Respond {
        request: String,
        verdict: VerdictArg,
        /// `path:line: severity: text`, `severity: text` or plain text; repeatable
        #[arg(long = "comment")]
        comments: Vec<String>,
        /// The model you're running as, recorded in the landed commits
        #[arg(long, env = "NBRANCH_MODEL")]
        model: Option<String>,
    },
    /// List review requests waiting for you
    Requests {
        #[arg(long)]
        json: bool,
    },
    /// Check out the change a review request is about (in a clone, fetched
    /// from the hub) and print where
    ReviewCheckout { request: String },
    #[command(name = "__hub-info", hide = true)]
    HubInfo,
    #[command(name = "__landed-heads", hide = true)]
    LandedHeads,
    #[command(name = "__worker", hide = true)]
    Worker { ticket: u64 },
}

#[derive(Clone, Copy, ValueEnum)]
enum VerdictArg {
    Accept,
    Reject,
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nbranch: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    if let Cmd::Init { dir, from, trunk } = &cli.command {
        let root = Root::init(dir, from, trunk)?;
        println!("Initialised nbranch root {} (trunk: {trunk}).", root.dir.display());
        println!("Agents start with `nbranch new`; add reviewers in .nbranch on the trunk.");
        return Ok(0);
    }
    if let Cmd::Clone { hub, dir, hub_command } = &cli.command {
        let addr: remote::HubAddr = hub.parse()?;
        let dir = dir.clone().unwrap_or_else(|| {
            PathBuf::from(addr.root.trim_end_matches('/').rsplit('/').next().unwrap_or("nbranch"))
        });
        let nick = identity::resolve()?;
        let c = remote::clone(&addr, &dir, hub_command, &nick)?;
        println!("Cloned {}:{} into {} on branch {}.", c.hub.dest, c.hub.root, c.dir.display(), c.branch());
        println!("Work and commit there, then `nbranch land`; results arrive over nchat as usual.");
        return Ok(0);
    }
    let root = match cli.root {
        Some(dir) => Root::discover(Some(dir))?,
        None => match Root::discover(None) {
            Ok(root) => root,
            Err(e) => match remote::Clone::discover()? {
                Some(c) => return in_clone(&c, cli.command),
                None => return Err(e),
            },
        },
    };
    let store = Store::new(&root);
    match cli.command {
        Cmd::Init { .. } | Cmd::Clone { .. } => unreachable!(),
        Cmd::New => {
            let nick = identity::resolve()?;
            let (path, created) = agent::new(&root, &nick)?;
            let verb = if created { "Created" } else { "Already have" };
            println!("{verb} worktree {} on branch {}.", path.display(), agent::branch_name(&nick));
        }
        Cmd::Land { wait, json } => return land(&root, wait, json),
        Cmd::Worker { ticket } => {
            land::run_worker(&root, ticket, true)?;
        }
        Cmd::Queue { json } => {
            let active: Vec<_> = store
                .locked(|| {
                    store.reap()?;
                    store.all()
                })?
                .into_iter()
                .filter(|t| !t.state.is_terminal())
                .collect();
            if json {
                print_json(&active)?;
            } else {
                print_queue(&root, active)?;
            }
        }
        Cmd::Show { ticket, json } => {
            let t = store.locked(|| {
                store.reap()?;
                store.load(ticket)
            })?;
            if json {
                print_json(&t)?;
            } else {
                for line in report::ticket_lines(&t, Some(40)) {
                    println!("{line}");
                }
            }
        }
        Cmd::Log { limit, full, json } => {
            let mut failed: Vec<_> = store.all()?.into_iter().filter(|t| t.state.is_failure()).collect();
            failed.reverse();
            failed.truncate(limit);
            if json {
                print_json(&failed)?;
            } else if failed.is_empty() {
                println!("No rejected lands.");
            } else {
                for t in &failed {
                    for line in report::ticket_lines(t, Some(if full { usize::MAX } else { 20 })) {
                        println!("{line}");
                    }
                    println!();
                }
            }
        }
        Cmd::Cancel { ticket } => {
            let t = store.locked(|| {
                let mut t = store.load(ticket)?;
                if t.state.is_terminal() {
                    anyhow::bail!("land #{ticket} already finished ({})", t.state);
                }
                t.cancel_requested = true;
                if !t.pid.is_some_and(ticket::pid_alive) {
                    t.state = State::Cancelled;
                    t.detail = None;
                }
                store.save(&mut t)?;
                Ok(t)
            })?;
            if t.state == State::Cancelled {
                println!("Cancelled land #{ticket}.");
            } else {
                println!("Cancelling land #{ticket}; it will stop at the next checkpoint.");
            }
        }
        Cmd::Sync => return agent::sync(&root, &identity::resolve()?),
        Cmd::Reset { force } => agent::reset(&root, &identity::resolve()?, force)?,
        Cmd::Gc { idle, force, dry_run } => {
            let idle = idle.map(|s| config::parse_duration(&s)).transpose()?;
            agent::gc(&root, idle, force, dry_run)?;
        }
        Cmd::Tui => tui::run(&root)?,
        Cmd::Whoami => {
            let nick = identity::resolve()?;
            println!("{nick}");
            let wt = root.agent_worktree(&nick);
            if wt.join(".git").exists() {
                println!("worktree: {}", wt.display());
                println!("branch: {}", agent::branch_name(&nick));
            } else {
                println!("no worktree yet; run `nbranch new`");
            }
        }
        Cmd::ReviewCheckout { request } => {
            let nick = identity::resolve()?;
            let req = review_agent::pending(&root, &nick)?
                .into_iter()
                .find(|r| r.id == request)
                .ok_or_else(|| anyhow::anyhow!("no review request {request} waiting for {nick}"))?;
            println!("{}", req.worktree.display());
            eprintln!("See the change with `git -C {} diff {} {}`.", req.worktree.display(), req.base, req.head);
        }
        Cmd::HubInfo => {
            let nick = identity::resolve()?;
            let trunk = root.trunk()?;
            let branch = format!("refs/heads/{}", agent::branch_name(&nick));
            print_json(&remote::HubInfo {
                root: root.dir.clone(),
                trunk,
                has_worktree: root.agent_worktree(&nick).join(".git").exists(),
                has_branch: git::try_rev_parse(&root.golden(), &branch).is_some(),
            })?;
        }
        Cmd::LandedHeads => print_json(&agent::landed_heads(&root, &identity::resolve()?)?)?,
        Cmd::ReviewAgent { nick, queue } => return review_agent::run(&root, nick.as_deref(), queue.as_deref()),
        Cmd::Respond { request, verdict, comments, model } => {
            review_agent::respond(&root, &request, matches!(verdict, VerdictArg::Accept), &comments, model)?;
        }
        Cmd::Requests { json } => {
            let pending = review_agent::pending(&root, &identity::resolve()?)?;
            if json {
                print_json(&pending)?;
            } else if pending.is_empty() {
                println!("No review requests waiting for you.");
            } else {
                for r in pending {
                    println!(
                        "{}  land #{} by {}  {}..{}  checked out at {}",
                        r.id,
                        r.ticket,
                        r.agent,
                        git::short(&r.base),
                        git::short(&r.head),
                        r.worktree.display()
                    );
                }
            }
        }
    }
    Ok(0)
}

/// Commands run in a clone on another machine: git work happens here, and
/// everything about the queue happens on the hub over SSH.
fn in_clone(c: &remote::Clone, command: Cmd) -> Result<i32> {
    c.check_identity()?;
    // the arguments after the program name, as given
    let args: Vec<String> = std::env::args().skip(1).collect();
    match command {
        Cmd::New | Cmd::Whoami => {
            println!("{}", c.nick);
            println!("clone: {}", c.dir.display());
            println!("branch: {}", c.branch());
            println!("hub: {}:{}", c.hub.dest, c.hub.root);
            Ok(0)
        }
        Cmd::Land { .. } => {
            if git::is_dirty(&c.dir).unwrap_or(false) {
                eprintln!("warning: uncommitted changes in {} are not part of this land", c.dir.display());
            }
            c.push_branch()?;
            c.hub.forward(&c.nick, &args, false)
        }
        Cmd::Sync => {
            c.fetch_trunk()?;
            agent::sync_in(&c.dir, &c.trunk_ref(), &c.trunk, &c.branch())
        }
        Cmd::Reset { force } => {
            c.fetch_trunk()?;
            let landed = if force { Vec::new() } else { c.landed_heads()? };
            agent::reset_in(&c.dir, &c.trunk_ref(), &c.trunk, &c.branch(), &landed, force)?;
            Ok(0)
        }
        Cmd::Queue { .. } | Cmd::Show { .. } | Cmd::Log { .. } | Cmd::Cancel { .. } | Cmd::Requests { .. } => {
            c.hub.forward(&c.nick, &args, false)
        }
        Cmd::Tui => c.hub.forward(&c.nick, &args, true),
        Cmd::Respond { request, verdict, comments, model } => {
            // NBRANCH_MODEL is set here, not on the hub
            let mut fwd = vec!["respond".to_string(), request, args_verdict(verdict).into()];
            for comment in comments {
                fwd.push("--comment".into());
                fwd.push(comment);
            }
            if let Some(m) = model {
                fwd.push("--model".into());
                fwd.push(m);
            }
            c.hub.forward(&c.nick, &fwd, false)
        }
        Cmd::ReviewCheckout { request } => {
            let (wt, base, head) = remote::review_checkout(c, &request)?;
            println!("{}", wt.display());
            eprintln!("See the change with `git -C {} diff {base} {head}`.", wt.display());
            Ok(0)
        }
        Cmd::Gc { .. } => anyhow::bail!("gc runs on the hub: ssh {} nbranch --root {} gc", c.hub.dest, c.hub.root),
        Cmd::ReviewAgent { .. } | Cmd::Worker { .. } | Cmd::HubInfo | Cmd::LandedHeads => {
            anyhow::bail!("this command runs on the hub, not in a clone")
        }
        Cmd::Init { .. } | Cmd::Clone { .. } => unreachable!(),
    }
}

fn args_verdict(v: VerdictArg) -> &'static str {
    match v {
        VerdictArg::Accept => "accept",
        VerdictArg::Reject => "reject",
    }
}

fn land(root: &Root, wait: bool, json: bool) -> Result<i32> {
    let nick = identity::resolve()?;
    let wt = root.agent_worktree(&nick);
    if wt.join(".git").exists() && git::is_dirty(&wt).unwrap_or(false) {
        eprintln!("warning: uncommitted changes in {} are not part of this land", wt.display());
    }
    let t = land::enqueue(root, &nick)?;
    if wait {
        let t = land::run_worker(root, t.id, false)?;
        if json {
            print_json(&t)?;
        } else {
            for line in report::ticket_lines(&t, Some(20)) {
                println!("{line}");
            }
        }
        return Ok(match t.state {
            State::Landed => 0,
            State::Rejected | State::Conflict => 1,
            _ => 2,
        });
    }
    land::spawn_worker(root, t.id)?;
    if json {
        print_json(&Store::new(root).load(t.id)?)?;
    } else {
        println!(
            "Queued land #{} ({} commit(s) from {} at {}). The result will be sent to {nick} as a notification; \
             check progress with `nbranch show {}`.",
            t.id,
            t.commits,
            t.branch,
            git::short(&t.head),
            t.id
        );
    }
    Ok(0)
}

fn print_queue(root: &Root, mut active: Vec<ticket::Ticket>) -> Result<()> {
    let trunk = root.trunk()?;
    let sha = git::rev_parse(&root.golden(), &format!("refs/heads/{trunk}"))?;
    println!("trunk {trunk} @ {}", git::short(&sha));
    if active.is_empty() {
        println!("The queue is empty.");
        return Ok(());
    }
    active.sort_by_key(|t| match t.state {
        State::Picking | State::Testing => (0, 0),
        State::Queued => (1, t.queue_seq.unwrap_or(0)),
        _ => (2, t.id),
    });
    let now = util::now();
    let mut position = 0;
    for t in active {
        let detail = if t.state == State::Queued {
            position += 1;
            format!("position {position}")
        } else {
            t.detail.clone().unwrap_or_default()
        };
        println!(
            "#{:<4} {:<20} {:<10} {:>4}  {detail}",
            t.id,
            t.agent,
            t.state.as_str(),
            util::age(now.saturating_sub(t.created))
        );
    }
    Ok(())
}
