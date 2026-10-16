//! PR polling for `conductor watch`: runs `gh pr view` for each live task with a PR and appends a
//! `pr` event when the PR changes. The poller never wakes the conductor itself; the watcher wakes
//! when it reads the `pr` event back, so a watcher that dies right after the append still wakes
//! on its next run.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use conductor_state::{
    CheckState, Event, EventPayload, PrState, ReviewDecision, append_event, read_task,
};
use jiff::{SignedDuration, Timestamp};

use super::WatchState;
use crate::log::append_log;
use crate::pr::{PrSnapshot, parse_view, view_cmd};
use crate::runner::{Runner, Tools};

/// The longest wait between retries of a failing `gh pr view`.
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

impl PrSnapshot {
    /// Why moving from `old` to `self` should wake the conductor: merged, closed, checks starting
    /// to fail, or changes newly requested.
    pub fn wake_reasons(self, old: Option<Self>) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        let was = |f: fn(&Self) -> bool| old.as_ref().is_some_and(f);
        if self.state == PrState::Merged && !was(|old| old.state == PrState::Merged) {
            reasons.push("PR merged");
        }
        if self.state == PrState::Closed && !was(|old| old.state == PrState::Closed) {
            reasons.push("PR closed without merging");
        }
        if self.checks == Some(CheckState::Fail) && !was(|old| old.checks == Some(CheckState::Fail))
        {
            reasons.push("PR checks failing");
        }
        if self.review == Some(ReviewDecision::ChangesRequested)
            && !was(|old| old.review == Some(ReviewDecision::ChangesRequested))
        {
            reasons.push("PR changes requested");
        }
        reasons
    }
}

/// When a task's PR is next due, and how many polls in a row have failed.
#[derive(Debug, Clone, Copy)]
struct Schedule {
    next: Instant,
    failures: u32,
}

/// Polls the PRs of a repo's live tasks.
pub(super) struct PrPoller<'a> {
    runner: &'a dyn Runner,
    tools: &'a Tools,
    interval: Duration,
    logs_dir: PathBuf,
    repo_id: String,
    /// Set once `gh` can't be run at all.
    disabled: bool,
    schedule: HashMap<String, Schedule>,
}

impl<'a> PrPoller<'a> {
    pub fn new(
        runner: &'a dyn Runner,
        tools: &'a Tools,
        interval: Duration,
        logs_dir: PathBuf,
        repo_id: String,
    ) -> Self {
        Self {
            runner,
            tools,
            interval,
            logs_dir,
            repo_id,
            disabled: false,
            schedule: HashMap::new(),
        }
    }

    /// Polls each due PR among `state`'s live tasks, appending a `pr` event for each one that
    /// changed. Returns whether it appended any.
    ///
    /// A task's PR URL comes from its `task.json`, else from its latest `status` event. A task
    /// without one is never polled, nor is a merged PR. A task seen for the first time is due at
    /// once.
    pub fn poll(&mut self, state: &WatchState, now: Instant) -> bool {
        self.schedule
            .retain(|id, _| state.cursors.tasks.contains_key(id));
        if self.disabled {
            return false;
        }
        let tasks_dir = state.repo_dir.join("tasks");
        let mut appended = false;
        for (id, cursor) in &state.cursors.tasks {
            if cursor.pr.is_some_and(|pr| pr.state == PrState::Merged) {
                continue;
            }
            let dir = tasks_dir.join(id);
            let Some(url) = read_task(&dir)
                .ok()
                .and_then(|task| task.pr_url)
                .or_else(|| cursor.pr_url.clone())
            else {
                continue;
            };
            let schedule = self.schedule.entry(id.clone()).or_insert(Schedule {
                next: now,
                failures: 0,
            });
            if now < schedule.next {
                continue;
            }

            let output = match self.runner.run(&view_cmd(self.tools, &url)) {
                Ok(output) => output,
                Err(err) => {
                    let msg = format!("{err:#}; PR polling disabled");
                    eprintln!("conductor: {msg}");
                    self.log(&msg);
                    self.disabled = true;
                    return appended;
                }
            };
            let snapshot = match parse_view(&output) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    schedule.failures += 1;
                    let delay = backoff(self.interval, schedule.failures);
                    schedule.next = now + delay;
                    let delay = SignedDuration::try_from(delay).unwrap_or(SignedDuration::MAX);
                    let msg = format!("{id}: {url}: {err:#}; retrying in {delay:#}");
                    self.log(&msg.replace('\n', " "));
                    continue;
                }
            };
            *schedule = Schedule {
                next: now + self.interval,
                failures: 0,
            };
            if cursor.pr == Some(snapshot) {
                continue;
            }
            let event = Event {
                ts: Timestamp::now(),
                task: id.clone(),
                session_id: None,
                payload: EventPayload::Pr {
                    state: snapshot.state,
                    checks: snapshot.checks,
                    review: snapshot.review,
                },
            };
            match append_event(&dir, &event) {
                Ok(()) => appended = true,
                Err(err) => self.log(&format!("{id}: {err}")),
            }
        }
        appended
    }

    /// Appends `msg` to `logs/watch.log`.
    fn log(&self, msg: &str) {
        let line = format!(
            "{} watch repo={} pid={}: {msg}",
            Timestamp::now(),
            self.repo_id,
            std::process::id()
        );
        if let Err(err) = append_log(&self.logs_dir, "watch.log", &line) {
            eprintln!(
                "conductor: cannot write to {}: {err}",
                self.logs_dir.display()
            );
        }
    }
}

