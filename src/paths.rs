use crate::git;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// An nbranch root:
///
/// ```text
/// <root>/golden.git/            the golden bare repository
/// <root>/golden.git/nbranch/    queue state, tickets, reviewer output
/// <root>/agents/<nick>/         agent worktrees
/// ```
#[derive(Clone, Debug)]
pub struct Root {
    pub dir: PathBuf,
}

impl Root {
    pub fn discover(explicit: Option<PathBuf>) -> Result<Root> {
        if let Some(dir) = explicit {
            let dir = dir
                .canonicalize()
                .with_context(|| format!("nbranch root {} not found", dir.display()))?;
            if !Root::is_root(&dir) {
                bail!("{} is not an nbranch root (no golden.git/nbranch)", dir.display());
            }
            return Ok(Root { dir });
        }
        let cwd = std::env::current_dir()?;
        for dir in cwd.ancestors() {
            if Root::is_root(dir) {
                return Ok(Root { dir: dir.to_path_buf() });
            }
        }
        bail!("not inside an nbranch root; run `nbranch init` or set NBRANCH_ROOT")
    }

    fn is_root(dir: &Path) -> bool {
        dir.join("golden.git").join("nbranch").is_dir()
    }

    pub fn init(dir: &Path, from: &str, trunk: &str) -> Result<Root> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        let golden = dir.join("golden.git");
        if golden.exists() {
            bail!("{} already exists", golden.display());
        }
        git::run(Path::new("."), &["clone", "--bare", "--quiet", from, &golden.to_string_lossy()])?;
        let root = Root { dir };
        let setup = || -> Result<()> {
            if git::try_rev_parse(&golden, &format!("refs/heads/{trunk}")).is_none() {
                bail!("trunk branch '{trunk}' not found in {from}");
            }
            git::run(&golden, &["symbolic-ref", "HEAD", &format!("refs/heads/{trunk}")])?;
            for d in [root.tickets_dir(), root.tmp_dir(), root.logs_dir(), root.requests_dir(), root.agents_dir()] {
                std::fs::create_dir_all(d)?;
            }
            Ok(())
        };
        if let Err(e) = setup() {
            let _ = std::fs::remove_dir_all(&golden);
            return Err(e);
        }
        Ok(root)
    }

    pub fn golden(&self) -> PathBuf {
        self.dir.join("golden.git")
    }

    pub fn state(&self) -> PathBuf {
        self.golden().join("nbranch")
    }

    pub fn tickets_dir(&self) -> PathBuf {
        self.state().join("tickets")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.state().join("tmp")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state().join("logs")
    }

    pub fn requests_dir(&self) -> PathBuf {
        self.state().join("requests")
    }

    pub fn reviews_dir(&self, ticket: u64) -> PathBuf {
        self.state().join("reviews").join(ticket.to_string())
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.dir.join("agents")
    }

    pub fn agent_worktree(&self, nick: &str) -> PathBuf {
        self.agents_dir().join(nick)
    }

    /// The trunk is whatever golden's HEAD points at (set by `nbranch init --trunk`).
    pub fn trunk(&self) -> Result<String> {
        git::run(&self.golden(), &["symbolic-ref", "--short", "HEAD"])
    }
}
