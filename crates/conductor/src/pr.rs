//! Reading a task's PR from GitHub with `gh pr view` and recording it as a `pr` event, shared by
//! `conductor watch`, `conductor teardown` and `conductor pr ready`, so `conductor status` shows
//! a PR's state as soon as one of them learns it.

use std::io::Write;
use std::path::Path;

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand};
use conductor_state::{
    CheckState, Event, EventPayload, Phase, PrState, PrStatus, ReviewDecision, TaskKind,
    append_event, events_path, fold, read_events, read_task,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::context::{Target, TargetArgs};
use crate::runner::{Cmd, RunOutput, Runner, System, Tools, check, failure, parse_json};
use crate::status::snake_case;
use crate::task::move_phase;

#[derive(Subcommand)]
pub enum PrCommand {
    /// Sign off a ship task's PR: mark it ready for review and move the task to
    /// `awaiting_merge`.
    ///
    /// Runs `gh pr ready` unless the PR is already ready, and records the PR's state, so
    /// `conductor status` shows it at once. The task must be in `review` or `awaiting_merge`.
    Ready(ReadyArgs),
}

#[derive(Args)]
pub struct ReadyArgs {
    /// The ship task whose PR to sign off.
    id: String,
    /// The repo id, e.g. `my-repo-3f9a1c`. Defaults to `CONDUCTOR_REPO_ID`, else the repo
    /// containing the current directory.
    #[arg(long, value_name = "ID")]
    repo: Option<String>,
}

pub fn run(command: &PrCommand) -> anyhow::Result<()> {
    match command {
        PrCommand::Ready(args) => {
            let target = Target::resolve(&TargetArgs {
                task: Some(args.id.clone()),
                repo: args.repo.clone(),
            })?;
            ready(
                &target.task_dir()?,
                &System,
                &Tools::resolve(),
                &mut std::io::stdout().lock(),
            )
        }
    }
}

/// What a `pr` event records about a PR, and what change detection compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub state: PrState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<CheckState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewDecision>,
}

impl From<&PrStatus> for PrSnapshot {
    fn from(pr: &PrStatus) -> Self {
        Self {
            state: pr.state,
            checks: pr.checks,
            review: pr.review,
        }
    }
}

/// The fields of `gh pr view --json state,isDraft,reviewDecision,statusCheckRollup`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPrView {
    state: String,
    #[serde(default)]
    is_draft: bool,
    /// Empty when no review is required.
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    status_check_rollup: Option<Vec<GhCheck>>,
}

