mod agent;
mod config;
mod git;
mod identity;
mod land;
mod notify;
mod paths;
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
    /// Reviewer command: ask a review agent to review and wait for its verdict
    ReviewAgent {
        /// The review agent's nick
        #[arg(long)]
        nick: String,
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
    let root = Root::discover(cli.root)?;
    let store = Store::new(&root);
    match cli.command {
        Cmd::Init { .. } => unreachable!(),
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
        Cmd::ReviewAgent { nick } => return review_agent::run(&root, &nick),
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
