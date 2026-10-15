use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::Args;
use conductor_state::{Config, Home, RepoIdent, Settings, resolve_repo};

/// What every repo-scoped command starts from: the state root, the repo the current directory
/// belongs to, and the config settings for that repo.
pub struct Ctx {
    pub home: Home,
    pub repo: RepoIdent,
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

    /// The repo's main checkout directory name, which session names are built from.
    pub fn repo_name(&self) -> String {
        self.repo
            .root
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned())
    }

    /// The conductor's Claude session name, `conductor-<repo dir name>`.
    pub fn conductor_session(&self) -> String {
        format!("conductor-{}", self.repo_name())
    }

    /// A worker's Claude session name, `<repo dir name>-<task id>`.
    pub fn worker_session(&self, task_id: &str) -> String {
        format!("{}-{task_id}", self.repo_name())
    }
}

/// `--task` and `--repo`, for commands that act on one task's state and may be run from outside
/// that task's environment, e.g. by the deck.
#[derive(Args, Debug, Default)]
pub struct TargetArgs {
    /// The task id. Defaults to `CONDUCTOR_TASK_ID`.
    #[arg(long, value_name = "ID")]
    pub task: Option<String>,
    /// The repo id, e.g. `my-repo-3f9a1c`. Defaults to `CONDUCTOR_REPO_ID`, else the repo
    /// containing the current directory.
    #[arg(long, value_name = "ID")]
    pub repo: Option<String>,
}

/// The state a task-scoped command works on: the state root, a repo id, and, when one was given,
/// a task id. Unlike [`Ctx`] it doesn't need the current directory to be in the repo, and it
/// doesn't load `config.toml`.
pub struct Target {
    pub home: Home,
    pub repo_id: String,
    pub task_id: Option<String>,
}

impl Target {
    /// Each id from its flag, else its environment variable; the repo id falls back to the repo
    /// containing the current directory.
    pub fn resolve(args: &TargetArgs) -> anyhow::Result<Self> {
        let home = Home::resolve()?;
        let repo_id = if let Some(id) = args.repo.clone().or_else(|| env_var("CONDUCTOR_REPO_ID")) {
            id
        } else {
            let cwd = std::env::current_dir().context("cannot read the current directory")?;
            resolve_repo(&cwd)
                .context("not inside a git repo; pass --repo or set CONDUCTOR_REPO_ID")?
                .id
        };
        let task_id = args.task.clone().or_else(|| env_var("CONDUCTOR_TASK_ID"));
        Ok(Self {
            home,
            repo_id,
            task_id,
        })
    }

    /// `repos/<repo-id>/`.
    pub fn repo_dir(&self) -> anyhow::Result<PathBuf> {
        check_id("repo", &self.repo_id)?;
        Ok(self.home.repo_dir(&self.repo_id))
    }

    /// The task id, or an error saying how to give one.
    pub fn task_id(&self) -> anyhow::Result<&str> {
        self.task_id
            .as_deref()
            .context("no task: pass --task or set CONDUCTOR_TASK_ID")
    }

    /// The directory of the task with `id`, which must exist.
    pub fn task_dir_for(&self, id: &str) -> anyhow::Result<PathBuf> {
        check_id("task", id)?;
        let dir = self.repo_dir()?.join("tasks").join(id);
        if !dir.is_dir() {
            bail!("no task `{id}` in repo `{}`", self.repo_id);
        }
        Ok(dir)
    }

    /// The target task's directory, which must exist.
    pub fn task_dir(&self) -> anyhow::Result<PathBuf> {
        self.task_dir_for(self.task_id()?)
    }
}

/// Fails unless `id` is a plain path component, so it can't point outside the state dir.
fn check_id(what: &str, id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.starts_with('.') || id.contains(['/', '\\']) {
        bail!("invalid {what} id {id:?}");
    }
    Ok(())
}

/// The value of environment variable `name`, treating empty as unset.
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
