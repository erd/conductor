//! The conductor-deck Herdr mod's side of conductor: opening the deck beside the conductor,
//! which the conductor's `SessionStart` hook does, and `conductor deck --json`, the feed the deck
//! reads its cards from.
//!
//! The deck runs the feed with its space's folder as the current directory, so conductor
//! resolves the repo and the deck never computes repo ids. The feed carries data, not card
//! states: the deck mixes it with the live hc status only the app has.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::bail;
use conductor_state::{
    Phase, StatusVerb, Task, TaskKind, events_path, fold, read_decision, read_events,
    read_repo_record, read_task, task_ids,
};
use jiff::Timestamp;
use serde::Serialize;

use crate::context::{Ctx, env_var};
use crate::hc;
use crate::runner::{Cmd, Runner, Tools, check};
use crate::status::{ActivityKind, PrRow, activity_parts};

/// The deck's mod name, which is also its folder under the app's `mods/`.
pub const MOD: &str = "conductor-deck";

/// The Herdr app's home: `HERDR_APP_HOME`, else `~/.config/herdr-app`.
pub fn app_home() -> Option<PathBuf> {
    env_var("HERDR_APP_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(env_var("HOME")?).join(".config/herdr-app")))
}

/// Opens the deck in a split to the right of the conductor's `terminal`, showing `space`,
/// unless the deck isn't installed in `app_home`.
///
/// Only a mod can see which mods' panes are open, so this can't tell whether a deck is already
/// open; the hook opens it only when a conductor session starts afresh.
pub fn open(
    runner: &dyn Runner,
    tools: &Tools,
    app_home: &Path,
    terminal: &str,
    space: &str,
) -> anyhow::Result<()> {
    if !app_home.join("mods").join(MOD).join("main.js").is_file() {
        return Ok(());
    }
    let Some(place) = hc::locate(runner, tools, terminal)? else {
        bail!("Herdr doesn't show terminal {terminal}");
    };
    let props = serde_json::json!({ "space": space }).to_string();
    let split = Cmd::new(&tools.herdr_app).args([
        "do",
        "pane.split",
        "--direction",
        "right",
        "--pane",
        &place.pane,
        "--mod",
        MOD,
        "--props",
        &props,
    ]);
    check("herdr-app do pane.split", runner.run(&split)?)?;
    Ok(())
}

/// Prints the deck's feed for the repo containing the current directory.
pub fn run(json: bool) -> anyhow::Result<()> {
    if !json {
        bail!("`conductor deck` prints the deck's JSON feed; pass --json");
    }
    let ctx = Ctx::from_cwd()?;
    let feed = feed(&ctx.repo.id, &ctx.repo.root, &ctx.repo_dir())?;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &feed)?;
    writeln!(out)?;
    Ok(())
}

/// The whole feed: the repo, and every task in it.
#[derive(Debug, Serialize)]
pub struct Feed {
    pub repo: RepoFeed,
    pub tasks: Vec<TaskFeed>,
}

#[derive(Debug, Serialize)]
pub struct RepoFeed {
    pub id: String,
    /// The main checkout.
    pub root: PathBuf,
    /// The repo's state dir, `repos/<repo-id>/`, which the deck watches for changes.
    pub dir: PathBuf,
    /// The conductor's terminal, from `repo.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    /// The conductor's Herdr space, from `repo.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
}

/// One task, or the error that kept it from being read.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TaskFeed {
    Task(Box<TaskEntry>),
    Unreadable { id: String, error: String },
}

#[derive(Debug, Serialize)]
pub struct TaskEntry {
    pub id: String,
    pub title: String,
    pub kind: TaskKind,
    pub phase: Phase,
    /// When the latest `phase` event moved the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_since: Option<Timestamp>,
    pub activity: ActivityKind,
    /// The running tool and its summary, the permission message, the turn's error, or the exit
    /// reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the current activity began, or when the task was last updated if it has no
    /// session events.
    pub since: Timestamp,
    /// The worker's latest `conductor report`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    /// Open decisions, in key order.
    pub decisions: Vec<OpenDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// The PR's state as of the latest `pr` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrRow>,
    /// The worker's terminal: from its latest `session_start`, else `task.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    pub updated_at: Timestamp,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub verb: StatusVerb,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub ts: Timestamp,
}

#[derive(Debug, Serialize)]
pub struct OpenDecision {
    pub key: String,
    /// From `decisions/<key>.json`; left out if that can't be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub opened_at: Timestamp,
}

/// The feed for repo `id`, whose main checkout is `root` and whose state is in `repo_dir`.
/// Includes torn-down and abandoned tasks, oldest first; tasks that can't be read come last,
/// by id, each with its error.
pub fn feed(id: &str, root: &Path, repo_dir: &Path) -> anyhow::Result<Feed> {
    let record = read_repo_record(repo_dir)?;
    let repo = RepoFeed {
        id: id.to_owned(),
        root: root.to_owned(),
        dir: repo_dir.to_owned(),
        terminal_id: record.as_ref().and_then(|r| r.terminal_id.clone()),
        space_id: record.and_then(|r| r.space_id),
    };

    let mut tasks = Vec::new();
    let mut unreadable = Vec::new();
    let ids = if repo_dir.is_dir() {
        task_ids(repo_dir)?
    } else {
        Vec::new()
    };
    for task_id in ids {
        let dir = repo_dir.join("tasks").join(&task_id);
        match entry(&dir) {
            Ok(entry) => tasks.push(entry),
            Err(e) => unreadable.push(TaskFeed::Unreadable {
                id: task_id,
                error: e.to_string(),
            }),
        }
    }
    tasks.sort_by(|(created_a, a), (created_b, b)| (created_a, &a.id).cmp(&(created_b, &b.id)));
    let tasks = tasks
        .into_iter()
        .map(|(_, entry)| TaskFeed::Task(Box::new(entry)))
        .chain(unreadable)
        .collect();
    Ok(Feed { repo, tasks })
}

