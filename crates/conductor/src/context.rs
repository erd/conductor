use std::path::PathBuf;

use anyhow::Context as _;
use conductor_state::{Config, Home, RepoIdent, Settings, resolve_repo};

/// What every repo-scoped command starts from: the state root, the repo the current directory
/// belongs to, and the config settings for that repo.
pub struct Ctx {
    pub home: Home,
    pub repo: RepoIdent,
    #[expect(
        dead_code,
        reason = "read by `spawn` and `watch`, which later issues add"
    )]
    pub settings: Settings,
}

impl Ctx {
    /// Resolves the repo from the current directory, which may be the main checkout, a
    /// subdirectory, or a linked worktree, and loads `config.toml`.
    pub fn from_cwd() -> anyhow::Result<Self> {
        let home = Home::resolve()?;
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        let repo = resolve_repo(&cwd).context("not inside a git repo")?;
        let settings = Config::load(&home.config_path())?.resolve(&repo.id);
        Ok(Self {
            home,
            repo,
            settings,
        })
    }

    /// `repos/<repo-id>/`.
    pub fn repo_dir(&self) -> PathBuf {
        self.home.repo_dir(&self.repo.id)
    }

    /// The conductor's Claude session name, `conductor-<repo dir name>`.
    pub fn conductor_session(&self) -> String {
        let name = self
            .repo
            .root
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy());
        format!("conductor-{name}")
    }
}

/// The value of environment variable `name`, treating empty as unset.
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
