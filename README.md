# nbranch - Independent Branches for Agents

## Goals

- Agents get their own independent copy of the source code
- Multiple agents can make changes at the same time without interfering with
  each other
- Acts as a local merge queue that permits agents to work on independent copies
  of the source code before 'landing' them onto the 'real' trunk branch
- Optionally run programs that either reject the land with comments or allow
  it to proceed

## Quick start

```sh
cargo install --path .
nbranch init ~/work/myproj --from git@example.com:me/myproj.git --trunk master
cd ~/work/myproj
NBRANCH_AGENT=alice nbranch new        # agents normally get their nick from nqueue
cd agents/alice && ...edit, commit...
nbranch land                           # result arrives as an nchat message
nbranch tui                            # watch the queue and the trunk
```

## Design

### Overview

- Essentially a thin wrapper around git worktrees with machinery to allow agents
  to review and land changes atomically
- An nbranch root looks like this:

  ```text
  <root>/golden.git/                 the golden bare repository
  <root>/golden.git/nbranch/         queue state, tickets, reviewer output
  <root>/agents/<nick>/              agent worktrees (branch nbranch/<nick>)
  ```

- The 'golden' copy of the repository is a bare git repo that agents don't edit
  directly
  - Humans may still commit to the trunk directly (e.g. by pushing to golden
    from their own clone). nbranch tolerates the trunk moving at any time
    (see [Landing](#landing))
  - Golden is not pushed to any upstream by nbranch. Syncing golden with a
    remote is a separate process outside the scope of nbranch (for now)
- The trunk is the branch golden's `HEAD` points at, chosen with
  `nbranch init --trunk <name>` (default `master`)
- `nbranch` branches are short-lived by design:
  - They use git worktrees to copy the code into a new directory
  - Each agent gets its own worktree and branch to work on
- Isolation between agents is cooperative, not enforced. Agents are assumed to
  be benign, and the [agent skill](skills/nbranch/SKILL.md) tells them not to
  touch other agents' worktrees or branches, or the trunk ref, directly
- Commands find the root by searching upwards from the current directory, or
  from `--root` / `NBRANCH_ROOT`

### Agent identity

- An agent's nbranch identity is its session's nqueue agent ID (which is also
  its nchat nick), so that land results and review requests reach the same
  session
- It's resolved from `NBRANCH_AGENT`, then `NQUEUE_ID`, then `NCHAT_NICK`, then
  `nchat nick`
- Worktrees and branches are named after the nick (`agents/<nick>`,
  `nbranch/<nick>`)

### Commands

| Command | Purpose |
| --- | --- |
| `nbranch init <dir> --from <repo> [--trunk <name>]` | Create a root with a golden bare clone |
| `nbranch new` | Create the caller's worktree and branch at the trunk HEAD (idempotent) |
| `nbranch land [--wait] [--json]` | Submit the caller's branch. Returns a ticket ID immediately; the result arrives as a [notification](#notifications). `--wait` blocks instead (exit 0 landed, 1 rejected/conflict, 2 anything else) |
| `nbranch queue [--json]` | Show queued and in-progress lands |
| `nbranch show <ticket> [--json]` | Show a land, including reviewer comments and output |
| `nbranch log [-n N] [--full] [--json]` | The [rejection log](#rejection-log) |
| `nbranch cancel <ticket>` | Withdraw an unfinished land |
| `nbranch sync` | Rebase the caller's branch onto the latest trunk |
| `nbranch reset [--force]` | Reset the caller's branch and worktree to the latest trunk. Refuses to discard uncommitted or unlanded work without `--force` |
| `nbranch gc [--idle D] [--force] [--dry-run]` | Remove worktrees that are idle (default `gc_idle`), clean and fully landed. `--force` also removes ones with unlanded commits. Also cleans up leftover temporary worktrees |
| `nbranch tui` | [Live view](#tui) of the queue, the trunk and recent results |
| `nbranch whoami` | Show the caller's nick and worktree |
| `nbranch review-agent [--nick <nick> \| --queue <queue>]` | Reviewer command that queues a review request for a review agent (see [Review agent](#review-agent)) |
| `nbranch requests [--json]` | List review requests waiting for the caller |
| `nbranch respond <request> accept\|reject [--comment C]... [--model M]` | Answer a review request |

nbranch never modifies an agent's worktree on its own; the repo state does not
change under the agent's feet. Only `sync` and `reset`, run by the agent,
change it.

### Landing

A land runs in a detached worker process (or in the foreground with `--wait`)
and moves through these phases:

1. **Enqueue** - `nbranch land` snapshots the branch HEAD (pinned by
   `refs/nbranch/tickets/<id>`) and records a ticket. Later edits to the
   worktree don't affect the land in progress. Each agent can have one
   unfinished land at a time
2. **Config check** - if any commit in the stack touches `.nbranch`, the land is
   rejected automatically. Changes to `.nbranch` may only be made directly on
   the trunk (for now). Stacks containing merge commits are also rejected
   (`nbranch sync` linearises them)
3. **Pre-queue review** - the `pre-queue` reviewers run against the snapshot,
   outside the queue lock, so many lands can be in this phase at once. This
   stage is meant for slow, semantic review (e.g. an LLM review agent). If any
   reviewer rejects, the land fails without entering the queue
4. **Queue** - the land waits for the queue lock. Lands are processed one at a
   time in the order they entered the queue. The lock is a `flock`, so a
   killed worker releases it, and tickets whose worker died are marked
   `abandoned`
5. **Pick** - the stack (commits since the merge-base with the trunk) is
   cherry-picked onto the *current* trunk HEAD in memory with
   `git merge-tree`. The trunk ref and every worktree are untouched. A
   conflict rejects the land, reporting the conflicting commit and files.
   Commits that become empty are dropped
6. **In-queue review** - the `in-queue` reviewers run in a temporary worktree
   holding the picked result. This stage is meant for fast, mechanical checks
   (build, tests) that must see the merged tree to catch semantic conflicts
7. **Advance** - the trunk is moved with a compare-and-swap:
   `git update-ref refs/heads/<trunk> <new> <old>`. If the trunk moved since
   step 5 (e.g. a human committed directly), the land returns to step 5 and is
   re-picked and re-reviewed on the new HEAD. It may then fail with conflicts;
   that's expected
8. **Notify** - the result is delivered to the lander and temporary worktrees
   are removed

Because nothing touches the trunk ref until step 7, and step 7 is a single
atomic ref update, a land either applies the whole stack or none of it.

Landed commits are cherry-picks, so they have new SHAs and the agent's branch
diverges from the trunk. Each landed commit keeps its original author and
message, is committed by `nbranch`, and gets trailers recording provenance:

```
Nbranch-Agent: <lander nick>
Nbranch-Ticket: <ticket id>
Reviewed-by: <reviewer name> [<reviewer nick>, <model>]
```

The nick and model are included when the reviewer reports them (e.g. a review
agent), and omitted for plain command reviewers.

### After a land

- **Landed** - the agent runs `nbranch reset` to move onto the new trunk HEAD
  before starting further work
- **Rejected** - the agent addresses the comments and lands again
- **Conflict** - the agent runs `nbranch sync` to rebase onto the latest trunk,
  resolves conflicts in its own worktree, and lands again
- **Reviewer error** - an infrastructure failure, not a judgement on the change.
  The agent should report it rather than change its code

### Reviewers

Reviewers are shell commands defined in `.nbranch`, each in one of the two
stages. All reviewers in a stage run in parallel, and all of them run even if
one rejects, so the agent gets every comment in a single round trip.

**Inputs**

- Working directory: a temporary worktree containing the tree under review (the
  branch snapshot for `pre-queue`, the picked result for `in-queue`)
- Environment:
  - `NBRANCH_STAGE` - `pre-queue` or `in-queue`
  - `NBRANCH_BASE` - the trunk commit the change is based on
  - `NBRANCH_HEAD` - the commit under review
  - `NBRANCH_AGENT` - the lander's nick
  - `NBRANCH_TICKET` - the land ticket ID
  - `NBRANCH_REVIEWER` - the reviewer's name from the config
  - `NBRANCH_ROOT` - the nbranch root
- stdin: the diff `NBRANCH_BASE..NBRANCH_HEAD`

**Exit codes**

- `0` - accepted
- `1` - rejected. At least one comment is required
- `2` or higher, a signal, or exceeding the timeout - reviewer error. The land
  fails with a distinct `reviewer-error` status so it isn't mistaken for a
  rejection. Timed-out reviewers are killed along with their process group

Many tools exit with other codes on failure (`make` uses 2), so wrap them:
`command = "make test || exit 1"`.

**Output** - JSON on stdout:

```json
{
  "reviewer": { "nick": "brisk-otter-12", "model": "claude-opus-5-5" },
  "comments": [
    {
      "file": "src/queue.rs",
      "line": 42,
      "severity": "error",
      "body": "Lock is never released on the error path"
    }
  ]
}
```

`reviewer` is optional and is used for provenance trailers. `file` and `line`
are optional on each comment. `severity` is one of `error` (the default),
`warning` or `note`.

Plain commands that don't print JSON are supported: when one rejects, the last
40 lines of its output become the comment.

**Caveat:** in-queue reviewers that run repo code (e.g. `make test`) execute
code the agent can modify. Taking `.nbranch` from golden stops agents from
removing reviewers, but not from weakening the tests a reviewer runs.

### Rejection log

Every reviewer's stdout and stderr is saved under
`golden.git/nbranch/reviews/<ticket>/<attempt>-<stage>-<reviewer>.{out,err}`,
and each ticket records every reviewer's verdict, comments and error.

- `nbranch log` lists recent rejected, conflicting and reviewer-error lands,
  newest first, with each failing reviewer's comments and the tail of its
  output (`--full` for all of it, `--json` for tooling)
- `nbranch show <ticket>` shows the same for any land
- The TUI's details pane shows it for the selected land

### TUI

`nbranch tui` refreshes twice a second and shows:

- **Queue** - the land being processed (with the reviewers currently running),
  then queued lands with their position, then lands in pre-queue review
- **Trunk** - recent trunk commits with the landing agent and ticket
- **Recent lands** - finished lands, coloured by outcome. `f` toggles showing
  only rejections
- **Details** - the selected land's reviews, comments, conflict and saved
  reviewer output

Keys: `↑`/`↓` (or `j`/`k`) select, `PgUp`/`PgDn` (or `K`/`J`) scroll the
details, `q` quits.

### Configuration

- Repo configuration is kept at the top level of the project in `.nbranch`
  (TOML)
  - `.nbranch` is always read from the golden trunk, never from the agent's
    worktree, so reviewers can't be bypassed by editing the config in a branch.
    Branches that change it are rejected (see [Landing](#landing))
- There is also a global config at `$XDG_CONFIG_HOME/nbranch/config`
  (`~/.config/nbranch/config`), and a per-root config at
  `<root>/golden.git/nbranch/config` for machine-specific settings that don't
  belong in the repo (e.g. paths for `setup_command`)
  - Layers apply in the order global, root, repo. Later layers override
    earlier settings, except that reviewers are combined: a later layer can
    add reviewers but can't remove or redefine earlier ones
- Unknown keys are errors, so typos don't silently disable a reviewer

```toml
reviewer_timeout = "15m"     # default per-reviewer timeout
gc_idle = "24h"              # how long a worktree must be idle before gc removes it
notify_as = "nbranch"        # nchat nick notifications are sent from
# notify_command = "..."     # replace nchat; "" disables notifications
# setup_command = "..."      # run in every new worktree (see below)
review_policy = "nbranch-review"  # nqueue policy review queues are created with

[[reviewer]]
name = "review-agent"
stage = "pre-queue"
command = "nbranch review-agent --nick brisk-otter-12"
timeout = "30m"

[[reviewer]]
name = "build-and-test"
stage = "in-queue"
command = "make test || exit 1"
```

### Worktree setup

`setup_command` runs in every worktree nbranch creates: agent worktrees (on
`nbranch new`) and the temporary worktrees reviewers run in. It gets
`NBRANCH_WORKTREE`, `NBRANCH_WORKTREE_KIND` (`agent` or `review`) and
`NBRANCH_ROOT`. Use it to link untracked assets (large data, proprietary files)
into worktrees. If it fails, `nbranch new` reports the error, and a land fails
with an `error` state.

Symlinks it creates must be ignored, or the worktree counts as dirty. A
`.gitignore` pattern ending in `/` only matches real directories, so add such
links to `<root>/golden.git/info/exclude`, which applies to every worktree.

### Notifications

- Land results are sent as nchat direct messages (`nchat msg <nick> <text>`,
  as the `notify_as` nick), which reach the agent's running session, so agents
  don't poll
- `notify_command` replaces nchat with any shell command. It receives
  `NBRANCH_TO`, `NBRANCH_MESSAGE`, `NBRANCH_EVENT` (`land-result`),
  `NBRANCH_TICKET` and `NBRANCH_STATE`
- Review requests go over [nqueue](#review-agent) instead
- `nbranch land --wait` prints the result instead of sending a notification
- A rejection notification includes the reviewers' comments, so the agent can
  usually act without running `nbranch show`

### Review agent

`nbranch review-agent` is the standard reviewer for LLM review. Requests travel
over [nqueue](../nqueue) review queues, so they're leased to one review agent at
a time and survive a review agent going away:

1. It writes a review request and enqueues it, from the nqueue agent `nbranch`,
   on a review queue:
   - `--nick <nick>` - the queue `nbranch.review.<nick>`, which `<nick>` is
     subscribed to, so only that agent reviews
   - `--queue <queue>` - a shared queue (default `nbranch.review`). Any agent
     subscribed to it (`nqueue subscribe <queue>`) may take the request; with
     several, each request goes to one of them
2. The nqueue plugin or extension delivers the request to a review agent, which
   reviews the checkout (read-only by convention) and runs
   `nbranch respond <request> accept` or
   `nbranch respond <request> reject --comment 'path:line: error: text'`, with
   `--model <model id>` or `NBRANCH_MODEL` set. Responding acks the message.
   If the review agent never answers, the lease expires and the request is
   delivered again (to another subscriber of a shared queue).
   `nbranch requests` lists requests waiting for the caller
3. `review-agent` prints the verdict as reviewer JSON (including the review
   agent's nick and model, which end up in the `Reviewed-by` trailer) and exits
   0 or 1

The review agent must be a separate session from the lander: `review-agent
--nick` fails with a reviewer error if `<nick>` is the lander, `respond` only
accepts answers from the addressed nick, and nobody may answer a request for
their own land (they `nqueue nack` it so another reviewer gets it). The
reviewer timeout bounds how long it waits.

Review queues are created on first use with the nqueue policy named by
`review_policy` (default `nbranch-review`), which must be defined in the nqueue
home config, `~/.nqueue/config.toml`:

```toml
[policies.nbranch-review]
mode = "compete"            # each request goes to one reviewer
delivery = "at-least-once"  # leased until `nbranch respond` acks it
lease-secs = 3600           # at least as long as a review may take
max-in-flight = 1           # one review per reviewer at a time
```

### Agent skill

[`skills/nbranch/SKILL.md`](skills/nbranch/SKILL.md) tells agents to:

- Work only in their own worktree, and not touch other agents' worktrees,
  branches or the trunk ref directly
- Not edit `.nbranch`
- Land with `nbranch land` and wait for the notification instead of polling
- Run `nbranch reset` after a successful land, and `nbranch sync` after a
  conflict
- Answer review requests (delivered as nqueue messages) with `nbranch respond`

## Development

```sh
cargo test     # unit tests plus end-to-end tests in tests/cli.rs
```

The end-to-end tests drive the real binary against throwaway repositories and
use `notify_command` to capture notifications instead of nchat. Review agent
tests start a private nqueue server, so they need `nqueue` (or `NQUEUE_BIN`)
and `valkey-server` installed.
