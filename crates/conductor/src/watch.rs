//! `conductor watch`: the conductor's backstop wake. Follows the `events.jsonl` of every live
//! task in a repo and exits 2, which wakes the conductor through its `asyncRewake` Stop hook,
//! when a worker needs it, when a worker gets a prompt while one of its decisions is open (the
//! user may have answered it in the pane), or when a task's PR is merged, closed, starts failing
//! checks or gets changes requested.

mod pr;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use clap::Args;
use conductor_state::{
    Config, Event, EventPayload, Phase, Settings, events_path, read_events, read_task, task_ids,
};
use jiff::SignedDuration;
use notify::{RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};

use self::pr::PrPoller;
use crate::context::{Target, TargetArgs};
use crate::pr::PrSnapshot;
use crate::runner::{System, Tools};

/// How often to rescan when no file notification arrives, in case one was missed.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The exit code that wakes the conductor.
const WAKE: u8 = 2;

#[derive(Args)]
pub struct WatchArgs {
    /// The repo id, e.g. `my-repo-3f9a1c`. Defaults to `CONDUCTOR_REPO_ID`, else the repo
    /// containing the current directory.
    #[arg(long, value_name = "ID")]
    repo: Option<String>,
    /// Exit 0 after this long with nothing to report, e.g. `30m`.
    #[arg(long, value_name = "DURATION", default_value = "8h", value_parser = parse_duration)]
    max_wait: Duration,
}

pub fn run(args: &WatchArgs) -> anyhow::Result<ExitCode> {
    let started = Instant::now();
    let target = Target::resolve(&TargetArgs {
        task: None,
        repo: args.repo.clone(),
    })?;
    let repo_dir = target.repo_dir()?;
    if !repo_dir.is_dir() {
        bail!(
            "no repo `{}` in {}; run `conductor init` in it first",
            target.repo_id,
            target.home.root().display()
        );
    }

    let lock_path = repo_dir.join("watch.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        // Another watcher already covers this repo.
        Err(TryLockError::WouldBlock) => return Ok(ExitCode::SUCCESS),
        Err(TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("cannot lock {}", lock_path.display()));
        }
    }

    let (tx, rx) = mpsc::channel();
    // Without notifications, the loop below still rescans every `POLL_INTERVAL`.
    let watcher = notify::recommended_watcher(tx).and_then(|mut watcher| {
        watcher.watch(&repo_dir, RecursiveMode::Recursive)?;
        Ok(watcher)
    });

    let tools = Tools::resolve();
    let mut poller = PrPoller::new(
        &System,
        &tools,
        pr_poll_interval(&target),
        target.home.logs_dir(),
        target.repo_id.clone(),
    );

    let mut state = WatchState::load(&repo_dir);
    loop {
        let wakes = state.scan();
        state.save()?;
        if !wakes.is_empty() {
            for (task, reasons) in &wakes {
                eprintln!("{task}: {}", reasons.join("; "));
            }
            return Ok(ExitCode::from(WAKE));
        }
        if poller.poll(&state, Instant::now()) {
            // Read the new `pr` events at once.
            continue;
        }

        let Some(left) = args.max_wait.checked_sub(started.elapsed()) else {
            return Ok(ExitCode::SUCCESS);
        };
        let wait = left.min(POLL_INTERVAL);
        if watcher.is_ok() {
            if rx.recv_timeout(wait).is_ok() {
                // Writes come in bursts; one rescan covers them all.
                while rx.try_recv().is_ok() {}
            }
        } else {
            std::thread::sleep(wait);
        }
    }
}

/// The repo's `pr_poll_interval`. A config that can't be read or a negative interval gives the
/// default, so the watcher still runs.
fn pr_poll_interval(target: &Target) -> Duration {
    let settings = Config::load(&target.home.config_path()).map_or_else(
        |e| {
            eprintln!("conductor: {e}");
            Settings::default()
        },
        |config| config.resolve(&target.repo_id),
    );
    Duration::try_from(settings.pr_poll_interval).unwrap_or_else(|_| {
        Duration::try_from(Settings::default().pr_poll_interval).unwrap_or_default()
    })
}

/// The watcher's position in each live task's log, saved in `repos/<id>/watch.cursor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursors {
    tasks: BTreeMap<String, TaskCursor>,
}

