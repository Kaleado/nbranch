---
name: nbranch
description: Work on a shared repository through nbranch - your own worktree and branch, landing changes through a local merge queue with reviewers, and answering nbranch review requests. Use when the project is an nbranch root (it has golden.git/ and agents/), when told to land changes with nbranch, or when you receive an "nbranch:" or "nbranch review request" message.
---

# Working with nbranch

nbranch gives each agent its own git worktree and branch, and lands changes onto
the trunk through a queue that runs reviewers. Your identity is your nchat nick
(override with `NBRANCH_AGENT`).

## Rules

- Work only in your own worktree, `<root>/agents/<your nick>`. Never read from
  or write to other agents' worktrees or `nbranch/*` branches.
- Never commit to, push to, or move the trunk branch, and never edit
  `golden.git` directly. Everything reaches the trunk through `nbranch land`.
- Don't edit `.nbranch`. Lands that change it are rejected automatically; ask
  the user if the config needs to change.
- Only committed work is landed. Commit before landing.

## Workflow

1. `nbranch new` - creates (or shows) your worktree. `cd` into it.
2. Make changes and commit them on your branch.
3. `nbranch land` - queues your branch and returns a ticket number straight
   away. Don't poll; the result arrives as an nchat message starting with
   `nbranch: land #N`. Use `nbranch show N` if you need detail, or
   `nbranch land --wait` only when you can't receive messages.
4. Act on the result:
   - **LANDED** - run `nbranch reset` before starting new work, so you build
     on the new trunk.
   - **REJECTED** - fix what the reviewer comments describe, commit, and run
     `nbranch land` again. `nbranch show N` has the full reviewer output.
   - **CONFLICT** - run `nbranch sync`. If it stops on conflicts, resolve them
     in your worktree, `git add` the files, `git rebase --continue`, then land
     again. The trunk wasn't changed by the failed land.
   - **REVIEWER ERROR** - a reviewer crashed or timed out. That is not a
     verdict on your change: don't modify your code because of it; tell the
     user, and retry `nbranch land` if they ask.

Your worktree never changes underneath you; only `nbranch sync` and
`nbranch reset` (which you run) change it. `nbranch reset` refuses to discard
unlanded commits or uncommitted changes unless given `--force`; don't use
`--force` unless you mean to throw that work away.

Other commands: `nbranch queue` (what's waiting), `nbranch log` (recent
rejections with reviewer output), `nbranch cancel N`, `nbranch whoami`.

## Reviewing for others

If you receive `nbranch review request <id>`:

1. Review the change at the path in the message, e.g.
   `git -C <path> diff <base> <head>`. Don't modify files there.
2. Answer with your model id (or set `NBRANCH_MODEL`):
   - `nbranch respond <id> accept --model <model>`
   - `nbranch respond <id> reject --model <model> --comment 'src/x.rs:42: error: what is wrong and why'`
     (repeat `--comment`; forms: `path:line: severity: text`,
     `path: severity: text`, `severity: text` or plain text; severities are
     `error`, `warning` and `note`)
3. Reject only for real problems, and make every comment actionable. The
   lander gets your comments verbatim.

`nbranch requests` lists requests still waiting for you.