/// How long to wait after the `failures`th failed poll in a row: the interval doubled for each,
/// up to [`MAX_BACKOFF`], and never less than the interval.
fn backoff(interval: Duration, failures: u32) -> Duration {
    let factor = 1u32 << failures.min(16);
    interval
        .saturating_mul(factor)
        .min(MAX_BACKOFF)
        .max(interval)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use anyhow::anyhow;
    use conductor_state::{events_path, read_events};

    use super::*;
    use crate::runner::{Cmd, Fake, RunOutput};

    const INTERVAL: Duration = Duration::from_secs(120);
    const URL: &str = "https://github.com/erd/web/pull/42";

    fn view(state: &str, draft: bool, review: &str, checks: &str) -> String {
        format!(
            r#"{{"isDraft":{draft},"reviewDecision":"{review}","state":"{state}","statusCheckRollup":[{checks}]}}"#
        )
    }

    const PASS: &str = r#"{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}"#;
    const FAIL: &str = r#"{"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}"#;
    const RUNNING: &str = r#"{"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""}"#;

    struct Repo {
        tmp: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            Self {
                tmp: tempfile::tempdir().unwrap(),
            }
        }

        fn dir(&self) -> &Path {
            self.tmp.path()
        }

        fn logs_dir(&self) -> PathBuf {
            self.dir().join("logs")
        }

        fn task_dir(&self, id: &str) -> PathBuf {
            self.dir().join("tasks").join(id)
        }

        /// Adds a task, with `pr_url` in its `task.json` if given.
        fn task(&self, id: &str, pr_url: Option<&str>) -> &Self {
            let dir = self.task_dir(id);
            std::fs::create_dir_all(&dir).unwrap();
            let mut task = serde_json::json!({
                "schema": "conductor-state.v1",
                "id": id,
                "title": "A task",
                "kind": "ship",
                "phase": "awaiting_merge",
                "created_at": "2026-10-14T08:00:00Z",
                "updated_at": "2026-10-14T08:00:00Z",
            });
            if let Some(url) = pr_url {
                task["pr_url"] = url.into();
            }
            std::fs::write(dir.join("task.json"), task.to_string()).unwrap();
            self
        }

        fn append(&self, id: &str, event: &str) {
            let mut value: serde_json::Value = serde_json::from_str(event).unwrap();
            value["ts"] = "2026-10-14T12:00:00Z".into();
            value["task"] = id.into();
            let event: Event = serde_json::from_value(value).unwrap();
            append_event(&self.task_dir(id), &event).unwrap();
        }

        fn pr_events(&self, id: &str) -> Vec<EventPayload> {
            read_events(&events_path(&self.task_dir(id)), 0)
                .unwrap()
                .events
                .into_iter()
                .map(|event| event.payload)
                .filter(|payload| matches!(payload, EventPayload::Pr { .. }))
                .collect()
        }

        fn watch_log(&self) -> String {
            std::fs::read_to_string(self.logs_dir().join("watch.log")).unwrap_or_default()
        }

        /// A watcher state that has done its first scan.
        fn state(&self) -> WatchState {
            let mut state = WatchState::load(self.dir());
            assert!(state.scan().is_empty());
            state
        }

        fn poller<'a>(&self, runner: &'a dyn Runner, tools: &'a Tools) -> PrPoller<'a> {
            PrPoller::new(
                runner,
                tools,
                INTERVAL,
                self.logs_dir(),
                "my-repo-abc123".to_owned(),
            )
        }
    }

    /// The wake reasons for `id` from one scan.
    fn wakes(state: &mut WatchState, id: &str) -> Option<Vec<String>> {
        state.scan().remove(id)
    }

    #[test]
    fn wakes_on_each_transition() {
        let repo = Repo::new();
        repo.task("fix-login-07", Some(URL));
        let mut state = repo.state();
        let fake = Fake::default()
            .ok(&["gh", "pr", "view", URL], &view("OPEN", true, "", RUNNING))
            .ok(
                &["gh", "pr", "view", URL],
                &view("OPEN", false, "REVIEW_REQUIRED", FAIL),
            )
            .ok(
                &["gh", "pr", "view", URL],
                &view("OPEN", false, "REVIEW_REQUIRED", FAIL),
            )
            .ok(
                &["gh", "pr", "view", URL],
                &view("OPEN", false, "CHANGES_REQUESTED", PASS),
            )
            .ok(
                &["gh", "pr", "view", URL],
                &view("OPEN", false, "APPROVED", FAIL),
            )
            .ok(
                &["gh", "pr", "view", URL],
                &view("MERGED", false, "APPROVED", PASS),
            );
        let tools = Tools::default();
        let mut poller = repo.poller(&fake, &tools);
        let t0 = Instant::now();
        let expected: [Option<&[&str]>; 6] = [
            None,
            Some(&["PR checks failing"]),
            None,
            Some(&["PR changes requested"]),
            Some(&["PR checks failing"]),
            Some(&["PR merged"]),
        ];
        for (i, expected) in (0u32..).zip(expected) {
            poller.poll(&state, t0 + INTERVAL * i);
            let expected = expected.map(|reasons| reasons.iter().map(|&r| r.to_owned()).collect());
            assert_eq!(wakes(&mut state, "fix-login-07"), expected, "poll {i}");
        }
        assert!(fake.done());
        // The unchanged third poll appended nothing.
        assert_eq!(repo.pr_events("fix-login-07").len(), 5);

        // A merged PR isn't polled again.
        assert!(!poller.poll(&state, t0 + INTERVAL * 10));
        assert_eq!(fake.calls().len(), 6);
    }

    #[test]
    fn wakes_when_closed_without_merging() {
        let repo = Repo::new();
        repo.task("fix-login-07", Some(URL));
        let mut state = repo.state();
        let fake = Fake::default()
            .ok(&["gh", "pr", "view", URL], &view("OPEN", false, "", PASS))
            .ok(&["gh", "pr", "view", URL], &view("CLOSED", false, "", PASS));
        let tools = Tools::default();
        let mut poller = repo.poller(&fake, &tools);
        let t0 = Instant::now();
        assert!(poller.poll(&state, t0));
        assert_eq!(wakes(&mut state, "fix-login-07"), None);
        assert!(poller.poll(&state, t0 + INTERVAL));
        assert_eq!(
            wakes(&mut state, "fix-login-07"),
            Some(vec!["PR closed without merging".to_owned()])
        );
    }

    #[test]
    fn unchanged_polls_append_nothing() {
        let repo = Repo::new();
        repo.task("fix-login-07", Some(URL));
        let mut state = repo.state();
        let open = view("OPEN", false, "REVIEW_REQUIRED", PASS);
        let fake = Fake::default()
            .ok(&["gh", "pr", "view", URL], &open)
            .ok(&["gh", "pr", "view", URL], &open)
            .ok(&["gh", "pr", "view", URL], &open);
        let tools = Tools::default();
        let mut poller = repo.poller(&fake, &tools);
        let t0 = Instant::now();
        assert!(poller.poll(&state, t0));
        assert_eq!(wakes(&mut state, "fix-login-07"), None);
        for i in 1..3 {
            assert!(!poller.poll(&state, t0 + INTERVAL * i));
            assert_eq!(wakes(&mut state, "fix-login-07"), None);
        }
        assert!(fake.done());
        assert_eq!(
            repo.pr_events("fix-login-07"),
            [EventPayload::Pr {
                state: PrState::Open,
                checks: Some(CheckState::Pass),
                review: Some(ReviewDecision::ReviewRequired),
            }]
        );
    }

    #[test]
    fn polls_only_when_due() {
        let repo = Repo::new();
        repo.task("fix-login-07", Some(URL));
        let state = repo.state();
        let open = view("OPEN", false, "", PASS);
        let fake = Fake::default()
            .ok(&["gh", "pr", "view", URL], &open)
            .ok(&["gh", "pr", "view", URL], &open);
        let tools = Tools::default();
        let mut poller = repo.poller(&fake, &tools);
        let t0 = Instant::now();
        poller.poll(&state, t0);
        poller.poll(&state, t0 + Duration::from_secs(1));
        poller.poll(&state, t0 + Duration::from_secs(119));
        assert_eq!(fake.calls().len(), 1);
        poller.poll(&state, t0 + INTERVAL);
        assert!(fake.done());
    }

    #[test]
    fn backs_off_after_errors() {
        let repo = Repo::new();
        repo.task("fix-login-07", Some(URL));
        let mut state = repo.state();
        let mut fake = Fake::default();
        for _ in 0..5 {
            fake = fake.fail(&["gh", "pr", "view", URL], 1, "HTTP 502: Bad Gateway");
        }
        let fake = fake
            .ok(&["gh", "pr", "view", URL], "not json")
            .ok(&["gh", "pr", "view", URL], &view("OPEN", false, "", PASS))
            .ok(&["gh", "pr", "view", URL], &view("OPEN", false, "", PASS));
        let tools = Tools::default();
        let mut poller = repo.poller(&fake, &tools);
        let min = Duration::from_secs(60);

        // Each failure doubles the wait, up to 15 minutes: 4m, 8m, 15m, 15m, 15m.
        let mut t = Instant::now();
        poller.poll(&state, t);
        for (calls, wait) in (2..).zip([4, 8, 15, 15, 15]) {
            let due = t + wait * min;
            poller.poll(
                &state,
                t + (wait * min).saturating_sub(Duration::from_secs(1)),
            );
            assert_eq!(fake.calls().len(), calls - 1, "polled early");
            poller.poll(&state, due);
            assert_eq!(fake.calls().len(), calls);
            t = due;
        }
        assert_eq!(repo.pr_events("fix-login-07").len(), 0);
        assert_eq!(wakes(&mut state, "fix-login-07"), None);

        // The 7th call succeeded, so the next is one interval later.
        let t = t + 15 * min;
        poller.poll(&state, t);
        assert_eq!(fake.calls().len(), 7);
        assert_eq!(repo.pr_events("fix-login-07").len(), 1);
        poller.poll(&state, t + INTERVAL);
        assert!(fake.done());

        let log = repo.watch_log();
        assert_eq!(log.lines().count(), 6, "{log}");
        assert!(
            log.contains(
                "fix-login-07: https://github.com/erd/web/pull/42: `gh pr view` failed (exit 1): \
                 HTTP 502: Bad Gateway; retrying in 4m"
            ),
            "{log}"
        );
        assert!(log.contains("unexpected `gh pr view` output"), "{log}");
    }

    /// A runner that can't start any command, as when `gh` isn't installed.
    #[derive(Default)]
    struct Missing {
        calls: Cell<usize>,
    }

    impl Runner for Missing {
        fn run(&self, cmd: &Cmd) -> anyhow::Result<RunOutput> {
            self.calls.set(self.calls.get() + 1);
            Err(anyhow!("cannot run `{}`", cmd.program))
        }
    }

    #[test]
    fn missing_gh_disables_polling() {
        let repo = Repo::new();
        repo.task("fix-login-07", Some(URL));
        repo.task("add-docs-02", Some("https://github.com/erd/web/pull/43"));
        let state = repo.state();
        let missing = Missing::default();
        let tools = Tools::default();
        let mut poller = repo.poller(&missing, &tools);
        let t0 = Instant::now();
        assert!(!poller.poll(&state, t0));
        assert!(!poller.poll(&state, t0 + INTERVAL * 10));
        assert_eq!(missing.calls.get(), 1);
        assert!(
            repo.watch_log()
                .contains("cannot run `gh`; PR polling disabled")
        );
    }

    #[test]
    fn polls_only_tasks_with_a_pr_url() {
        let repo = Repo::new();
        repo.task("add-docs-02", None)
            .task("fix-login-07", Some(URL))
            .task("pick-db-03", None);
        repo.append(
            "pick-db-03",
            r#"{"kind":"status","verb":"done","pr_url":"https://github.com/erd/web/pull/44"}"#,
        );
        // A task with no task.json yet but a reported PR.
        std::fs::create_dir_all(repo.task_dir("rate-limit-08")).unwrap();
        let state = repo.state();
        let open = view("OPEN", false, "", PASS);
        let fake = Fake::default().ok(&["gh", "pr", "view", URL], &open).ok(
            &["gh", "pr", "view", "https://github.com/erd/web/pull/44"],
            &open,
        );
        let tools = Tools::default();
        let mut poller = repo.poller(&fake, &tools);
        assert!(poller.poll(&state, Instant::now()));
        assert!(fake.done());
        assert_eq!(repo.pr_events("pick-db-03").len(), 1);
        assert_eq!(repo.pr_events("add-docs-02").len(), 0);
    }

    #[test]
    fn backoff_doubles_up_to_fifteen_minutes() {
        let min = Duration::from_secs(60);
        let waits: Vec<Duration> = (1..=6).map(|n| backoff(2 * min, n)).collect();
        assert_eq!(waits, [4, 8, 15, 15, 15, 15].map(|m| m * min));
        assert_eq!(backoff(30 * min, 1), 30 * min);
        assert_eq!(backoff(2 * min, u32::MAX), 15 * min);
    }
}