/// One entry of `statusCheckRollup`: a `CheckRun` has `status` and `conclusion`, a
/// `StatusContext` has `state`.
#[derive(Deserialize)]
struct GhCheck {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

impl GhCheck {
    fn reduce(&self) -> CheckState {
        if let Some(state) = &self.state {
            return match state.as_str() {
                "FAILURE" | "ERROR" => CheckState::Fail,
                "PENDING" | "EXPECTED" => CheckState::Pending,
                _ => CheckState::Pass,
            };
        }
        if self.status.as_deref() != Some("COMPLETED") {
            return CheckState::Pending;
        }
        match self.conclusion.as_deref() {
            Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => {
                CheckState::Fail
            }
            _ => CheckState::Pass,
        }
    }
}

impl GhPrView {
    fn snapshot(&self) -> anyhow::Result<PrSnapshot> {
        let state = match self.state.as_str() {
            "OPEN" if self.is_draft => PrState::Draft,
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            other => bail!("unknown PR state {other:?}"),
        };
        let review = match self.review_decision.as_deref() {
            Some("APPROVED") => Some(ReviewDecision::Approved),
            Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
            Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
            _ => None,
        };
        Ok(PrSnapshot {
            state,
            checks: rollup(self.status_check_rollup.as_deref().unwrap_or_default()),
            review,
        })
    }
}

/// Any failing check fails the PR; otherwise any pending one keeps it pending. `None` when the PR
/// has no checks.
fn rollup(checks: &[GhCheck]) -> Option<CheckState> {
    let states: Vec<CheckState> = checks.iter().map(GhCheck::reduce).collect();
    if states.is_empty() {
        None
    } else if states.contains(&CheckState::Fail) {
        Some(CheckState::Fail)
    } else if states.contains(&CheckState::Pending) {
        Some(CheckState::Pending)
    } else {
        Some(CheckState::Pass)
    }
}

/// The `gh pr view` that reads what a [`PrSnapshot`] needs.
pub fn view_cmd(tools: &Tools, url: &str) -> Cmd {
    Cmd::new(&tools.gh).args([
        "pr",
        "view",
        url,
        "--json",
        "state,isDraft,reviewDecision,statusCheckRollup",
    ])
}

/// The PR as the output of [`view_cmd`] describes it.
pub fn parse_view(output: &RunOutput) -> anyhow::Result<PrSnapshot> {
    if !output.success() {
        return Err(failure("gh pr view", output));
    }
    parse_json::<GhPrView>(&output.stdout)
        .context("unexpected `gh pr view` output")
        .and_then(|view| view.snapshot())
}

/// Reads the PR at `url` from GitHub.
pub fn fetch(runner: &dyn Runner, tools: &Tools, url: &str) -> anyhow::Result<PrSnapshot> {
    parse_view(&runner.run(&view_cmd(tools, url))?)
}

/// Appends a `pr` event for task `id` unless `last`, the latest one, already says the same.
/// Returns whether it appended.
pub fn record(
    task_dir: &Path,
    id: &str,
    snapshot: PrSnapshot,
    last: Option<&PrStatus>,
) -> anyhow::Result<bool> {
    if last.map(PrSnapshot::from) == Some(snapshot) {
        return Ok(false);
    }
    let event = Event {
        ts: Timestamp::now(),
        task: id.to_owned(),
        session_id: None,
        payload: EventPayload::Pr {
            state: snapshot.state,
            checks: snapshot.checks,
            review: snapshot.review,
        },
    };
    append_event(task_dir, &event)?;
    Ok(true)
}

/// `conductor pr ready`: marks the task's PR ready unless it already is, records its state,
/// and moves the task to `awaiting_merge`. A refusal writes nothing.
fn ready(
    task_dir: &Path,
    runner: &dyn Runner,
    tools: &Tools,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let mut task = read_task(task_dir)?;
    let id = task.id.clone();
    let live = fold(&read_events(&events_path(task_dir), 0)?.events);
    if task.kind != TaskKind::Ship {
        bail!(
            "{id} is a {} task; only ship tasks have a PR",
            snake_case(&task.kind)
        );
    }
    let phase = task.phase;
    if phase != Phase::AwaitingMerge && !Phase::can_transition(phase, Phase::AwaitingMerge) {
        bail!(
            "{id} is {}; marking its PR ready needs review or awaiting_merge",
            snake_case(&phase)
        );
    }
    let url = task
        .pr_url
        .clone()
        .or(live.pr_url)
        .with_context(|| format!("{id} has no PR"))?;

    let mut snapshot = fetch(runner, tools, &url)?;
    match snapshot.state {
        PrState::Draft => {
            let cmd = Cmd::new(&tools.gh).args(["pr", "ready", &url]);
            check("gh pr ready", runner.run(&cmd)?)?;
            snapshot.state = PrState::Open;
            writeln!(out, "marked {url} ready")?;
        }
        PrState::Open => writeln!(out, "{url} is already ready")?,
        state @ (PrState::Merged | PrState::Closed) => {
            bail!(
                "{id}'s PR is {}; it can't be marked ready",
                snake_case(&state)
            );
        }
    }
    record(task_dir, &id, snapshot, live.pr.as_ref())?;
    if task.phase != Phase::AwaitingMerge {
        move_phase(task_dir, &mut task, Phase::AwaitingMerge)?;
    }
    writeln!(out, "{id} awaiting_merge")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use conductor_state::{SchemaV1, StatusVerb, Task, write_task_atomic};
    use tempfile::TempDir;

    use super::*;
    use crate::runner::Fake;
    use crate::status::collect;

    const ID: &str = "fix-login-07";
    const URL: &str = "https://github.com/o/my-repo/pull/12";
    const PASS: &str = r#"{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}"#;
    const FAIL: &str = r#"{"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}"#;
    const RUNNING: &str = r#"{"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""}"#;

    fn view(state: &str, draft: bool, review: &str, checks: &str) -> String {
        format!(
            r#"{{"isDraft":{draft},"reviewDecision":"{review}","state":"{state}","statusCheckRollup":[{checks}]}}"#
        )
    }

    /// A repo dir holding one task whose worker reported a PR.
    struct Fixture {
        tmp: TempDir,
    }

    impl Fixture {
        fn new(kind: TaskKind, phase: Phase) -> Self {
            let fx = Self {
                tmp: tempfile::tempdir().unwrap(),
            };
            let ts = "2026-10-14T12:00:00Z".parse().unwrap();
            let task = Task {
                schema: SchemaV1,
                id: ID.into(),
                title: "Fix login".into(),
                kind,
                phase,
                branch: Some("conductor/fix-login-07".into()),
                worktree_path: None,
                session_name: None,
                claude_session_id: None,
                herdr_workspace_id: None,
                herdr_pane_id: None,
                pr_url: None,
                created_at: ts,
                updated_at: ts,
            };
            std::fs::create_dir_all(fx.task_dir()).unwrap();
            write_task_atomic(&fx.task_dir(), &task).unwrap();
            fx.append(EventPayload::Status {
                verb: StatusVerb::Done,
                text: None,
                pr_url: Some(URL.into()),
            });
            fx
        }

        fn task_dir(&self) -> PathBuf {
            self.tmp.path().join("tasks").join(ID)
        }

        fn append(&self, payload: EventPayload) {
            let event = Event {
                ts: Timestamp::now(),
                task: ID.into(),
                session_id: None,
                payload,
            };
            append_event(&self.task_dir(), &event).unwrap();
        }

        fn events(&self) -> Vec<EventPayload> {
            read_events(&events_path(&self.task_dir()), 0)
                .unwrap()
                .events
                .into_iter()
                .map(|e| e.payload)
                .collect()
        }

        fn ready(&self, fake: &Fake) -> (anyhow::Result<()>, String) {
            let mut out = Vec::new();
            let result = ready(&self.task_dir(), fake, &Tools::default(), &mut out);
            (result, String::from_utf8(out).unwrap())
        }
    }

    #[test]
    fn draft_pr_is_marked_ready_and_shown_open() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Review);
        fx.append(EventPayload::Pr {
            state: PrState::Draft,
            checks: Some(CheckState::Pending),
            review: None,
        });
        let before = fx.events().len();
        let fake = Fake::default()
            .ok(
                &["gh", "pr", "view", URL],
                &view("OPEN", true, "REVIEW_REQUIRED", PASS),
            )
            .ok(&["gh", "pr", "ready", URL], "");
        let (result, out) = fx.ready(&fake);
        result.unwrap();
        assert!(fake.done());
        assert_eq!(
            fake.argvs()[0],
            [
                "gh",
                "pr",
                "view",
                URL,
                "--json",
                "state,isDraft,reviewDecision,statusCheckRollup"
            ]
        );
        assert_eq!(out, format!("marked {URL} ready\n{ID} awaiting_merge\n"));
        assert_eq!(
            fx.events()[before..],
            [
                EventPayload::Pr {
                    state: PrState::Open,
                    checks: Some(CheckState::Pass),
                    review: Some(ReviewDecision::ReviewRequired),
                },
                EventPayload::Phase {
                    from: Phase::Review,
                    to: Phase::AwaitingMerge,
                },
            ]
        );

