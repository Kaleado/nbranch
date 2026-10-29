use crate::ticket::{State, Ticket, Verdict};
use crate::{git, util};

/// Human-readable description of a land. `output_tail` adds that many lines
/// of each failing reviewer's saved stdout/stderr (`usize::MAX` for all).
pub fn ticket_lines(t: &Ticket, output_tail: Option<usize>) -> Vec<String> {
    let mut out = vec![format!(
        "#{} {}  {}  ({} commit(s) from {}, {}..{}, {} ago)",
        t.id,
        t.agent,
        t.state,
        t.commits,
        t.branch,
        git::short(&t.base),
        git::short(&t.head),
        util::age(util::now().saturating_sub(t.created)),
    )];
    if let Some(d) = &t.detail {
        out.push(format!("  {d}"));
    }
    if t.state == State::Landed {
        if t.landed.is_empty() {
            out.push("  nothing to land: all changes were already on the trunk".into());
        } else {
            out.push(format!(
                "  landed {} commit(s): trunk {} -> {}",
                t.landed.len(),
                git::short(t.trunk_before.as_deref().unwrap_or("")),
                git::short(t.trunk_after.as_deref().unwrap_or("")),
            ));
        }
    }
    if t.attempts > 1 {
        out.push(format!("  picked {} times because the trunk moved", t.attempts));
    }
    if let Some(c) = &t.conflict {
        out.push(format!("  conflict in commit {} ({})", git::short(&c.commit), c.subject));
        out.push(format!("  conflicting files: {}", c.files.join(", ")));
    }
    if let Some(e) = &t.error {
        out.push(format!("  error: {e}"));
    }
    for r in &t.reviews {
        let who = match &r.reviewer {
            Some(id) => match &id.model {
                Some(m) => format!(" [{}, {}]", id.nick, m),
                None => format!(" [{}]", id.nick),
            },
            None => String::new(),
        };
        let verdict = match r.verdict {
            Verdict::Accepted => "accepted",
            Verdict::Rejected => "REJECTED",
            Verdict::Error => "ERROR",
        };
        out.push(format!(
            "  [{} #{}] {}{}: {} ({:.1}s)",
            r.stage.as_str(),
            r.attempt,
            r.name,
            who,
            verdict,
            r.duration_ms as f64 / 1000.0
        ));
        if let Some(e) = &r.error {
            out.push(format!("      error: {e}"));
        }
        for c in &r.comments {
            for line in c.to_string().lines() {
                out.push(format!("      {line}"));
            }
        }
        if r.verdict == Verdict::Accepted {
            continue;
        }
        let Some(n) = output_tail else { continue };
        for (label, path) in [("stdout", &r.stdout), ("stderr", &r.stderr)] {
            let Some(path) = path else { continue };
            let text = util::read_tail(path, n);
            if text.trim().is_empty() {
                continue;
            }
            out.push(format!("      {label} ({}):", path.display()));
            out.extend(text.lines().map(|l| format!("      | {l}")));
        }
    }
    out
}
