use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use conductor_state::{RepoRecord, read_repo_record, repo_record_path, resolve_repo};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git repo `my-repo` with one commit and a worktree at `.worktrees/task`, plus an empty
/// conductor home, both in one temp dir.
struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Self { tmp };
        let repo = fixture.repo();
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            &repo,
            &["worktree", "add", "-q", ".worktrees/task", "-b", "task"],
        );
        std::fs::create_dir_all(fixture.home()).unwrap();
        fixture
    }

    fn repo(&self) -> PathBuf {
        self.tmp.path().join("my-repo")
    }

    fn worktree(&self) -> PathBuf {
        self.repo().join(".worktrees/task")
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn repo_dir(&self) -> PathBuf {
        let id = resolve_repo(&self.repo()).unwrap().id;
        self.home().join("repos").join(id)
    }

    /// `conductor <args>` run in `dir` with this fixture's home and no herdr variables.
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_conductor"));
        command
            .args(args)
            .current_dir(dir)
            .env("CONDUCTOR_HOME", self.home())
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_WORKSPACE_ID");
        command
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn assert_success(output: &Output) {
    assert!(output.status.success(), "stderr: {}", stderr(output));
}

#[test]
fn version_prints_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
        .arg("--version")
        .output()
        .expect("run conductor --version");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("conductor {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn init_same_record_from_main_checkout_and_worktree() {
    let fx = Fixture::new();
    let expected_path = repo_record_path(&fx.repo_dir());

    let from_main = fx.run(&fx.repo(), &["init"]);
    assert_success(&from_main);
    assert_eq!(stdout(&from_main).trim(), expected_path.to_str().unwrap());
    let record = read_repo_record(&fx.repo_dir()).unwrap().unwrap();
    let root = fx.repo().canonicalize().unwrap();
    assert_eq!(
        record,
        RepoRecord {
            schema: conductor_state::SchemaV1,
            repo_id: conductor_state::repo_id_for_root(&root),
            repo_root: root,
            conductor_session: "conductor-my-repo".into(),
            herdr_workspace_id: None,
        }
    );

    let from_worktree = fx.run(&fx.worktree(), &["init"]);
    assert_success(&from_worktree);
    assert_eq!(stdout(&from_worktree), stdout(&from_main));
    assert_eq!(read_repo_record(&fx.repo_dir()).unwrap().unwrap(), record);
}

#[test]
fn init_keeps_herdr_workspace_outside_herdr() {
    let fx = Fixture::new();
    let output = fx
        .command(&fx.repo(), &["init"])
        .env("HERDR_WORKSPACE_ID", "w7")
        .output()
        .unwrap();
    assert_success(&output);
    let workspace = || {
        read_repo_record(&fx.repo_dir())
            .unwrap()
            .unwrap()
            .herdr_workspace_id
    };
    assert_eq!(workspace().as_deref(), Some("w7"));

    // Run again with no herdr in the environment: the recorded workspace stays.
    assert_success(&fx.run(&fx.worktree(), &["init"]));
    assert_eq!(workspace().as_deref(), Some("w7"));
}

#[test]
fn start_dry_run_prints_command_and_writes_nothing() {
    let fx = Fixture::new();
    let output = fx.run(&fx.worktree(), &["start", "--dry-run"]);
    assert_success(&output);

    let root = fx.repo().canonicalize().unwrap();
    assert_eq!(
        stdout(&output),
        format!(
            "cd {}\nCONDUCTOR_ROLE=conductor CONDUCTOR_HOME={} claude -n conductor-my-repo \
             --permission-mode auto /conductor:conductor\n",
            root.display(),
            fx.home().display(),
        )
    );
    assert!(
        stderr(&output).contains("not running inside herdr"),
        "{}",
        stderr(&output)
    );
    assert!(!fx.home().join("repos").exists());
}

#[test]
fn start_dry_run_in_herdr_has_no_warning() {
    let fx = Fixture::new();
    let output = fx
        .command(&fx.repo(), &["start", "--dry-run"])
        .env("HERDR_PANE_ID", "p1")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(stderr(&output), "");
}

#[test]
fn status_with_no_tasks() {
    let fx = Fixture::new();
    let id = resolve_repo(&fx.repo()).unwrap().id;

    let table = fx.run(&fx.repo(), &["status"]);
    assert_success(&table);
    assert_eq!(stdout(&table), format!("no tasks for {id}\n"));

    let json = fx.run(&fx.worktree(), &["status", "--json"]);
    assert_success(&json);
    assert_eq!(stdout(&json), "[]\n");
}

#[test]
fn status_reads_tasks_from_home() {
    let fx = Fixture::new();
    let task_dir = fx.repo_dir().join("tasks/fix-login-07");
    std::fs::create_dir_all(&task_dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/status/tasks/fix-login-07/task.json");
    std::fs::copy(fixture, task_dir.join("task.json")).unwrap();
    // A task dir with no task.json is reported and skipped.
    std::fs::create_dir_all(fx.repo_dir().join("tasks/broken-01")).unwrap();

    let output = fx.run(&fx.repo(), &["status", "--json"]);
    assert_success(&output);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["id"], "fix-login-07");
    assert_eq!(rows[0]["activity"], "not_started");
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert!(
        stderr(&output).contains("skipping task broken-01"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn outside_a_git_repo_fails() {
    let fx = Fixture::new();
    for args in [&["status"][..], &["init"], &["start", "--dry-run"]] {
        let output = fx.run(fx.tmp.path(), args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            stderr(&output).starts_with("conductor: not inside a git repo: "),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn bad_config_fails_and_names_the_file() {
    let fx = Fixture::new();
    let config = fx.home().join("config.toml");
    std::fs::write(&config, "deck_width = \"wide\"\n").unwrap();
    let output = fx.run(&fx.repo(), &["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with(&format!("conductor: {}", config.display())),
        "{}",
        stderr(&output)
    );
}

#[test]
fn stubs_are_not_implemented_yet() {
    let fx = Fixture::new();
    for name in [
        "report", "decision", "task", "spawn", "teardown", "watch", "resume",
    ] {
        let output = fx.run(&fx.repo(), &[name, "some", "--args"]);
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(
            stderr(&output),
            format!("conductor: `conductor {name}` is not implemented yet\n")
        );
    }
}

#[test]
fn hook_never_fails() {
    let fx = Fixture::new();
    // Even outside a repo: the hook shim runs it in every Claude session.
    let output = fx.run(fx.tmp.path(), &["hook", "pre_tool_use"]);
    assert_success(&output);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn every_command_has_help() {
    for name in [
        "init", "start", "status", "hook", "report", "decision", "task", "spawn", "teardown",
        "watch", "resume",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
            .args([name, "--help"])
            .output()
            .unwrap();
        assert_success(&output);
        assert!(stdout(&output).starts_with(char::is_uppercase), "{name}");
        assert!(stdout(&output).contains("Usage: conductor"), "{name}");
    }
}
