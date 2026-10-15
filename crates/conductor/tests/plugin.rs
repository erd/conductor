//! The Claude Code plugin: `hooks/hooks.json`, the `hooks/conductor-hook.sh` shim it runs, and
//! the marketplace and plugin manifests.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const REPO_ID: &str = "my-repo-abc123";
const TASK_ID: &str = "fix-login-07";

/// How long a watcher may take to start or to notice an event.
const DEADLINE: Duration = Duration::from_secs(10);

const SHIM: &str = "${CLAUDE_PLUGIN_ROOT}/hooks/conductor-hook.sh";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_json(path: &str) -> Value {
    let text = std::fs::read_to_string(repo_root().join(path)).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{path}: {err}"))
}

/// A temp dir holding a conductor home with one task dir, and an empty `HOME`.
struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(fixture.task_dir()).unwrap();
        std::fs::create_dir_all(fixture.user_home()).unwrap();
        fixture
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("conductor")
    }

    fn user_home(&self) -> PathBuf {
        self.tmp.path().join("user")
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

    /// Every file under the conductor home.
    fn files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.home(), &mut files);
        files
    }

    /// `sh conductor-hook.sh <arg>` with the conductor binary as `CONDUCTOR_BIN`, this
    /// fixture's homes, and no conductor or herdr session variables.
    fn shim(&self, arg: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .arg(repo_root().join("hooks/conductor-hook.sh"))
            .arg(arg)
            .current_dir(self.tmp.path())
            .env("CONDUCTOR_BIN", env!("CARGO_BIN_EXE_conductor"))
            .env("CONDUCTOR_HOME", self.home())
            .env("HOME", self.user_home())
            .env_remove("CONDUCTOR_TASK_ID")
            .env_remove("CONDUCTOR_ROLE")
            .env_remove("CONDUCTOR_REPO_ID")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_WORKSPACE_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

/// Runs `command` with `stdin` piped in, tolerating a child that doesn't read it.
fn run_with_stdin(command: &mut Command, stdin: &str) -> Output {
    let mut child = command.spawn().unwrap();
    // The child may exit without reading stdin (the shim when the binary is missing, the hook's
    // fast path), closing the pipe before or during the write.
    match child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
        result => result.unwrap(),
    }
    child.wait_with_output().unwrap()
}

#[track_caller]
fn assert_silent_success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

const PRE_TOOL_USE: &str = r#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"},"tool_use_id":"tu1"}"#;

#[test]
fn hooks_json_runs_the_shim_for_every_event() {
    let hooks = read_json("hooks/hooks.json");
    let hooks = hooks["hooks"].as_object().unwrap();
    let events = [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "user-prompt-submit"),
        ("PreToolUse", "pre-tool-use"),
        ("PostToolUse", "post-tool-use"),
        ("PostToolUseFailure", "post-tool-use-failure"),
        ("Notification", "notification"),
        ("Stop", "stop"),
        ("StopFailure", "stop-failure"),
        ("SessionEnd", "session-end"),
    ];
    assert_eq!(hooks.len(), events.len(), "{:?}", hooks.keys());
    for (name, arg) in events {
        let first = &hooks[name][0]["hooks"][0];
        assert_eq!(first["type"], "command", "{name}");
        assert_eq!(first["command"], format!(r#"sh "{SHIM}" {arg}"#), "{name}");
        assert_eq!(first.get("async"), None, "{name}");
    }

    let stop = hooks["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    let watch = &stop[1]["hooks"][0];
    assert_eq!(watch["command"], format!(r#"sh "{SHIM}" watch"#));
    assert_eq!(watch["async"], true);
    assert_eq!(watch["asyncRewake"], true);
    // 8 h, `conductor watch`'s default `--max-wait`.
    assert_eq!(watch["timeout"], 8 * 60 * 60);
}

#[test]
fn marketplace_lists_the_plugin_from_the_repo_root() {
    let plugin = read_json(".claude-plugin/plugin.json");
    let marketplace = read_json(".claude-plugin/marketplace.json");
    assert_eq!(plugin["name"], "conductor");
    assert_eq!(marketplace["name"], "conductor");
    let entries = marketplace["plugins"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], plugin["name"]);
    assert_eq!(entries[0]["source"], "./");
}

/// The `key: value` lines between a skill's opening and closing `---`.
fn frontmatter(path: &str) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(repo_root().join(path)).unwrap();
    let rest = text
        .strip_prefix("---\n")
        .unwrap_or_else(|| panic!("{path}: no frontmatter"));
    let (block, _) = rest
        .split_once("\n---\n")
        .unwrap_or_else(|| panic!("{path}: unterminated frontmatter"));
    block
        .lines()
        .map(|line| {
            let (key, value) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("{path}: not `key: value`: {line}"));
            (key.to_owned(), value.trim().to_owned())
        })
        .collect()
}

/// Every skill is named after its directory, so it loads as `/conductor:<dir>`. Only the
/// conductor skill, which `conductor start` invokes, is hidden from the model.
#[test]
fn skills_have_frontmatter() {
    for (skill, manual_only) in [("conductor", true), ("worker", false), ("status", false)] {
        let path = format!("skills/{skill}/SKILL.md");
        let fields = frontmatter(&path);
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("name"), Some(skill), "{path}");
        assert!(!get("description").unwrap_or_default().is_empty(), "{path}");
        assert_eq!(
            get("disable-model-invocation"),
            manual_only.then_some("true"),
            "{path}"
        );
    }
}

