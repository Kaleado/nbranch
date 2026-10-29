use crate::{git, paths::Root};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub reviewer_timeout: Option<String>,
    pub gc_idle: Option<String>,
    pub notify_command: Option<String>,
    pub notify_as: Option<String>,
    pub setup_command: Option<String>,
    #[serde(default, rename = "reviewer")]
    pub reviewers: Vec<ReviewerFile>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReviewerFile {
    pub name: String,
    pub stage: Stage,
    pub command: String,
    pub timeout: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    PreQueue,
    InQueue,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::PreQueue => "pre-queue",
            Stage::InQueue => "in-queue",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Notify {
    /// `nchat msg <nick> <text>`, sent as `as_nick`.
    Nchat { as_nick: String },
    /// A shell command given NBRANCH_TO and NBRANCH_MESSAGE.
    Command(String),
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Reviewer {
    pub name: String,
    pub stage: Stage,
    pub command: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub gc_idle: Duration,
    pub notify: Notify,
    /// Run in every worktree nbranch creates, e.g. to link untracked assets.
    pub setup: Option<String>,
    pub reviewers: Vec<Reviewer>,
}

impl ConfigFile {
    pub fn parse(text: &str, origin: &str) -> Result<ConfigFile> {
        toml::from_str(text).with_context(|| format!("parsing {origin}"))
    }
}

pub fn global_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nbranch").join("config"))
}

impl Config {
    /// Load the global config, the root's own config (machine-specific
    /// settings that don't belong in the repo) and the repo's `.nbranch`,
    /// always read from the golden trunk so a branch can't change the rules
    /// it's reviewed under.
    pub fn load(root: &Root, trunk: &str) -> Result<Config> {
        let read = |p: Option<PathBuf>| -> Result<ConfigFile> {
            match p {
                Some(p) if p.is_file() => ConfigFile::parse(&std::fs::read_to_string(&p)?, &p.display().to_string()),
                _ => Ok(ConfigFile::default()),
            }
        };
        let global = read(global_path())?;
        let local = read(Some(root.state().join("config")))?;
        let golden = root.golden();
        let spec = format!("refs/heads/{trunk}:.nbranch");
        let repo = if git::output(&golden, &["cat-file", "-e", &spec])?.status.success() {
            ConfigFile::parse(&git::run(&golden, &["cat-file", "blob", &spec])?, ".nbranch on the trunk")?
        } else {
            ConfigFile::default()
        };
        Config::merge(vec![global, local, repo])
    }

    /// Later layers override earlier ones, except reviewers, which are
    /// combined: a later layer can add reviewers but can't remove or redefine
    /// earlier ones.
    pub fn merge(layers: Vec<ConfigFile>) -> Result<Config> {
        let mut merged = ConfigFile::default();
        for layer in layers {
            merged.reviewer_timeout = layer.reviewer_timeout.or(merged.reviewer_timeout);
            merged.gc_idle = layer.gc_idle.or(merged.gc_idle);
            merged.notify_command = layer.notify_command.or(merged.notify_command);
            merged.notify_as = layer.notify_as.or(merged.notify_as);
            merged.setup_command = layer.setup_command.or(merged.setup_command);
            merged.reviewers.extend(layer.reviewers);
        }
        let reviewer_timeout = parse_duration(merged.reviewer_timeout.as_deref().unwrap_or("15m"))?;
        let gc_idle = parse_duration(merged.gc_idle.as_deref().unwrap_or("24h"))?;
        let notify = match merged.notify_command {
            Some(c) if c.trim().is_empty() => Notify::Disabled,
            Some(c) => Notify::Command(c),
            None => Notify::Nchat { as_nick: merged.notify_as.unwrap_or_else(|| "nbranch".into()) },
        };
        let setup = merged.setup_command.filter(|c| !c.trim().is_empty());
        let mut reviewers: Vec<Reviewer> = Vec::new();
        for r in merged.reviewers {
            if r.name.trim().is_empty() {
                bail!("reviewer with an empty name");
            }
            if reviewers.iter().any(|x| x.name == r.name) {
                bail!("duplicate reviewer name '{}' (a repo can add reviewers but not redefine global ones)", r.name);
            }
            let timeout = match &r.timeout {
                Some(t) => parse_duration(t)?,
                None => reviewer_timeout,
            };
            reviewers.push(Reviewer { name: r.name, stage: r.stage, command: r.command, timeout });
        }
        Ok(Config { gc_idle, notify, setup, reviewers })
    }

    pub fn stage(&self, stage: Stage) -> Vec<&Reviewer> {
        self.reviewers.iter().filter(|r| r.stage == stage).collect()
    }
}

/// Parse durations like `90`, `30s`, `15m`, `2h`, `1d` or `500ms`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().with_context(|| format!("invalid duration '{s}'"))?;
    let secs = match unit.trim() {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        "ms" => return Ok(Duration::from_millis(n)),
        _ => bail!("invalid duration unit in '{s}' (use ms, s, m, h or d)"),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ConfigFile {
        ConfigFile::parse(text, "test").unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("5y").is_err());
    }

    #[test]
    fn repo_overrides_scalars_and_adds_reviewers() {
        let global = parse(
            r#"
            reviewer_timeout = "1m"
            notify_as = "bot"
            [[reviewer]]
            name = "lint"
            stage = "pre-queue"
            command = "true"
            "#,
        );
        let repo = parse(
            r#"
            reviewer_timeout = "2m"
            [[reviewer]]
            name = "tests"
            stage = "in-queue"
            command = "make test"
            timeout = "30s"
            "#,
        );
        let cfg = Config::merge(vec![global, ConfigFile::default(), repo]).unwrap();
        assert_eq!(cfg.notify, Notify::Nchat { as_nick: "bot".into() });
        assert_eq!(cfg.reviewers.len(), 2);
        assert_eq!(cfg.reviewers[0].timeout, Duration::from_secs(120));
        assert_eq!(cfg.reviewers[1].timeout, Duration::from_secs(30));
        assert_eq!(cfg.stage(Stage::InQueue)[0].name, "tests");
    }

    #[test]
    fn repo_cannot_redefine_global_reviewer() {
        let r = "[[reviewer]]\nname = \"lint\"\nstage = \"pre-queue\"\ncommand = \"true\"\n";
        let err = Config::merge(vec![parse(r), parse(r)]).unwrap_err();
        assert!(err.to_string().contains("duplicate reviewer"));
    }

    #[test]
    fn empty_notify_command_disables() {
        let cfg = Config::merge(vec![parse("notify_command = \"\""), ConfigFile::default()]).unwrap();
        assert_eq!(cfg.notify, Notify::Disabled);
    }

    #[test]
    fn middle_layer_overrides_global_and_is_overridden_by_repo() {
        let cfg = Config::merge(vec![
            parse("setup_command = \"global\"\ngc_idle = \"1h\""),
            parse("setup_command = \"local\"\ngc_idle = \"2h\""),
            parse("gc_idle = \"3h\""),
        ])
        .unwrap();
        assert_eq!(cfg.setup.as_deref(), Some("local"));
        assert_eq!(cfg.gc_idle, Duration::from_secs(3 * 3600));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(ConfigFile::parse("trunk = \"main\"", "test").is_err());
        assert!(ConfigFile::parse("[[reviewer]]\nname=\"a\"\nstage=\"later\"\ncommand=\"x\"", "test").is_err());
    }
}
