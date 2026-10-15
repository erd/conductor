use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use conductor_state::{
    Phase, RepoRecord, SchemaV1, Task, TaskKind, read_repo_record, repo_record_path, resolve_repo,
    write_task_atomic,
};
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

    /// `conductor <args>` run in `dir` with this fixture's home and no herdr or conductor
    /// session variables, which are set when the tests run inside a conductor worker.
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_conductor"));
        command
            .args(args)
            .current_dir(dir)
            .env("CONDUCTOR_HOME", self.home())
            .env_remove("CONDUCTOR_TASK_ID")
            .env_remove("CONDUCTOR_ROLE")
            .env_remove("CONDUCTOR_REPO_ID")
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
            herdr_pane_id: None,
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
            "cd {}\nCONDUCTOR_ROLE=conductor CONDUCTOR_REPO_ID={} CONDUCTOR_HOME={} claude \
             -n conductor-my-repo --permission-mode auto /conductor:conductor\n",
            root.display(),
            conductor_state::repo_id_for_root(&root),
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
fn report_done_pr_shows_in_status_json() {
    let fx = Fixture::new();
    let task_dir = fx.repo_dir().join("tasks/fix-login-07");
    std::fs::create_dir_all(&task_dir).unwrap();
    let ts = "2026-10-14T12:00:00Z".parse().unwrap();
    let task = Task {
        schema: SchemaV1,
        id: "fix-login-07".into(),
        title: "Fix login".into(),
        kind: TaskKind::Ship,
        phase: Phase::Working,
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
    write_task_atomic(&task_dir, &task).unwrap();
    let url = "https://github.com/erd/my-repo/pull/42";

    // A worker in its worktree, with only CONDUCTOR_TASK_ID: the repo comes from the cwd.
    let output = fx
        .command(&fx.worktree(), &["report", "done", "--pr", url])
        .env("CONDUCTOR_TASK_ID", "fix-login-07")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(stdout(&output), "");
    // task.json is left alone; the URL lives in the event.
    let text = std::fs::read_to_string(task_dir.join("task.json")).unwrap();
    assert!(!text.contains(url), "{text}");

    let output = fx.run(&fx.repo(), &["status", "--json"]);
    assert_success(&output);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["pr_url"], url);
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
    for name in ["spawn", "teardown", "watch", "resume"] {
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
    for args in [
        &["hook", "pre-tool-use"][..],
        &["hook", "pre_tool_use"],
        &["hook", "no-such-event", "--extra"],
        &["hook"],
    ] {
        let output = fx.run(fx.tmp.path(), args);
        assert_success(&output);
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{args:?}"
        );
    }
}

#[test]
fn conductor_session_start_records_pane_and_workspace() {
    let fx = Fixture::new();
    assert_success(&fx.run(&fx.repo(), &["init"]));
    let repo_id = resolve_repo(&fx.repo()).unwrap().id;
    let hook = |event: &str| {
        let output = fx
            .command(fx.tmp.path(), &["hook", event])
            .env("CONDUCTOR_ROLE", "conductor")
            .env("CONDUCTOR_REPO_ID", &repo_id)
            .env("HERDR_PANE_ID", "p1")
            .env("HERDR_WORKSPACE_ID", "w1")
            .output()
            .unwrap();
        assert_success(&output);
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    };
    let record = || read_repo_record(&fx.repo_dir()).unwrap().unwrap();

    // Only SessionStart touches repo.json in the conductor session.
    hook("pre-tool-use");
    assert_eq!(record().herdr_pane_id, None);

    hook("session-start");
    assert_eq!(record().herdr_pane_id.as_deref(), Some("p1"));
    assert_eq!(record().herdr_workspace_id.as_deref(), Some("w1"));
    assert!(!fx.home().join("logs").exists());

    // `init` from a plain terminal keeps both.
    assert_success(&fx.run(&fx.repo(), &["init"]));
    assert_eq!(record().herdr_pane_id.as_deref(), Some("p1"));
    assert_eq!(record().herdr_workspace_id.as_deref(), Some("w1"));
}

