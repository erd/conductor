//! `conductor watch`, run in a temp `CONDUCTOR_HOME` from outside any git repo with `--repo`.
//! Each test starts a watcher, waits for it to write `watch.cursor` (its first scan is done),
//! appends events, and checks how and why it exits.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const REPO_ID: &str = "my-repo-abc123";
const TASK_ID: &str = "fix-login-07";

/// How long a watcher may take to start or to notice an event.
const DEADLINE: Duration = Duration::from_secs(10);

struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    /// A conductor home with one task dir, `TASK_ID`, without a task.json.
    fn new() -> Self {
        let fixture = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(fixture.task_dir(TASK_ID)).unwrap();
        fixture
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn repo_dir(&self) -> PathBuf {
        self.home().join("repos").join(REPO_ID)
    }

    fn task_dir(&self, task: &str) -> PathBuf {
        self.repo_dir().join("tasks").join(task)
    }

    fn cursor_path(&self) -> PathBuf {
        self.repo_dir().join("watch.cursor")
    }

    /// Appends events, given as JSON objects without `ts` and `task`, to `task`'s log.
    fn append(&self, task: &str, events: &[&str]) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.task_dir(task).join("events.jsonl"))
            .unwrap();
        for event in events {
            let mut value: serde_json::Value = serde_json::from_str(event).unwrap();
            value["ts"] = "2026-10-14T12:00:00Z".into();
            value["task"] = task.into();
            writeln!(file, "{value}").unwrap();
        }
    }

    fn set_phase(&self, task: &str, phase: &str) {
        let task_json = serde_json::json!({
            "schema": "conductor-state.v1",
            "id": task,
            "title": "A task",
            "kind": "ship",
            "phase": phase,
            "created_at": "2026-10-14T08:00:00Z",
            "updated_at": "2026-10-14T08:00:00Z",
        });
        std::fs::write(self.task_dir(task).join("task.json"), task_json.to_string()).unwrap();
    }

    fn command(&self, max_wait: &str) -> Command {
        self.command_for(REPO_ID, max_wait)
    }

    fn command_for(&self, repo: &str, max_wait: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_conductor"));
        command
            .args(["watch", "--repo", repo, "--max-wait", max_wait])
            .current_dir(self.tmp.path())
            .env("CONDUCTOR_HOME", self.home())
            .env_remove("CONDUCTOR_TASK_ID")
            .env_remove("CONDUCTOR_ROLE")
            .env_remove("CONDUCTOR_REPO_ID")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Starts a watcher and waits until its first scan is done.
    fn start(&self, max_wait: &str) -> Child {
        let _ = std::fs::remove_file(self.cursor_path());
        let mut child = self.command(max_wait).spawn().unwrap();
        let deadline = Instant::now() + DEADLINE;
        while !self.cursor_path().exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("watch exited early with {status}");
            }
            assert!(Instant::now() < deadline, "watch never wrote its cursor");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    /// Starts a watcher that already has a cursor file, so it can't signal it's ready by
    /// writing one; it reads only events appended after its cursor anyway.
    fn restart(&self, max_wait: &str) -> Child {
        assert!(self.cursor_path().exists());
        self.command(max_wait).spawn().unwrap()
    }
}