        let rows = collect(fx.tmp.path(), false).unwrap();
        assert_eq!(rows[0].phase, Phase::AwaitingMerge);
        assert_eq!(rows[0].pr.as_ref().map(|pr| pr.state), Some(PrState::Open));
    }

    #[test]
    fn already_ready_pr_is_left_alone() {
        // After a fix-up round: the PR is ready and its state already recorded.
        let fx = Fixture::new(TaskKind::Ship, Phase::Review);
        fx.append(EventPayload::Pr {
            state: PrState::Open,
            checks: Some(CheckState::Pass),
            review: None,
        });
        let before = fx.events().len();
        let fake = Fake::default().ok(&["gh", "pr", "view", URL], &view("OPEN", false, "", PASS));
        let (result, out) = fx.ready(&fake);
        result.unwrap();
        assert!(fake.done(), "no gh pr ready");
        assert_eq!(
            out,
            format!("{URL} is already ready\n{ID} awaiting_merge\n")
        );
        assert_eq!(
            fx.events()[before..],
            [EventPayload::Phase {
                from: Phase::Review,
                to: Phase::AwaitingMerge,
            }]
        );

        // A rerun in awaiting_merge only records what changed.
        let fake = Fake::default().ok(&["gh", "pr", "view", URL], &view("OPEN", false, "", FAIL));
        fx.ready(&fake).0.unwrap();
        assert_eq!(
            fx.events().last(),
            Some(&EventPayload::Pr {
                state: PrState::Open,
                checks: Some(CheckState::Fail),
                review: None,
            })
        );
    }

    #[test]
    fn refusals_write_nothing() {
        let cases = [
            (
                TaskKind::Scout,
                Phase::Review,
                "fix-login-07 is a scout task; only ship tasks have a PR",
            ),
            (
                TaskKind::Ship,
                Phase::Working,
                "fix-login-07 is working; marking its PR ready needs review or awaiting_merge",
            ),
        ];
        for (kind, phase, message) in cases {
            let fx = Fixture::new(kind, phase);
            let before = fx.events();
            let fake = Fake::default();
            let (result, _) = fx.ready(&fake);
            assert_eq!(result.unwrap_err().to_string(), message);
            assert_eq!(fake.calls(), []);
            assert_eq!(fx.events(), before);
        }

        // No PR reported.
        let fx = Fixture::new(TaskKind::Ship, Phase::Review);
        std::fs::remove_file(events_path(&fx.task_dir())).unwrap();
        let fake = Fake::default();
        let (result, _) = fx.ready(&fake);
        assert_eq!(result.unwrap_err().to_string(), "fix-login-07 has no PR");
        assert_eq!(fake.calls(), []);

        for (state, name) in [("MERGED", "merged"), ("CLOSED", "closed")] {
            let fx = Fixture::new(TaskKind::Ship, Phase::Review);
            let before = fx.events();
            let fake = Fake::default().ok(&["gh", "pr", "view", URL], &view(state, false, "", ""));
            let (result, _) = fx.ready(&fake);
            assert_eq!(
                result.unwrap_err().to_string(),
                format!("fix-login-07's PR is {name}; it can't be marked ready")
            );
            assert!(fake.done());
            assert_eq!(fx.events(), before);
        }
    }

    #[test]
    fn failing_gh_pr_ready_writes_nothing() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Review);
        let before = fx.events();
        let fake = Fake::default()
            .ok(&["gh", "pr", "view", URL], &view("OPEN", true, "", ""))
            .fail(&["gh", "pr", "ready", URL], 1, "HTTP 502");
        let (result, _) = fx.ready(&fake);
        assert_eq!(
            result.unwrap_err().to_string(),
            "`gh pr ready` failed (exit 1): HTTP 502"
        );
        assert_eq!(fx.events(), before);
        assert_eq!(read_task(&fx.task_dir()).unwrap().phase, Phase::Review);
    }

    #[test]
    fn maps_gh_output() {
        let snapshot = |json: &str| {
            parse_json::<GhPrView>(json)
                .and_then(|view| view.snapshot())
                .map_err(|e| e.to_string())
        };
        assert_eq!(
            snapshot(&view("OPEN", true, "", "")),
            Ok(PrSnapshot {
                state: PrState::Draft,
                checks: None,
                review: None,
            })
        );
        assert_eq!(
            snapshot(&view("MERGED", false, "APPROVED", PASS)),
            Ok(PrSnapshot {
                state: PrState::Merged,
                checks: Some(CheckState::Pass),
                review: Some(ReviewDecision::Approved),
            })
        );
        assert_eq!(
            snapshot(
                r#"{"isDraft":false,"reviewDecision":null,"state":"CLOSED","statusCheckRollup":null}"#
            ),
            Ok(PrSnapshot {
                state: PrState::Closed,
                checks: None,
                review: None,
            })
        );
        assert_eq!(
            snapshot(&view("LOCKED", false, "", "")),
            Err("unknown PR state \"LOCKED\"".to_owned())
        );
    }

    #[test]
    fn rolls_up_checks() {
        let reduce = |checks: &[&str]| {
            let json = format!("[{}]", checks.join(","));
            rollup(&serde_json::from_str::<Vec<GhCheck>>(&json).unwrap())
        };
        let status = |state: &str| format!(r#"{{"__typename":"StatusContext","state":"{state}"}}"#);
        let run = |status: &str, conclusion: &str| {
            format!(
                r#"{{"__typename":"CheckRun","status":"{status}","conclusion":"{conclusion}"}}"#
            )
        };
        assert_eq!(reduce(&[]), None);
        assert_eq!(reduce(&[PASS]), Some(CheckState::Pass));
        assert_eq!(
            reduce(&[
                PASS,
                &run("COMPLETED", "SKIPPED"),
                &run("COMPLETED", "NEUTRAL")
            ]),
            Some(CheckState::Pass)
        );
        assert_eq!(reduce(&[PASS, RUNNING]), Some(CheckState::Pending));
        assert_eq!(
            reduce(&[PASS, &run("QUEUED", "")]),
            Some(CheckState::Pending)
        );
        assert_eq!(reduce(&[RUNNING, FAIL]), Some(CheckState::Fail));
        for conclusion in [
            "TIMED_OUT",
            "CANCELLED",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            assert_eq!(
                reduce(&[&run("COMPLETED", conclusion)]),
                Some(CheckState::Fail),
                "{conclusion}"
            );
        }
        assert_eq!(reduce(&[&status("SUCCESS")]), Some(CheckState::Pass));
        assert_eq!(reduce(&[&status("PENDING")]), Some(CheckState::Pending));
        assert_eq!(reduce(&[&status("EXPECTED")]), Some(CheckState::Pending));
        assert_eq!(reduce(&[PASS, &status("ERROR")]), Some(CheckState::Fail));
        assert_eq!(reduce(&[&status("FAILURE")]), Some(CheckState::Fail));
    }
}