#[test]
fn conductor_session_start_without_repo_json_logs() {
    let fx = Fixture::new();
    let output = fx
        .command(fx.tmp.path(), &["hook", "session-start"])
        .env("CONDUCTOR_ROLE", "conductor")
        .env("CONDUCTOR_REPO_ID", "missing-000000")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let log = std::fs::read_to_string(fx.home().join("logs/hook.log")).unwrap();
    assert!(log.contains("conductor init"), "{log}");
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

impl Fixture {
    /// `conductor task new` from the main checkout; returns the new task's id.
    fn new_task(&self, title: &str, kind: &str) -> String {
        let output = self.run(
            &self.repo(),
            &["task", "new", "--title", title, "--kind", kind],
        );
        assert_success(&output);
        let out = stdout(&output);
        let mut lines = out.lines();
        let id = lines.next().unwrap().to_owned();
        let brief = lines.next().unwrap();
        assert_eq!(lines.next(), None);
        let task_dir = self.repo_dir().join("tasks").join(&id);
        assert_eq!(Path::new(brief), task_dir.join("brief.md"));
        id
    }

    fn task(&self, id: &str) -> Task {
        conductor_state::read_task(&self.repo_dir().join("tasks").join(id)).unwrap()
    }

    fn phase_events(&self, id: &str) -> Vec<(Phase, Phase)> {
        let events = self.repo_dir().join("tasks").join(id).join("events.jsonl");
        conductor_state::read_events(&events, 0)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| match event.payload {
                conductor_state::EventPayload::Phase { from, to } => Some((from, to)),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn task_new_ids_are_unique_and_count_per_repo() {
    let fx = Fixture::new();
    assert_eq!(fx.new_task("Fix login", "ship"), "fix-login-01");
    assert_eq!(fx.new_task("Add docs!", "scout"), "add-docs-02");
    // The same title still gets a fresh id.
    assert_eq!(fx.new_task("Fix login", "ship"), "fix-login-03");
    // The counter continues past the largest existing one, whatever its slug.
    std::fs::create_dir_all(fx.repo_dir().join("tasks/old-task-07")).unwrap();
    assert_eq!(fx.new_task("Rate limit", "ship"), "rate-limit-08");

    let task = fx.task("add-docs-02");
    assert_eq!(task.phase, Phase::Intake);
    assert_eq!(task.kind, TaskKind::Scout);
    assert_eq!(task.title, "Add docs!");
    assert_eq!(task.branch, None);
    assert_eq!(task.created_at, task.updated_at);
    let brief = std::fs::read_to_string(fx.repo_dir().join("tasks/add-docs-02/brief.md")).unwrap();
    assert!(
        brief.contains("- Branch: `conductor/add-docs-02`"),
        "{brief}"
    );
    assert!(
        brief.contains("- Conductor session: `conductor-my-repo`"),
        "{brief}"
    );
    assert!(
        brief.contains("- Your session: `my-repo-add-docs-02`"),
        "{brief}"
    );
}

#[test]
fn task_new_copies_intent_and_rejects_bad_input() {
    let fx = Fixture::new();
    let intent = "Make login work with `+` in emails.\nThanks!";
    let output = fx.run(
        &fx.worktree(),
        &[
            "task",
            "new",
            "--title",
            "Fix login",
            "--kind",
            "ship",
            "--intent",
            intent,
        ],
    );
    assert_success(&output);
    let brief = std::fs::read_to_string(fx.repo_dir().join("tasks/fix-login-01/brief.md")).unwrap();
    assert!(
        brief.contains(&format!("## Intent (verbatim)\n\n{intent}\n")),
        "{brief}"
    );

    let output = fx.run(
        &fx.repo(),
        &["task", "new", "--title", " ", "--kind", "ship"],
    );
    assert_eq!(stderr(&output), "conductor: the title is empty\n");
    let output = fx.run(
        &fx.repo(),
        &["task", "new", "--title", "x", "--kind", "build"],
    );
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn task_phase_follows_the_lifecycle() {
    let fx = Fixture::new();
    let id = fx.new_task("Fix login", "ship");

    assert_success(&fx.run(&fx.repo(), &["task", "phase", &id, "briefed"]));
    assert_eq!(fx.task(&id).phase, Phase::Briefed);
    assert_eq!(fx.phase_events(&id), [(Phase::Intake, Phase::Briefed)]);

    // An illegal move fails, names the allowed ones, and changes nothing.
    let output = fx.run(&fx.repo(), &["task", "phase", &id, "working"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "conductor: cannot move {id} from briefed to working; allowed next phases: \
             spawning, failed, abandoned\n"
        )
    );
    assert_eq!(fx.task(&id).phase, Phase::Briefed);
    assert_eq!(fx.phase_events(&id).len(), 1);

    // --force makes it anyway, with a warning.
    let output = fx.run(&fx.repo(), &["task", "phase", &id, "working", "--force"]);
    assert_success(&output);
    assert_eq!(
        stderr(&output),
        format!("conductor: warning: forcing {id} from briefed to working\n")
    );
    assert_eq!(fx.task(&id).phase, Phase::Working);
    assert_eq!(fx.phase_events(&id)[1], (Phase::Briefed, Phase::Working));

    let output = fx.run(&fx.repo(), &["task", "phase", &id, "done"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("expected one of: intake, briefed,"));
}

#[test]
fn task_abandon() {
    let fx = Fixture::new();
    let id = fx.new_task("Fix login", "ship");
    // From outside the repo, naming it by id.
    let repo_id = resolve_repo(&fx.repo()).unwrap().id;
    let output = fx.run(fx.tmp.path(), &["task", "abandon", &id, "--repo", &repo_id]);
    assert_success(&output);
    assert_eq!(fx.task(&id).phase, Phase::Abandoned);
    assert_eq!(fx.phase_events(&id), [(Phase::Intake, Phase::Abandoned)]);

    let output = fx.run(&fx.repo(), &["task", "abandon", &id]);
    assert_eq!(
        stderr(&output),
        format!(
            "conductor: cannot move {id} from abandoned to abandoned; allowed next phases: \
             torn_down\n"
        )
    );

    assert_success(&fx.run(&fx.repo(), &["task", "phase", &id, "torn_down"]));
    let output = fx.run(&fx.repo(), &["task", "phase", &id, "intake"]);
    assert_eq!(
        stderr(&output),
        format!("conductor: cannot move {id} from torn_down to intake: nothing leaves torn_down\n")
    );
}

#[test]
fn task_show_and_list() {
    let fx = Fixture::new();
    let id = fx.new_task("Fix login", "ship");

    let output = fx.run(&fx.repo(), &["task", "show", &id, "--json"]);
    assert_success(&output);
    let shown: Task = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown, fx.task(&id));

    let output = fx.run(&fx.repo(), &["task", "show", &id]);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.starts_with(&format!("id       {id}\n")), "{text}");
    assert!(text.contains("phase    intake\n"), "{text}");
    assert!(!text.contains("branch"), "{text}");

    let list = fx.run(&fx.repo(), &["task", "list", "--json"]);
    assert_success(&list);
    let status = fx.run(&fx.repo(), &["status", "--json"]);
    assert_eq!(stdout(&list), stdout(&status));

    let output = fx.run(&fx.repo(), &["task", "show", "nope-01"]);
    let repo_id = resolve_repo(&fx.repo()).unwrap().id;
    assert_eq!(
        stderr(&output),
        format!("conductor: no task `nope-01` in repo `{repo_id}`\n")
    );
    let output = fx.run(&fx.repo(), &["task", "show", "../x"]);
    assert_eq!(stderr(&output), "conductor: invalid task id \"../x\"\n");
}
