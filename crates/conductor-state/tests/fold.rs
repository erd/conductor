//! Folds each fixture under `tests/fixtures/` and checks the resulting [`LiveStatus`]. The
//! deck reuses these fixtures, so each one is a realistic event log for one card state.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use conductor_state::{
    Activity, CheckState, CurrentTool, Event, LastStatus, LiveStatus, PrState, PrStatus,
    ReviewDecision, StatusVerb, fold, read_events,
};
use jiff::Timestamp;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Reads a fixture, insisting that every line parses.
fn events(name: &str) -> Vec<Event> {
    let batch = read_events(&fixture_path(name), 0).unwrap();
    assert!(batch.skipped.is_empty(), "{name}: {:?}", batch.skipped);
    batch.events
}

fn ts(time: &str) -> Timestamp {
    format!("2026-10-14T{time}Z").parse().unwrap()
}

/// The status every fixture shares after its `session_start` (session `s1` in pane `p1`).
fn base(activity: Activity, since: &str) -> LiveStatus {
    LiveStatus {
        activity,
        since: Some(ts(since)),
        session_id: Some("s1".into()),
        herdr_pane_id: Some("p1".into()),
        herdr_workspace_id: Some("w1".into()),
        ..LiveStatus::default()
    }
}

fn tool(name: &str, summary: &str) -> Activity {
    Activity::Working {
        tool: Some(CurrentTool {
            name: name.into(),
            summary: Some(summary.into()),
        }),
    }
}

const THINKING: Activity = Activity::Working { tool: None };
const PR_URL: &str = "https://github.com/erd/web/pull/42";

#[test]
fn every_fixture_parses() {
    let dir = fixture_path("");
    let mut count = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(!events(name).is_empty(), "{name} is empty");
            count += 1;
        }
    }
    assert!(count >= 13, "only {count} fixtures in {}", dir.display());
}

#[test]
fn not_started() {
    assert_eq!(fold(&[]), LiveStatus::default());
    assert_eq!(LiveStatus::default().activity, Activity::NotStarted);
}

#[test]
fn working_tool() {
    let status = fold(&events("working_tool.jsonl"));
    // `since` is the prompt: changing tools doesn't restart the clock.
    assert_eq!(status, base(tool("Edit", "src/login.rs"), "12:00:01"));
}

#[test]
fn working_between_tools() {
    let status = fold(&events("working_between_tools.jsonl"));
    assert_eq!(status, base(THINKING, "12:00:01"));
}

#[test]
fn needs_permission() {
    let status = fold(&events("needs_permission.jsonl"));
    let activity = Activity::NeedsPermission {
        message: "Claude needs your permission to use Bash".into(),
    };
    assert_eq!(status, base(activity, "12:00:11"));
}

#[test]
fn permission_granted() {
    let status = fold(&events("permission_granted.jsonl"));
    assert_eq!(status, base(THINKING, "12:01:30"));
}

#[test]
fn idle() {
    let status = fold(&events("idle.jsonl"));
    // The later `idle_prompt` doesn't restart the clock.
    let expected = LiveStatus {
        status: Some(LastStatus {
            verb: StatusVerb::Done,
            text: Some("Draft PR open".into()),
            ts: ts("12:05:00"),
        }),
        pr_url: Some(PR_URL.into()),
        ..base(Activity::Idle { error: None }, "12:05:02")
    };
    assert_eq!(status, expected);
}

#[test]
fn idle_error() {
    let status = fold(&events("idle_error.jsonl"));
    let activity = Activity::Idle {
        error: Some("rate_limit".into()),
    };
    assert_eq!(status, base(activity, "12:00:20"));
}

#[test]
fn exited() {
    let status = fold(&events("exited.jsonl"));
    // The stray `tool_end` after `session_end` doesn't revive the session.
    let activity = Activity::Exited {
        reason: "prompt_input_exit".into(),
    };
    assert_eq!(status, base(activity, "12:10:00"));
}

#[test]
fn resumed() {
    let status = fold(&events("resumed.jsonl"));
    let expected = LiveStatus {
        activity: Activity::Idle { error: None },
        since: Some(ts("13:00:00")),
        session_id: Some("s2".into()),
        herdr_pane_id: Some("p2".into()),
        herdr_workspace_id: Some("w2".into()),
        ..LiveStatus::default()
    };
    assert_eq!(status, expected);
}

#[test]
fn compact_mid_turn() {
    let status = fold(&events("compact_mid_turn.jsonl"));
    assert_eq!(status, base(tool("Edit", "src/login.rs"), "12:00:01"));
}

#[test]
fn decisions() {
    let status = fold(&events("decisions.jsonl"));
    let expected = LiveStatus {
        open_decisions: BTreeSet::from(["auth-flow".into()]),
        status: Some(LastStatus {
            verb: StatusVerb::Blocked,
            text: Some("Waiting on two decisions".into()),
            ts: ts("12:02:02"),
        }),
        ..base(Activity::Idle { error: None }, "12:02:03")
    };
    assert_eq!(status, expected);
}

#[test]
fn pr_lifecycle() {
    let events = events("pr_lifecycle.jsonl");
    let pr = |state, checks, review, time| {
        Some(PrStatus {
            state,
            checks: Some(checks),
            review,
            ts: ts(time),
        })
    };
    let steps: Vec<_> = (1..=events.len()).map(|n| fold(&events[..n]).pr).collect();
    let mut expected = vec![None; 3];
    expected.extend([
        pr(PrState::Draft, CheckState::Pending, None, "12:06:00"),
        // A `phase` event leaves the PR alone.
        pr(PrState::Draft, CheckState::Pending, None, "12:06:00"),
        pr(
            PrState::Open,
            CheckState::Fail,
            Some(ReviewDecision::ReviewRequired),
            "12:08:00",
        ),
        pr(
            PrState::Open,
            CheckState::Pass,
            Some(ReviewDecision::ChangesRequested),
            "12:20:00",
        ),
        pr(
            PrState::Open,
            CheckState::Pass,
            Some(ReviewDecision::Approved),
            "12:40:00",
        ),
        pr(
            PrState::Merged,
            CheckState::Pass,
            Some(ReviewDecision::Approved),
            "13:00:00",
        ),
    ]);
    assert_eq!(steps, expected);

    let status = fold(&events);
    assert_eq!(status.pr_url.as_deref(), Some(PR_URL));
    assert_eq!(status.activity, THINKING);
}

#[test]
fn pr_closed() {
    let status = fold(&events("pr_closed.jsonl"));
    let expected = PrStatus {
        state: PrState::Closed,
        checks: None,
        review: None,
        ts: ts("12:30:00"),
    };
    assert_eq!(status.pr, Some(expected));
}

#[test]
fn unknown_kind_is_ignored() {
    let status = fold(&events("unknown_kind.jsonl"));
    assert_eq!(status, base(tool("Edit", "src/login.rs"), "12:00:01"));
}

#[test]
fn apply_matches_fold() {
    let events = events("pr_lifecycle.jsonl");
    let mut status = LiveStatus::default();
    for event in &events {
        status.apply(event);
    }
    assert_eq!(status, fold(&events));
}
