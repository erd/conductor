//! `conductor hook <event>`. The conductor-session cases, which need a git repo, are in `cli.rs`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const REPO_ID: &str = "my-repo-abc123";
const TASK_ID: &str = "fix-login-07";

/// A conductor home holding one task dir.
struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(fixture.task_dir()).unwrap();
        fixture
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn task_dir(&self) -> PathBuf {
        self.home()
            .join("repos")
            .join(REPO_ID)
            .join("tasks")
            .join(TASK_ID)
    }

    fn events(&self) -> PathBuf {
        self.task_dir().join("events.jsonl")
    }

    fn log(&self) -> PathBuf {
        self.home().join("logs/hook.log")
    }

    /// `conductor hook <event>` as a worker session for this fixture's task.
    fn worker(&self, event: &str) -> Command {
        let mut command = bare_hook(event);
        command
            .env("CONDUCTOR_HOME", self.home())
            .env("CONDUCTOR_TASK_ID", TASK_ID)
            .env("CONDUCTOR_REPO_ID", REPO_ID)
            .env("HERDR_PANE_ID", "p-1")
            .env("HERDR_WORKSPACE_ID", "w-1");
        command
    }
}

/// `conductor hook <event>` with no conductor or herdr variables, which are set when the tests
/// run inside a conductor worker.
fn bare_hook(event: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_conductor"));
    command
        .args(["hook", event])
        .env_remove("CONDUCTOR_TASK_ID")
        .env_remove("CONDUCTOR_ROLE")
        .env_remove("CONDUCTOR_REPO_ID")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_WORKSPACE_ID");
    command
}

/// Runs `command` with `stdin` piped in, tolerating a child that doesn't read it.
fn run_with_stdin(command: &mut Command, stdin: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The child may exit without reading stdin (the shim when the binary is missing, the hook's
    // fast path), closing the pipe before or during the write.
    match child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
        result => result.unwrap(),
    }
    child.wait_with_output().unwrap()
}

/// Asserts the hook exited 0 without printing anything.
#[track_caller]
fn assert_silent_success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Each `fixtures/hook/<case>.json` holds a hook `event`, the `input` Claude sends on stdin
/// (based on the examples at <https://code.claude.com/docs/en/hooks>), and the `expected` event
/// line minus its `ts`.
#[test]
fn fixtures_produce_expected_events() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hook");
    let mut cases: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    cases.sort();
    assert!(cases.len() >= 9);

    for case in cases {
        let name = case.file_stem().unwrap().to_string_lossy().into_owned();
        let fixture: Value = serde_json::from_str(&std::fs::read_to_string(&case).unwrap())
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        let fx = Fixture::new();
        let output = run_with_stdin(
            &mut fx.worker(fixture["event"].as_str().unwrap()),
            &fixture["input"].to_string(),
        );
        assert_silent_success(&output);
        assert!(
            !fx.log().exists(),
            "{name}: {}",
            read_lines(&fx.log()).join("\n")
        );

        let lines = read_lines(&fx.events());
        assert_eq!(lines.len(), 1, "{name}: {lines:?}");
        let mut event: Value = serde_json::from_str(&lines[0]).unwrap();
        let ts = event.as_object_mut().unwrap().remove("ts").unwrap();
        assert!(
            ts.as_str().unwrap().parse::<jiff::Timestamp>().is_ok(),
            "{name}: {ts}"
        );
        assert_eq!(event, fixture["expected"], "{name}");
    }
}

#[test]
fn fast_path_reads_no_stdin_and_creates_no_files() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = bare_hook("pre-tool-use")
        .env("CONDUCTOR_HOME", tmp.path().join("conductor"))
        .env("HOME", tmp.path().join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep stdin open and empty: a hook that read it would block until the deadline.
    let stdin = child.stdin.take().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the hook blocked reading stdin");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(stdin);

    assert_silent_success(&child.wait_with_output().unwrap());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn malformed_stdin_logs_and_succeeds() {
    let fx = Fixture::new();
    assert_silent_success(&run_with_stdin(&mut fx.worker("pre-tool-use"), "not json"));
    assert!(!fx.events().exists());

    let log = read_lines(&fx.log());
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("hook pre-tool-use"), "{}", log[0]);
    assert!(log[0].contains(&format!("task={TASK_ID}")), "{}", log[0]);
    assert!(log[0].contains("malformed hook input"), "{}", log[0]);
}

#[test]
fn missing_required_field_logs_and_succeeds() {
    let fx = Fixture::new();
    let output = run_with_stdin(&mut fx.worker("session-end"), r#"{"session_id":"s"}"#);
    assert_silent_success(&output);
    assert!(!fx.events().exists());
    assert!(read_lines(&fx.log())[0].contains("no `reason`"));
}

#[test]
fn unknown_task_or_repo_logs_and_succeeds() {
    let input = r#"{"session_id":"s","hook_event_name":"Stop"}"#;

    let fx = Fixture::new();
    let output = run_with_stdin(fx.worker("stop").env("CONDUCTOR_TASK_ID", "nope-01"), input);
    assert_silent_success(&output);
    assert!(read_lines(&fx.log())[0].contains("nope-01"));

    let fx = Fixture::new();
    let output = run_with_stdin(fx.worker("stop").env_remove("CONDUCTOR_REPO_ID"), input);
    assert_silent_success(&output);
    assert!(!fx.events().exists());
    assert!(read_lines(&fx.log())[0].contains("CONDUCTOR_REPO_ID is not set"));
}

#[test]
fn unknown_event_logs_and_succeeds() {
    let fx = Fixture::new();
    let output = run_with_stdin(&mut fx.worker("pre-compact"), "{}");
    assert_silent_success(&output);
    assert!(!fx.events().exists());
    assert!(read_lines(&fx.log())[0].contains(r#"unknown hook event "pre-compact""#));
}

#[test]
fn other_roles_record_nothing() {
    let fx = Fixture::new();
    let output = run_with_stdin(
        bare_hook("session-start")
            .env("CONDUCTOR_HOME", fx.home())
            .env("CONDUCTOR_ROLE", "something-else")
            .env("CONDUCTOR_REPO_ID", REPO_ID),
        r#"{"session_id":"s","source":"startup"}"#,
    );
    assert_silent_success(&output);
    assert!(!fx.events().exists());
    assert!(!fx.log().exists());
}