#[test]
fn shim_records_a_worker_event() {
    let fx = Fixture::new();
    let output = run_with_stdin(
        fx.shim("pre-tool-use")
            .env("CONDUCTOR_TASK_ID", TASK_ID)
            .env("CONDUCTOR_REPO_ID", REPO_ID),
        PRE_TOOL_USE,
    );
    assert_silent_success(&output);
    let events = std::fs::read_to_string(fx.events()).unwrap();
    let event: Value = serde_json::from_str(events.trim()).unwrap();
    assert_eq!(event["kind"], "tool_start");
    assert_eq!(event["summary"], "ls");
}

#[test]
fn shim_defaults_to_the_cargo_bin_and_exits_0_without_it() {
    let fx = Fixture::new();
    for command in [
        fx.shim("pre-tool-use").env_remove("CONDUCTOR_BIN"),
        fx.shim("pre-tool-use")
            .env("CONDUCTOR_BIN", fx.tmp.path().join("missing")),
    ] {
        let output = run_with_stdin(
            command
                .env("CONDUCTOR_TASK_ID", TASK_ID)
                .env("CONDUCTOR_REPO_ID", REPO_ID),
            PRE_TOOL_USE,
        );
        assert_silent_success(&output);
        assert!(fx.files().is_empty(), "{:?}", fx.files());
    }
}

/// A session outside conductor never writes to the conductor home, from any hook.
#[test]
fn shim_outside_conductor_writes_nothing() {
    let fx = Fixture::new();
    for arg in [
        "session-start",
        "pre-tool-use",
        "stop",
        "session-end",
        "watch",
    ] {
        let started = Instant::now();
        assert_silent_success(&run_with_stdin(&mut fx.shim(arg), PRE_TOOL_USE));
        assert!(
            started.elapsed() < DEADLINE,
            "{arg} took {:?}",
            started.elapsed()
        );
    }
    assert!(fx.files().is_empty(), "{:?}", fx.files());
}

/// A worker session's Stop doesn't start a watcher either.
#[test]
fn shim_watch_needs_the_conductor_role() {
    let fx = Fixture::new();
    for role in [None, Some("worker")] {
        let mut command = fx.shim("watch");
        command
            .env("CONDUCTOR_TASK_ID", TASK_ID)
            .env("CONDUCTOR_REPO_ID", REPO_ID);
        if let Some(role) = role {
            command.env("CONDUCTOR_ROLE", role);
        }
        assert_silent_success(&run_with_stdin(&mut command, "{}"));
    }
    assert!(
        !fx.home()
            .join("repos")
            .join(REPO_ID)
            .join("watch.cursor")
            .exists()
    );
}

#[test]
fn shim_watch_in_the_conductor_wakes_on_attention() {
    let fx = Fixture::new();
    let cursor = fx.home().join("repos").join(REPO_ID).join("watch.cursor");
    let mut child = fx
        .shim("watch")
        .env("CONDUCTOR_ROLE", "conductor")
        .env("CONDUCTOR_REPO_ID", REPO_ID)
        .spawn()
        .unwrap();
    drop(child.stdin.take());

    let deadline = Instant::now() + DEADLINE;
    while !cursor.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("watch exited early with {status}");
        }
        assert!(Instant::now() < deadline, "watch never wrote its cursor");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut events = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(fx.events())
        .unwrap();
    writeln!(
        events,
        r#"{{"ts":"2026-10-15T12:00:00Z","task":"{TASK_ID}","kind":"attention","type":"elicitation_dialog","message":"Pick one"}}"#
    )
    .unwrap();

    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("watch didn't wake");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "fix-login-07: needs attention (Pick one)\n"
    );
}
