use anyhow::{Context, Result};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write via a temporary file and rename, so readers never see a partial file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

/// Make a string safe to use as a file name component.
pub fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect()
}

pub fn valid_nick(nick: &str) -> bool {
    !nick.is_empty()
        && nick.len() <= 64
        && !nick.starts_with('.')
        && nick.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn read_tail(path: &Path, lines: usize) -> String {
    std::fs::read(path)
        .map(|b| tail(&String::from_utf8_lossy(&b), lines))
        .unwrap_or_default()
}

pub fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nick_validation() {
        assert!(valid_nick("witty-heron-64"));
        assert!(!valid_nick(""));
        assert!(!valid_nick("../evil"));
        assert!(!valid_nick("a b"));
    }

    #[test]
    fn tail_keeps_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }
}
