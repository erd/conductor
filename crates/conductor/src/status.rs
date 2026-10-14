use std::fmt::Write as _;
use std::path::Path;

use conductor_state::{
    Activity, CheckState, LiveStatus, Phase, PrState, ReviewDecision, Task, TaskKind, events_path,
    fold, read_events, read_task, task_ids,
};
use jiff::Timestamp;
use serde::Serialize;

use crate::context::Ctx;
use crate::table::{self, cut};

/// Longest title the table shows before cutting it with `…`; activity uses the same limit.
const MAX_CELL: usize = 40;

/// Prints the repo's tasks as a table, or with `json` as a JSON array.
pub fn run(ctx: &Ctx, json: bool, all: bool) -> anyhow::Result<()> {
    let rows = collect(&ctx.repo_dir(), all)?;
    if json {
        print!("{}", render_json(&rows)?);
    } else {
        print!("{}", render_table(&rows, &ctx.repo.id, Timestamp::now()));
    }
    Ok(())
}

/// One task in `conductor status`, and one element of the `--json` array. Keys are stable;
/// unset optional fields are left out.
#[derive(Debug, Serialize)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub kind: TaskKind,
    pub phase: Phase,
    /// What the worker's session is doing, from the task's events.
    pub activity: ActivityKind,
    /// The running tool and its summary, the permission message, the turn's error, or the exit
    /// reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub open_decisions: Vec<String>,
    /// When the current activity began, or when the task was last updated if it has no
    /// session events.
    pub since: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// The PR's state as of the latest `pr` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    NotStarted,
    Working,
    NeedsPermission,
    Idle,
    Exited,
}

#[derive(Debug, Serialize)]
pub struct PrRow {
    pub state: PrState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<CheckState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewDecision>,
}

/// Reads every task in `repo_dir`, oldest first, leaving out torn-down tasks unless `all`. A
/// task that can't be read is reported on stderr and skipped, so it doesn't hide the rest.
pub fn collect(repo_dir: &Path, all: bool) -> anyhow::Result<Vec<Row>> {
    let mut tasks = Vec::new();
    for id in task_ids(repo_dir)? {
        let dir = repo_dir.join("tasks").join(&id);
        let read = read_task(&dir).and_then(|task| {
            let batch = read_events(&events_path(&dir), 0)?;
            for line in &batch.skipped {
                eprintln!(
                    "conductor: warning: {} at byte {}: {}",
                    events_path(&dir).display(),
                    line.offset,
                    line.error
                );
            }
            Ok((task, fold(&batch.events)))
        });
        match read {
            Ok((task, _)) if task.phase == Phase::TornDown && !all => {}
            Ok(task) => tasks.push(task),
            Err(err) => eprintln!("conductor: warning: skipping task {id}: {err}"),
        }
    }
    tasks.sort_by(|(a, _), (b, _)| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    Ok(tasks
        .into_iter()
        .map(|(task, live)| row(task, live))
        .collect())
}

fn row(task: Task, live: LiveStatus) -> Row {
    let (activity, detail) = match live.activity {
        Activity::NotStarted => (ActivityKind::NotStarted, None),
        Activity::Working { tool } => (
            ActivityKind::Working,
            tool.map(|tool| match tool.summary {
                Some(summary) => format!("{} {summary}", tool.name),
                None => tool.name,
            }),
        ),
        Activity::NeedsPermission { message } => (ActivityKind::NeedsPermission, Some(message)),
        Activity::Idle { error } => (ActivityKind::Idle, error),
        Activity::Exited { reason } => (ActivityKind::Exited, Some(reason)),
    };
    Row {
        id: task.id,
        title: task.title,
        kind: task.kind,
        phase: task.phase,
        activity,
        detail,
        open_decisions: live.open_decisions.into_iter().collect(),
        since: live.since.unwrap_or(task.updated_at),
        branch: task.branch,
        pr_url: task.pr_url.or(live.pr_url),
        pr: live.pr.map(|pr| PrRow {
            state: pr.state,
            checks: pr.checks,
            review: pr.review,
        }),
    }
}

/// `rows` as a pretty JSON array, ending in a newline.
pub fn render_json(rows: &[Row]) -> serde_json::Result<String> {
    Ok(serde_json::to_string_pretty(rows)? + "\n")
}

/// `rows` as an aligned table, with times in each activity measured up to `now`.
pub fn render_table(rows: &[Row], repo_id: &str, now: Timestamp) -> String {
    if rows.is_empty() {
        return format!("no tasks for {repo_id}\n");
    }
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|row| {
            [
                row.id.clone(),
                cut(&row.title, MAX_CELL),
                snake_case(&row.phase),
                activity_cell(row),
                duration_cell(now.duration_since(row.since).as_secs()),
                row.branch.clone().unwrap_or_else(|| "-".into()),
                pr_cell(row),
            ]
        })
        .collect();
    table::render(
        ["ID", "TITLE", "PHASE", "ACTIVITY", "FOR", "BRANCH", "PR"],
        &cells,
    )
}