/// How far the watcher has read one task's `events.jsonl`, and what it needs to remember from
/// the events before that point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TaskCursor {
    /// Byte offset just past the last event read.
    offset: u64,
    /// Whether the worker has reported a `status` since the last prompt from the user or the
    /// conductor, or already woke the conductor for not doing so.
    #[serde(default)]
    reported: bool,
    /// The latest tool the worker started, for naming what a permission prompt is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_tool: Option<LastTool>,
    /// Keys of the task's decisions opened and not yet answered.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    open_decisions: BTreeSet<String>,
    /// Whether a prompt has already woken the conductor since the open decisions were opened.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    prompted: bool,
    /// The latest `pr_url` the worker reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pr_url: Option<String>,
    /// The PR as of the latest `pr` event, for that PR URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pr: Option<PrSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LastTool {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// The whole Bash command, when `summary` cuts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

impl TaskCursor {
    /// Takes in one event, returning why it should wake the conductor, if it should.
    fn apply(&mut self, event: &Event) -> Option<String> {
        match &event.payload {
            // A background job finishing starts a turn, but neither the user nor the conductor
            // asked anything.
            EventPayload::Prompt { text } if is_harness_prompt(text) => None,
            EventPayload::Prompt { .. } => {
                self.reported = false;
                if self.open_decisions.is_empty() || self.prompted {
                    return None;
                }
                // The deck and the conductor answer before they prompt the worker, so a prompt
                // while a decision is open is most likely the user, answering it in the pane.
                // Once is enough: a back-and-forth in the pane shouldn't wake the conductor on
                // every line.
                self.prompted = true;
                let keys: Vec<_> = self
                    .open_decisions
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect();
                let (noun, verb) = if keys.len() == 1 {
                    ("decision", "is")
                } else {
                    ("decisions", "are")
                };
                Some(format!(
                    "got a prompt while {noun} {} {verb} open",
                    keys.join(", ")
                ))
            }
            EventPayload::ToolStart {
                tool_name,
                summary,
                command,
                ..
            } => {
                self.last_tool = Some(LastTool {
                    name: tool_name.clone(),
                    summary: summary.clone(),
                    command: command.clone(),
                });
                None
            }
            EventPayload::Status { pr_url, .. } => {
                self.reported = true;
                if pr_url.is_some() && *pr_url != self.pr_url {
                    // A new PR; what was seen of the old one says nothing about it.
                    self.pr_url.clone_from(pr_url);
                    self.pr = None;
                }
                None
            }
            EventPayload::TurnEnd { error } => match error {
                Some(error) => Some(format!("turn ended with an error ({error})")),
                // A worker with an open decision is waiting on the user. Once woken, the
                // conductor needn't be again until the next prompt.
                None if !self.reported && self.open_decisions.is_empty() => {
                    self.reported = true;
                    Some("went idle without reporting".to_owned())
                }
                None => None,
            },
            EventPayload::Attention {
                notification_type,
                message,
            } => match notification_type.as_str() {
                // The whole command, so the conductor can tell what's waiting from the wake.
                "permission_prompt" => Some(match &self.last_tool {
                    Some(LastTool {
                        name,
                        summary,
                        command,
                    }) => match command.as_ref().or(summary.as_ref()) {
                        // One line per task, even for a multi-line command.
                        Some(input) => {
                            format!("needs permission ({name}: {})", input.replace('\n', "\\n"))
                        }
                        None => format!("needs permission ({name})"),
                    },
                    None => "needs permission".to_owned(),
                }),
                // Follows every turn_end after a minute; the turn_end rule already covers an
                // idle worker, and one that reported shouldn't wake the conductor again.
                "idle_prompt" => None,
                _ => Some(format!("needs attention ({message})")),
            },
            EventPayload::DecisionOpen { key } => {
                self.open_decisions.insert(key.clone());
                self.prompted = false;
                Some(format!("opened decision `{key}`"))
            }
            EventPayload::DecisionAnswered { key } => {
                self.open_decisions.remove(key);
                if self.open_decisions.is_empty() {
                    self.prompted = false;
                }
                None
            }
            EventPayload::SessionEnd { reason } => Some(format!("session ended ({reason})")),
            EventPayload::Pr {
                state,
                checks,
                review,
            } => {
                let new = PrSnapshot {
                    state: *state,
                    checks: *checks,
                    review: *review,
                };
                let reasons = new.wake_reasons(self.pr.replace(new));
                (!reasons.is_empty()).then(|| reasons.join("; "))
            }
            _ => None,
        }
    }
}

