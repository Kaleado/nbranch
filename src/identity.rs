use crate::util;
use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

/// The calling agent's nick: `NBRANCH_AGENT`, else nchat's nick for this
/// session (`NCHAT_NICK`, or `nchat nick`).
pub fn resolve() -> Result<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let nick = match env("NBRANCH_AGENT").or_else(|| env("NCHAT_NICK")) {
        Some(n) => n.trim().to_string(),
        None => from_nchat()?,
    };
    if !util::valid_nick(&nick) {
        bail!("invalid nick '{nick}' (use letters, digits, '-', '_' and '.')");
    }
    Ok(nick)
}

fn from_nchat() -> Result<String> {
    let out = Command::new("nchat")
        .arg("nick")
        .stdin(Stdio::null())
        .output()
        .context("can't determine your nick: set NBRANCH_AGENT or run inside an nchat session")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.split_whitespace().next() {
        Some(nick) if out.status.success() => Ok(nick.to_string()),
        _ => bail!(
            "can't determine your nick from nchat ({}); set NBRANCH_AGENT",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}