fn activity_cell(row: &Row) -> String {
    let mut text = match (row.activity, &row.detail) {
        (ActivityKind::NotStarted, _) => "not started".to_owned(),
        (ActivityKind::Working, Some(tool)) => tool.clone(),
        (ActivityKind::Working, None) => "thinking".to_owned(),
        (ActivityKind::NeedsPermission, _) => "needs permission".to_owned(),
        (ActivityKind::Idle, None) => "idle".to_owned(),
        (ActivityKind::Idle, Some(_)) => "idle (error)".to_owned(),
        (ActivityKind::Exited, _) => "exited".to_owned(),
    };
    if !row.open_decisions.is_empty() {
        let _ = write!(text, " · decision: {}", row.open_decisions.join(", "));
    }
    cut(&text, MAX_CELL)
}

/// `45s`, `12m`, `3h05m` or `2d`.
fn duration_cell(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d", secs / 86400),
    }
}

/// `#42 open ✓ approved`: the PR number (or URL), then whatever the latest `pr` event says.
fn pr_cell(row: &Row) -> String {
    let Some(url) = &row.pr_url else {
        return "-".into();
    };
    let mut parts = vec![match url.rsplit_once("/pull/") {
        Some((_, number)) if number.bytes().all(|b| b.is_ascii_digit()) => format!("#{number}"),
        _ => url.clone(),
    }];
    if let Some(pr) = &row.pr {
        parts.push(snake_case(&pr.state));
        if let Some(checks) = pr.checks {
            parts.push(
                match checks {
                    CheckState::Pass => "✓",
                    CheckState::Fail => "✗",
                    CheckState::Pending => "…",
                }
                .into(),
            );
        }
        if let Some(review) = pr.review {
            parts.push(snake_case(&review).replace('_', " "));
        }
    }
    parts.join(" ")
}

/// The serde name of a unit enum variant, e.g. `awaiting_merge`.
pub fn snake_case(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status")
    }

    fn now() -> Timestamp {
        "2026-10-14T12:30:00Z".parse().unwrap()
    }

    #[test]
    fn zero_tasks() {
        let empty = tempfile::tempdir().unwrap();
        let rows = collect(empty.path(), true).unwrap();
        insta::assert_snapshot!("zero_tasks_table", render_table(&rows, "web-3f9a1c", now()));
        insta::assert_snapshot!("zero_tasks_json", render_json(&rows).unwrap());
    }

    #[test]
    fn fixture_tasks() {
        let rows = collect(&fixtures(), false).unwrap();
        insta::assert_snapshot!("tasks_table", render_table(&rows, "web-3f9a1c", now()));
        insta::assert_snapshot!("tasks_json", render_json(&rows).unwrap());
    }

    #[test]
    fn all_includes_torn_down() {
        let rows = collect(&fixtures(), true).unwrap();
        insta::assert_snapshot!("all_tasks_table", render_table(&rows, "web-3f9a1c", now()));
    }

    #[test]
    fn durations() {
        let cases = [
            (-5, "0s"),
            (45, "45s"),
            (60, "1m"),
            (3599, "59m"),
            (3600 + 5 * 60, "1h05m"),
            (2 * 86400 + 7, "2d"),
        ];
        for (secs, text) in cases {
            assert_eq!(duration_cell(secs), text, "{secs}");
        }
    }
}
