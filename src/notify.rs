use crate::config::Notify;
use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

/// Deliver `message` to the agent `to`. With the default nchat transport the
/// nchat plugin/extension injects it into the agent's running session.
pub fn send(notify: &Notify, to: &str, message: &str, extra: &[(&str, String)]) -> Result<()> {
    let out = match notify {
        Notify::Disabled => return Ok(()),
        Notify::Nchat { as_nick } => Command::new("nchat")
            .args(["msg", to, message])
            .env("NCHAT_NICK", as_nick)
            .env_remove("NCHAT_SESSION")
            .stdin(Stdio::null())
            .output()
            .context("running nchat (set notify_command in the config to use something else)")?,
        Notify::Command(cmd) => Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("NBRANCH_TO", to)
            .env("NBRANCH_MESSAGE", message)
            .envs(extra.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::null())
            .output()
            .context("running notify_command")?,
    };
    if !out.status.success() {
        bail!("notification failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