/// Waits for `child` to exit, killing it and failing after `DEADLINE`.
fn finish(mut child: Child) -> Output {
    let deadline = Instant::now() + DEADLINE;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("watch didn't exit: {:?}", child.wait_with_output().unwrap());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn assert_wakes(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(stderr(output), expected);
}

#[track_caller]
fn assert_times_out(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(stderr(output), "");
}

#[test]
fn permission_prompt_wakes_and_names_the_tool() {
    let fx = Fixture::new();
    let watch = fx.start("1m");
    fx.append(
        TASK_ID,
        &[
            r#"{"kind":"tool_start","tool_name":"Bash","summary":"rm -rf build","tool_use_id":"tu1"}"#,
            r#"{"kind":"attention","type":"permission_prompt","message":"Claude needs your permission to use Bash"}"#,
        ],
    );
    assert_wakes(
        &finish(watch),
        "fix-login-07: needs permission (Bash: rm -rf build)\n",
    );
}

#[test]
fn each_wake_condition_wakes() {
    let cases = [
        (
            r#"{"kind":"decision_open","key":"db-choice"}"#,
            "fix-login-07: opened decision `db-choice`\n",
        ),
        (
            r#"{"kind":"session_end","reason":"logout"}"#,
            "fix-login-07: session ended (logout)\n",
        ),
        (
            r#"{"kind":"turn_end"}"#,
            "fix-login-07: went idle without reporting\n",
        ),
        (
            r#"{"kind":"attention","type":"elicitation_dialog","message":"Pick one"}"#,
            "fix-login-07: needs attention (Pick one)\n",
        ),
    ];
    for (event, expected) in cases {
        let fx = Fixture::new();
        let watch = fx.start("1m");
        fx.append(TASK_ID, &[event]);
        assert_wakes(&finish(watch), expected);
    }
}

#[test]
fn noise_and_reported_turns_do_not_wake() {
    let fx = Fixture::new();
    let watch = fx.start("2s");
    fx.append(
        TASK_ID,
        &[
            r#"{"kind":"prompt","text":"go"}"#,
            r#"{"kind":"tool_start","tool_name":"Read","summary":"src/login.rs"}"#,
            r#"{"kind":"tool_end","tool_name":"Read","summary":"src/login.rs","ok":true}"#,
            r#"{"kind":"status","verb":"done","text":"Draft PR open"}"#,
            r#"{"kind":"turn_end"}"#,
            r#"{"kind":"attention","type":"idle_prompt","message":"Claude is waiting for your input"}"#,
        ],
    );
    assert_times_out(&finish(watch));
}

#[test]
fn history_before_the_first_run_does_not_wake() {
    let fx = Fixture::new();
    fx.append(
        TASK_ID,
        &[
            r#"{"kind":"decision_open","key":"db-choice"}"#,
            r#"{"kind":"turn_end"}"#,
        ],
    );
    assert_times_out(&finish(fx.start("1s")));
}

#[test]
fn history_before_the_first_run_still_counts_toward_turn_end() {
    let fx = Fixture::new();
    fx.append(TASK_ID, &[r#"{"kind":"status","verb":"working"}"#]);
    let watch = fx.start("2s");
    fx.append(TASK_ID, &[r#"{"kind":"turn_end"}"#]);
    assert_times_out(&finish(watch));
}

#[test]
fn a_restarted_watcher_does_not_wake_on_the_same_events() {
    let fx = Fixture::new();
    let watch = fx.start("1m");
    fx.append(TASK_ID, &[r#"{"kind":"session_end","reason":"logout"}"#]);
    assert_wakes(&finish(watch), "fix-login-07: session ended (logout)\n");

    assert_times_out(&finish(fx.restart("1s")));
}

#[test]
fn a_prompt_while_a_decision_is_open_wakes_the_next_watcher() {
    let fx = Fixture::new();
    let watch = fx.start("1m");
    fx.append(
        TASK_ID,
        &[r#"{"kind":"decision_open","key":"approve-plan"}"#],
    );
    assert_wakes(
        &finish(watch),
        "fix-login-07: opened decision `approve-plan`\n",
    );

    // The user answers in the worker's pane; nothing records the answer.
    let watch = fx.restart("1m");
    fx.append(TASK_ID, &[r#"{"kind":"prompt","text":"Plan is approved"}"#]);
    assert_wakes(
        &finish(watch),
        "fix-login-07: got a prompt while decision `approve-plan` is open\n",
    );
}

#[test]
fn a_restarted_watcher_wakes_on_events_since_it_last_ran() {
    let fx = Fixture::new();
    assert_times_out(&finish(fx.start("1s")));
    fx.append(TASK_ID, &[r#"{"kind":"decision_open","key":"db-choice"}"#]);
    assert_wakes(
        &finish(fx.restart("1m")),
        "fix-login-07: opened decision `db-choice`\n",
    );
}

#[test]
fn a_second_instance_exits_at_once() {
    let fx = Fixture::new();
    let mut first = fx.start("1m");
    let second = finish(fx.command("1m").spawn().unwrap());
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    assert_eq!(stderr(&second), "");

    // The first one is still watching.
    assert!(first.try_wait().unwrap().is_none());
    fx.append(TASK_ID, &[r#"{"kind":"turn_end"}"#]);
    assert_wakes(
        &finish(first),
        "fix-login-07: went idle without reporting\n",
    );
}

#[test]
fn picks_up_a_task_created_while_running() {
    let fx = Fixture::new();
    let watch = fx.start("1m");
    std::fs::create_dir_all(fx.task_dir("add-docs-02")).unwrap();
    fx.append(
        "add-docs-02",
        &[
            r#"{"kind":"session_start","source":"startup"}"#,
            r#"{"kind":"attention","type":"permission_prompt","message":"Claude needs your permission to use WebFetch"}"#,
        ],
    );
    assert_wakes(&finish(watch), "add-docs-02: needs permission\n");
}

#[test]
fn reports_every_waking_task_on_its_own_line() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.task_dir("add-docs-02")).unwrap();
    assert_times_out(&finish(fx.start("1s")));
    // Appended while no watcher runs, so the next one reads them all in its first scan.
    fx.append(
        TASK_ID,
        &[
            r#"{"kind":"turn_end"}"#,
            r#"{"kind":"decision_open","key":"db-choice"}"#,
        ],
    );
    fx.append(
        "add-docs-02",
        &[r#"{"kind":"session_end","reason":"other"}"#],
    );
    assert_wakes(
        &finish(fx.restart("1m")),
        "add-docs-02: session ended (other)\n\
         fix-login-07: went idle without reporting; opened decision `db-choice`\n",
    );
}

#[test]
fn ignores_torn_down_and_abandoned_tasks() {
    let fx = Fixture::new();
    fx.set_phase(TASK_ID, "torn_down");
    std::fs::create_dir_all(fx.task_dir("add-docs-02")).unwrap();
    fx.set_phase("add-docs-02", "abandoned");
    let watch = fx.start("2s");
    fx.append(TASK_ID, &[r#"{"kind":"session_end","reason":"other"}"#]);
    fx.append(
        "add-docs-02",
        &[r#"{"kind":"session_end","reason":"other"}"#],
    );
    assert_times_out(&finish(watch));
}

#[test]
fn polls_the_pr_and_wakes_when_merged() {
    use std::os::unix::fs::PermissionsExt as _;

    let fx = Fixture::new();
    let url = "https://github.com/erd/web/pull/42";
    fx.set_phase(TASK_ID, "awaiting_merge");
    let task_json = fx.task_dir(TASK_ID).join("task.json");
    let mut task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_json).unwrap()).unwrap();
    task["pr_url"] = url.into();
    std::fs::write(&task_json, task.to_string()).unwrap();

    // A `gh` that reports the PR merged.
    let bin = fx.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\n\
         echo '{\"isDraft\":false,\"reviewDecision\":\"APPROVED\",\"state\":\"MERGED\",\
         \"statusCheckRollup\":[]}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = finish(fx.command("1m").env("PATH", path).spawn().unwrap());
    assert_wakes(&output, "fix-login-07: PR merged\n");

    let events = std::fs::read_to_string(fx.task_dir(TASK_ID).join("events.jsonl")).unwrap();
    let event: serde_json::Value = serde_json::from_str(events.trim()).unwrap();
    assert_eq!(event["kind"], "pr");
    assert_eq!(event["state"], "merged");
    assert_eq!(event["review"], "approved");
}

#[test]
fn unknown_repo_fails() {
    let fx = Fixture::new();
    let output = fx.command_for("nope-000000", "1s").output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr(&output).contains("no repo `nope-000000`"),
        "{output:?}"
    );
}