struct WatchState {
    repo_dir: PathBuf,
    cursor_path: PathBuf,
    cursors: Cursors,
    /// No cursor file was found, so tasks already present start at the end of their logs
    /// rather than replaying history. Cleared after the first scan.
    first_run: bool,
    /// What was last written to the cursor file.
    saved: Option<Cursors>,
}

impl WatchState {
    fn load(repo_dir: &Path) -> Self {
        let cursor_path = repo_dir.join("watch.cursor");
        let cursors = match fs::read(&cursor_path) {
            Ok(bytes) => serde_json::from_slice::<Cursors>(&bytes)
                .inspect_err(|e| {
                    eprintln!(
                        "conductor: ignoring unreadable {}: {e}",
                        cursor_path.display()
                    );
                })
                .ok(),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => {
                eprintln!("conductor: ignoring {}: {e}", cursor_path.display());
                None
            }
        };
        Self {
            repo_dir: repo_dir.to_owned(),
            cursor_path,
            first_run: cursors.is_none(),
            cursors: cursors.unwrap_or_default(),
            saved: None,
        }
    }

    /// Reads every live task's new events, returning the tasks that should wake the conductor
    /// and why, sorted by task id. Tasks no longer live are forgotten.
    fn scan(&mut self) -> BTreeMap<String, Vec<String>> {
        let ids = task_ids(&self.repo_dir).unwrap_or_else(|e| {
            eprintln!("conductor: {e}");
            Vec::new()
        });
        let tasks_dir = self.repo_dir.join("tasks");
        let live: Vec<(String, PathBuf)> = ids
            .into_iter()
            .map(|id| {
                let dir = tasks_dir.join(&id);
                (id, dir)
            })
            .filter(|(_, dir)| is_live(dir))
            .collect();
        self.cursors
            .tasks
            .retain(|id, _| live.iter().any(|(live_id, _)| live_id == id));

        let mut wakes = BTreeMap::new();
        for (id, dir) in live {
            let path = events_path(&dir);
            let cursor = self.cursors.tasks.entry(id.clone()).or_insert_with(|| {
                let mut cursor = TaskCursor::default();
                if self.first_run {
                    // Take in the history for its state, without waking on it.
                    if let Ok(batch) = read_events(&path, 0) {
                        for event in &batch.events {
                            cursor.apply(event);
                        }
                        cursor.offset = batch.offset;
                    }
                }
                cursor
            });
            let batch = match read_events(&path, cursor.offset) {
                Ok(batch) => batch,
                Err(conductor_state::Error::OffsetPastEnd { len, .. }) => {
                    eprintln!("conductor: {} shrank; skipping to its end", path.display());
                    cursor.offset = len;
                    continue;
                }
                Err(e) => {
                    eprintln!("conductor: {e}");
                    continue;
                }
            };
            let reasons: Vec<String> = batch
                .events
                .iter()
                .filter_map(|event| cursor.apply(event))
                .collect();
            cursor.offset = batch.offset;
            if !reasons.is_empty() {
                wakes.insert(id, reasons);
            }
        }
        self.first_run = false;
        wakes
    }

    /// Writes the cursors if they changed since the last save: a temp file, then `rename`.
    fn save(&mut self) -> anyhow::Result<()> {
        if self.saved.as_ref() == Some(&self.cursors) {
            return Ok(());
        }
        let mut json = serde_json::to_vec_pretty(&self.cursors)?;
        json.push(b'\n');
        let tmp = self.repo_dir.join(".watch.cursor.tmp");
        fs::write(&tmp, json)
            .and_then(|()| fs::rename(&tmp, &self.cursor_path))
            .with_context(|| format!("cannot write {}", self.cursor_path.display()))?;
        self.saved = Some(self.cursors.clone());
        Ok(())
    }
}