/// A task's entry, keyed by when it was created for sorting.
fn entry(task_dir: &Path) -> anyhow::Result<(Timestamp, TaskEntry)> {
    let task: Task = read_task(task_dir)?;
    let events = read_events(&events_path(task_dir), 0)?;
    let live = fold(&events.events);
    let (activity, detail) = activity_parts(live.activity);
    let decisions = live
        .open_decisions
        .into_iter()
        .map(|(key, opened_at)| OpenDecision {
            title: read_decision(task_dir, &key)
                .ok()
                .flatten()
                .map(|d| d.title),
            key,
            opened_at,
        })
        .collect();
    let entry = TaskEntry {
        id: task.id,
        title: task.title,
        kind: task.kind,
        phase: task.phase,
        phase_since: live.phase_since,
        activity,
        detail,
        since: live.since.unwrap_or(task.updated_at),
        report: live.status.map(|status| Report {
            verb: status.verb,
            text: status.text,
            pr_url: status.pr_url,
            ts: status.ts,
        }),
        decisions,
        branch: task.branch,
        pr_url: task.pr_url.or(live.pr_url),
        pr: live.pr.map(PrRow::from),
        terminal_id: live.terminal_id.or(task.terminal_id),
        updated_at: task.updated_at,
    };
    Ok((task.created_at, entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hc::tests::ls;
    use crate::runner::Fake;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status")
    }

    /// A Herdr home with the deck installed if `installed`.
    fn app_home(installed: bool) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        if installed {
            let dir = home.path().join("mods").join(MOD);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("main.js"), "").unwrap();
        }
        home
    }

    #[test]
    fn opens_beside_the_conductor() {
        let home = app_home(true);
        let fake = Fake::default()
            .ok(&["herdr-app", "ls"], &ls(None))
            .ok(&["herdr-app", "do", "pane.split"], r#"{"pane":"p5"}"#);
        open(&fake, &Tools::default(), home.path(), "t1", "sp1").unwrap();
        assert!(fake.done());
        assert_eq!(
            fake.argvs()[1],
            [
                "herdr-app",
                "do",
                "pane.split",
                "--direction",
                "right",
                "--pane",
                "p1",
                "--mod",
                "conductor-deck",
                "--props",
                r#"{"space":"sp1"}"#
            ]
        );
    }

    #[test]
    fn skips_when_not_installed() {
        let home = app_home(false);
        let fake = Fake::default();
        open(&fake, &Tools::default(), home.path(), "t1", "sp1").unwrap();
        assert_eq!(fake.calls(), []);
    }

    #[test]
    fn app_failures_are_errors() {
        let home = app_home(true);
        let fake = Fake::default().ok(&["herdr-app", "ls"], &ls(None));
        let err = open(&fake, &Tools::default(), home.path(), "t5", "sp1").unwrap_err();
        assert_eq!(err.to_string(), "Herdr doesn't show terminal t5");

        let fake = Fake::default().ok(&["herdr-app", "ls"], &ls(None)).fail(
            &["herdr-app", "do", "pane.split"],
            1,
            r#"{"error":"no pane"}"#,
        );
        let err = open(&fake, &Tools::default(), home.path(), "t1", "sp1").unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"`herdr-app do pane.split` failed (exit 1): {"error":"no pane"}"#
        );
    }

    /// The status fixtures, plus a task whose `task.json` doesn't parse.
    #[test]
    fn feed_of_the_status_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("web-3f9a1c");
        copy_dir(&fixtures(), &repo_dir);
        let broken = repo_dir.join("tasks/broken-09");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("task.json"), "{").unwrap();
        std::fs::write(
            repo_dir.join("repo.json"),
            r#"{"schema":"conductor-state.v2","repo_root":"/src/web","repo_id":"web-3f9a1c","conductor_session":"conductor-web","terminal_id":"t11","space_id":"sp1"}"#,
        )
        .unwrap();

        let feed = feed("web-3f9a1c", Path::new("/src/web"), &repo_dir).unwrap();
        let json = serde_json::to_string_pretty(&feed)
            .unwrap()
            .replace(&tmp.path().to_string_lossy().into_owned(), "<home>");
        insta::assert_snapshot!("feed_json", json);
    }

    #[test]
    fn feed_of_a_repo_with_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        let feed = feed(
            "web-3f9a1c",
            Path::new("/src/web"),
            &tmp.path().join("none"),
        )
        .unwrap();
        assert!(feed.tasks.is_empty());
        assert_eq!(feed.repo.terminal_id, None);
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), dest).unwrap();
            }
        }
    }
}
