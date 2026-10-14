use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest as _, Sha256};

use crate::{Error, Result};

/// The root of conductor's state, `~/.config/conductor` unless `CONDUCTOR_HOME` says
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home(PathBuf);

impl Home {
    /// `$CONDUCTOR_HOME` if set, else `$HOME/.config/conductor`. This is `~/.config` on macOS
    /// too, not `~/Library/Application Support`.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`] if neither variable is set.
    pub fn resolve() -> Result<Self> {
        resolve_from(std::env::var_os("CONDUCTOR_HOME"), std::env::var_os("HOME"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(root.into())
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.0
    }

    /// `config.toml`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.0.join("config.toml")
    }

    /// `logs/`.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.0.join("logs")
    }

    /// `repos/<repo-id>/`.
    #[must_use]
    pub fn repo_dir(&self, repo_id: &str) -> PathBuf {
        self.0.join("repos").join(repo_id)
    }

    /// `repos/<repo-id>/tasks/<task-id>/`.
    #[must_use]
    pub fn task_dir(&self, repo_id: &str, task_id: &str) -> PathBuf {
        self.repo_dir(repo_id).join("tasks").join(task_id)
    }
}

fn resolve_from(conductor_home: Option<OsString>, home: Option<OsString>) -> Result<Home> {
    let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty());
    if let Some(root) = non_empty(conductor_home) {
        return Ok(Home::new(root));
    }
    let home = non_empty(home).ok_or(Error::NoHome)?;
    Ok(Home::new(PathBuf::from(home).join(".config/conductor")))
}

/// `repo.json` in a repo dir.
#[must_use]
pub fn repo_record_path(repo_dir: &Path) -> PathBuf {
    repo_dir.join("repo.json")
}

/// `task.json` in a task dir.
#[must_use]
pub fn task_path(task_dir: &Path) -> PathBuf {
    task_dir.join("task.json")
}

/// `events.jsonl` in a task dir.
#[must_use]
pub fn events_path(task_dir: &Path) -> PathBuf {
    task_dir.join("events.jsonl")
}

/// `decisions/<key>.json` in a task dir.
#[must_use]
pub fn decision_path(task_dir: &Path, key: &str) -> PathBuf {
    task_dir.join("decisions").join(format!("{key}.json"))
}

/// `brief.md` in a task dir.
#[must_use]
pub fn brief_path(task_dir: &Path) -> PathBuf {
    task_dir.join("brief.md")
}

/// `report.md` in a task dir.
#[must_use]
pub fn report_path(task_dir: &Path) -> PathBuf {
    task_dir.join("report.md")
}

/// A repo's main checkout and its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdent {
    /// Canonical path of the main checkout.
    pub root: PathBuf,
    /// [`repo_id_for_root`] of `root`.
    pub id: String,
}

/// Finds the main checkout of the repo containing `dir` and its id. Gives the same answer
/// from the main checkout, any subdirectory, and any linked worktree.
///
/// The main checkout is the parent of `git rev-parse --git-common-dir`, so a bare repo or a
/// submodule (whose common dir is under `.git/modules/`) isn't supported.
///
/// # Errors
///
/// [`Error::Git`] if `git` can't be run or `dir` isn't in a repo, and [`Error::Io`] if the
/// main checkout can't be canonicalized.
pub fn resolve_repo(dir: &Path) -> Result<RepoIdent> {
    let git_error = |message: String| Error::Git {
        dir: dir.to_owned(),
        message,
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .map_err(|e| git_error(e.to_string()))?;
    if !output.status.success() {
        return Err(git_error(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let stdout = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let common_dir = Path::new(std::ffi::OsStr::from_bytes(stdout));
    let main = common_dir
        .parent()
        .ok_or_else(|| git_error(format!("no parent for {}", common_dir.display())))?;
    let root = main.canonicalize().map_err(Error::io(main))?;
    let id = repo_id_for_root(&root);
    Ok(RepoIdent { root, id })
}

/// `<dir name>-<first 6 hex chars of sha256(root)>`, e.g. `hamlet-web-v2-3f9a1c`. `root`
/// should be canonical; [`resolve_repo`] takes care of that.
#[must_use]
pub fn repo_id_for_root(root: &Path) -> String {
    let name = root
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy());
    let hash = Sha256::digest(root.as_os_str().as_bytes());
    format!("{name}-{:02x}{:02x}{:02x}", hash[0], hash[1], hash[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conductor_home_wins() {
        let home = resolve_from(Some("/state".into()), Some("/Users/me".into())).unwrap();
        assert_eq!(home.root(), Path::new("/state"));
    }

    #[test]
    fn falls_back_to_dot_config() {
        let home = resolve_from(None, Some("/Users/me".into())).unwrap();
        assert_eq!(home.root(), Path::new("/Users/me/.config/conductor"));
        let home = resolve_from(Some(OsString::new()), Some("/Users/me".into())).unwrap();
        assert_eq!(home.root(), Path::new("/Users/me/.config/conductor"));
    }

    #[test]
    fn no_home() {
        assert!(matches!(resolve_from(None, None), Err(Error::NoHome)));
    }

    #[test]
    fn layout() {
        let home = Home::new("/h");
        let task = home.task_dir("web-3f9a1c", "fix-login-07");
        assert_eq!(task, Path::new("/h/repos/web-3f9a1c/tasks/fix-login-07"));
        assert_eq!(home.config_path(), Path::new("/h/config.toml"));
        assert_eq!(
            repo_record_path(&home.repo_dir("web-3f9a1c")),
            Path::new("/h/repos/web-3f9a1c/repo.json")
        );
        assert_eq!(task_path(&task), task.join("task.json"));
        assert_eq!(events_path(&task), task.join("events.jsonl"));
        assert_eq!(brief_path(&task), task.join("brief.md"));
        assert_eq!(report_path(&task), task.join("report.md"));
        assert_eq!(decision_path(&task, "db"), task.join("decisions/db.json"));
    }

    #[test]
    fn repo_id_format() {
        // Pinned so ids never drift: `printf %s /src/hamlet-web-v2 | shasum -a 256`.
        let id = repo_id_for_root(Path::new("/src/hamlet-web-v2"));
        assert_eq!(id, "hamlet-web-v2-e5336f");
        assert_ne!(id, repo_id_for_root(Path::new("/other/hamlet-web-v2")));
    }
}