/// Whether the task in `task_dir` is still worth watching. A task whose `task.json` can't be
/// read counts as live: its dir appears just before its `task.json` does.
fn is_live(task_dir: &Path) -> bool {
    read_task(task_dir).map_or(true, |task| {
        !matches!(task.phase, Phase::TornDown | Phase::Abandoned)
    })
}

/// First lines of the prompts the harness injects, e.g. when a background shell or subagent
/// finishes, as opposed to prompts from the user or the conductor.
const HARNESS_PROMPTS: &[&str] = &["<task-notification>"];

fn is_harness_prompt(text: &str) -> bool {
    let text = text.trim_start();
    HARNESS_PROMPTS.iter().any(|tag| text.starts_with(tag))
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let duration: SignedDuration = s.parse().map_err(|e: jiff::Error| e.to_string())?;
    Duration::try_from(duration).map_err(|_| "must not be negative".to_owned())
}

#[cfg(test)]
mod tests {
    use conductor_state::{CheckState, PrState};

    use super::*;

    fn event(json: &str) -> Event {
        let mut value: serde_json::Value = serde_json::from_str(json).unwrap();
        value["ts"] = "2026-10-14T12:00:00Z".into();
        value["task"] = "fix-login-07".into();
        serde_json::from_value(value).unwrap()
    }

    fn apply_all(cursor: &mut TaskCursor, lines: &[&str]) -> Vec<Option<String>> {
        lines
            .iter()
            .map(|line| cursor.apply(&event(line)))
            .collect()
    }

    #[test]
    fn noise_does_not_wake() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"session_start","source":"startup"}"#,
                r#"{"kind":"prompt","text":"go"}"#,
                r#"{"kind":"tool_start","tool_name":"Read","summary":"a.rs"}"#,
                r#"{"kind":"tool_end","tool_name":"Read","summary":"a.rs","ok":true}"#,
                r#"{"kind":"decision_answered","key":"db"}"#,
                r#"{"kind":"phase","from":"briefed","to":"working"}"#,
                r#"{"kind":"something_new","x":1}"#,
            ],
        );
        assert!(wakes.iter().all(Option::is_none), "{wakes:?}");
    }

    #[test]
    fn turn_end_wakes_only_without_a_status_since_the_last_prompt() {
        let mut cursor = TaskCursor::default();
        let prompt = r#"{"kind":"prompt","text":"go"}"#;
        let wakes = apply_all(
            &mut cursor,
            &[
                prompt,
                r#"{"kind":"turn_end"}"#,
                prompt,
                r#"{"kind":"status","verb":"working"}"#,
                r#"{"kind":"turn_end"}"#,
                r#"{"kind":"turn_end"}"#,
                prompt,
                r#"{"kind":"turn_end"}"#,
                r#"{"kind":"turn_end"}"#,
                r#"{"kind":"status","verb":"done"}"#,
                r#"{"kind":"turn_end","error":"rate_limit"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                Some("went idle without reporting".to_owned()),
                None,
                None,
                None,
                None,
                None,
                Some("went idle without reporting".to_owned()),
                // Already woken for that prompt.
                None,
                None,
                Some("turn ended with an error (rate_limit)".to_owned()),
            ]
        );
    }

    #[test]
    fn harness_notifications_ask_for_no_report() {
        let mut cursor = TaskCursor::default();
        let notification = r#"{"kind":"prompt","text":"<task-notification>"}"#;
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"prompt","text":"go"}"#,
                r#"{"kind":"status","verb":"working","text":"plan.sh running"}"#,
                r#"{"kind":"turn_end"}"#,
                notification,
                r#"{"kind":"turn_end"}"#,
                notification,
                r#"{"kind":"turn_end"}"#,
                r#"{"kind":"prompt","text":"how's it going?"}"#,
                r#"{"kind":"turn_end"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some("went idle without reporting".to_owned()),
            ]
        );
    }

    #[test]
    fn open_decision_suppresses_idle_wake() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"decision_open","key":"approve-plan"}"#,
                r#"{"kind":"turn_end"}"#,
                r#"{"kind":"prompt","text":"what does step 3 do?"}"#,
                r#"{"kind":"turn_end"}"#,
                r#"{"kind":"decision_answered","key":"approve-plan"}"#,
                r#"{"kind":"prompt","text":"approve-plan: approved"}"#,
                r#"{"kind":"turn_end"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                Some("opened decision `approve-plan`".to_owned()),
                None,
                Some("got a prompt while decision `approve-plan` is open".to_owned()),
                None,
                None,
                None,
                Some("went idle without reporting".to_owned()),
            ]
        );
    }

    #[test]
    fn harness_notification_while_a_decision_is_open_does_not_wake() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"decision_open","key":"db"}"#,
                r#"{"kind":"prompt","text":"<task-notification>"}"#,
                r#"{"kind":"prompt","text":"db: pg"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                Some("opened decision `db`".to_owned()),
                None,
                Some("got a prompt while decision `db` is open".to_owned()),
            ]
        );
    }

    #[test]
    fn permission_prompt_names_the_last_tool() {
        let mut cursor = TaskCursor::default();
        let prompt = r#"{"kind":"attention","type":"permission_prompt","message":"Claude needs your permission to use Bash"}"#;
        let wakes = apply_all(
            &mut cursor,
            &[
                prompt,
                r#"{"kind":"tool_start","tool_name":"WebFetch"}"#,
                prompt,
                r#"{"kind":"tool_start","tool_name":"Bash","summary":"rm -rf build"}"#,
                prompt,
            ],
        );
        assert_eq!(
            wakes,
            [
                Some("needs permission".to_owned()),
                None,
                Some("needs permission (WebFetch)".to_owned()),
                None,
                Some("needs permission (Bash: rm -rf build)".to_owned()),
            ]
        );
    }

    #[test]
    fn permission_prompt_shows_the_whole_command() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"tool_start","tool_name":"Bash","summary":"cd /src/x && uv run cli…","command":"cd /src/x && uv run cli merge 1 2 --stage prod --commit"}"#,
                r#"{"kind":"attention","type":"permission_prompt","message":"Claude needs your permission"}"#,
                r#"{"kind":"tool_start","tool_name":"Bash","summary":"psql <<'SQL'…","command":"psql <<'SQL'\nDELETE FROM t;\nSQL"}"#,
                r#"{"kind":"attention","type":"permission_prompt","message":"Claude needs your permission"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                Some(
                    "needs permission (Bash: cd /src/x && uv run cli merge 1 2 --stage prod --commit)"
                        .to_owned()
                ),
                None,
                Some(r"needs permission (Bash: psql <<'SQL'\nDELETE FROM t;\nSQL)".to_owned()),
            ]
        );
    }

    #[test]
    fn other_wake_conditions() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"attention","type":"idle_prompt","message":"Claude is waiting for your input"}"#,
                r#"{"kind":"attention","type":"elicitation_dialog","message":"Pick one"}"#,
                r#"{"kind":"decision_open","key":"db-choice"}"#,
                r#"{"kind":"session_end","reason":"logout"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                Some("needs attention (Pick one)".to_owned()),
                Some("opened decision `db-choice`".to_owned()),
                Some("session ended (logout)".to_owned()),
            ]
        );
    }

    #[test]
    fn prompt_while_a_decision_is_open_wakes_once() {
        let mut cursor = TaskCursor::default();
        let prompt = r#"{"kind":"prompt","text":"Plan is approved"}"#;
        let wakes = apply_all(
            &mut cursor,
            &[
                prompt,
                r#"{"kind":"decision_open","key":"approve-plan"}"#,
                prompt,
                prompt,
                r#"{"kind":"decision_answered","key":"approve-plan"}"#,
                prompt,
                r#"{"kind":"decision_open","key":"db"}"#,
                r#"{"kind":"decision_open","key":"tone"}"#,
                prompt,
                r#"{"kind":"decision_answered","key":"db"}"#,
                prompt,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                Some("opened decision `approve-plan`".to_owned()),
                Some("got a prompt while decision `approve-plan` is open".to_owned()),
                None,
                None,
                None,
                Some("opened decision `db`".to_owned()),
                Some("opened decision `tone`".to_owned()),
                Some("got a prompt while decisions `db`, `tone` are open".to_owned()),
                None,
                None,
            ]
        );
    }

    #[test]
    fn pr_events_wake_on_transitions() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"pr","state":"draft","checks":"pending"}"#,
                r#"{"kind":"pr","state":"open","checks":"fail","review":"changes_requested"}"#,
                r#"{"kind":"pr","state":"open","checks":"fail","review":"approved"}"#,
                r#"{"kind":"pr","state":"open","checks":"pass","review":"approved"}"#,
                r#"{"kind":"pr","state":"closed","checks":"pass"}"#,
                r#"{"kind":"pr","state":"open","checks":"pass"}"#,
                r#"{"kind":"pr","state":"merged","checks":"pass","review":"approved"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                Some("PR checks failing; PR changes requested".to_owned()),
                None,
                None,
                Some("PR closed without merging".to_owned()),
                None,
                Some("PR merged".to_owned()),
            ]
        );
    }

    #[test]
    fn first_pr_event_wakes_on_a_failing_pr() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[r#"{"kind":"pr","state":"open","checks":"fail"}"#],
        );
        assert_eq!(wakes, [Some("PR checks failing".to_owned())]);
    }

    #[test]
    fn a_new_pr_url_forgets_the_old_pr() {
        let mut cursor = TaskCursor::default();
        let wakes = apply_all(
            &mut cursor,
            &[
                r#"{"kind":"status","verb":"done","pr_url":"https://github.com/erd/web/pull/42"}"#,
                r#"{"kind":"pr","state":"open","checks":"fail"}"#,
                r#"{"kind":"status","verb":"done","pr_url":"https://github.com/erd/web/pull/42"}"#,
                r#"{"kind":"pr","state":"open","checks":"fail"}"#,
                r#"{"kind":"status","verb":"done","pr_url":"https://github.com/erd/web/pull/43"}"#,
                r#"{"kind":"pr","state":"open","checks":"fail"}"#,
            ],
        );
        assert_eq!(
            wakes,
            [
                None,
                Some("PR checks failing".to_owned()),
                None,
                None,
                None,
                Some("PR checks failing".to_owned()),
            ]
        );
        assert_eq!(
            cursor.pr_url.as_deref(),
            Some("https://github.com/erd/web/pull/43")
        );
    }

    #[test]
    fn cursor_round_trips() {
        let cursors = Cursors {
            tasks: BTreeMap::from([(
                "fix-login-07".to_owned(),
                TaskCursor {
                    offset: 1234,
                    reported: true,
                    last_tool: Some(LastTool {
                        name: "Bash".to_owned(),
                        summary: Some("rm -rf build".to_owned()),
                        command: None,
                    }),
                    open_decisions: BTreeSet::from(["db".to_owned()]),
                    prompted: true,
                    pr_url: Some("https://github.com/erd/web/pull/42".to_owned()),
                    pr: Some(PrSnapshot {
                        state: PrState::Open,
                        checks: Some(CheckState::Fail),
                        review: None,
                    }),
                },
            )]),
        };
        let json = serde_json::to_string(&cursors).unwrap();
        assert_eq!(serde_json::from_str::<Cursors>(&json).unwrap(), cursors);
    }

    #[test]
    fn cursor_from_before_decision_tracking_loads() {
        let json = r#"{"tasks":{"fix-login-07":{"offset":10,"reported":false}}}"#;
        let cursors: Cursors = serde_json::from_str(json).unwrap();
        let cursor = &cursors.tasks["fix-login-07"];
        assert!(cursor.open_decisions.is_empty());
        assert!(!cursor.prompted);
    }

    #[test]
    fn cursor_from_before_whole_commands_loads() {
        let json = r#"{"tasks":{"fix-login-07":{"offset":10,"last_tool":{"name":"Bash","summary":"ls"}}}}"#;
        let cursors: Cursors = serde_json::from_str(json).unwrap();
        let last_tool = cursors.tasks["fix-login-07"].last_tool.as_ref().unwrap();
        assert_eq!(last_tool.command, None);
    }

    #[test]
    fn parses_max_wait() {
        assert_eq!(parse_duration("8h"), Ok(Duration::from_secs(8 * 3600)));
        assert_eq!(parse_duration("1m 30s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert!(parse_duration("-5s").is_err());
        assert!(parse_duration("soon").is_err());
    }
}
