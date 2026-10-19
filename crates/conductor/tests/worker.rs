//! `conductor report` and `conductor decision`, run in a temp `CONDUCTOR_HOME` from outside any
//! git repo, so the repo always comes from `CONDUCTOR_REPO_ID` or `--repo`. `report --pr`
//! through `conductor status`, which needs a repo, is in `cli.rs`.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

const REPO_ID: &str = "my-repo-abc123";
const TASK_ID: &str = "fix-login-07";

/// A conductor home holding one task dir, `TASK_ID`, and no task.json, which these commands
/// don't need.
struct Fixture {
    tmp: TempDir,
}

impl Fixture {
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

    fn task_dir(&self, task: &str) -> PathBuf {
        self.home()
            .join("repos")
            .join(REPO_ID)
            .join("tasks")
            .join(task)
    }

    fn decision_file(&self, key: &str) -> Value {
        let path = self.task_dir(TASK_ID).join(format!("decisions/{key}.json"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Writes the task's `task.json` in `phase`.
    fn write_task(&self, phase: &str) {
        let task = json!({
            "schema": "conductor-state.v1",
            "id": TASK_ID,
            "title": "Fix login",
            "kind": "ship",
            "phase": phase,
            "created_at": "2026-10-14T11:56:00Z",
            "updated_at": "2026-10-14T12:15:00Z",
        });
        let path = self.task_dir(TASK_ID).join("task.json");
        std::fs::write(path, serde_json::to_vec_pretty(&task).unwrap()).unwrap();
    }

    /// The phase in the task's `task.json`.
    fn phase(&self) -> Value {
        let path = self.task_dir(TASK_ID).join("task.json");
        let task: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        task["phase"].clone()
    }

    /// Every event in the task's log.
    fn events(&self) -> Vec<Value> {
        let path = self.task_dir(TASK_ID).join("events.jsonl");
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// `conductor <args>` with no conductor or herdr variables, run outside any repo.
    fn bare(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_conductor"));
        command
            .args(args)
            .current_dir(self.tmp.path())
            .env("CONDUCTOR_HOME", self.home())
            .env_remove("CONDUCTOR_TASK_ID")
            .env_remove("CONDUCTOR_ROLE")
            .env_remove("CONDUCTOR_REPO_ID")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_WORKSPACE_ID");
        command
    }

    /// `conductor <args>` as the task's worker.
    fn worker(&self, args: &[&str]) -> Output {
        self.bare(args)
            .env("CONDUCTOR_TASK_ID", TASK_ID)
            .env("CONDUCTOR_REPO_ID", REPO_ID)
            .output()
            .unwrap()
    }

    /// `conductor <args> --repo REPO_ID --task TASK_ID`, as the deck runs it.
    fn outside(&self, args: &[&str]) -> Output {
        self.bare(args)
            .args(["--repo", REPO_ID, "--task", TASK_ID])
            .output()
            .unwrap()
    }

    /// Opens decision `db` with two options, recommending `pg`.
    fn open_db(&self) -> Output {
        self.worker(&[
            "decision",
            "open",
            "db",
            "--title",
            "Which database?",
            "--about",
            "Needs to run in CI",
            "--option",
            "pg=Postgres:the usual",
            "--option",
            "sqlite=SQLite",
            "--recommend",
            "pg",
        ])
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

/// Asserts `output` failed with exit 1 and nothing on stdout, and that stderr contains `text`.
#[track_caller]
fn assert_fails_with(output: &Output, text: &str) {
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(output));
    assert!(output.stdout.is_empty(), "stdout: {}", stdout(output));
    assert!(stderr(output).contains(text), "stderr: {}", stderr(output));
}

/// Each event's `kind` and `key`, in order.
fn kinds(events: &[Value]) -> Vec<(String, String)> {
    events
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap().to_owned(),
                e["key"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn kind_key(kind: &str, key: &str) -> (String, String) {
    (kind.to_owned(), key.to_owned())
}

#[test]
fn open_list_answer_list() {
    let fx = Fixture::new();

    let open = fx.open_db();
    assert_success(&open);
    let path = fx.task_dir(TASK_ID).join("decisions/db.json");
    assert_eq!(stdout(&open).trim(), path.to_str().unwrap());
    assert_eq!(
        fx.decision_file("db"),
        json!({
            "schema": "conductor-state.v1",
            "key": "db",
            "title": "Which database?",
            "about": "Needs to run in CI",
            "options": [
                {"value": "pg", "label": "Postgres", "hint": "the usual"},
                {"value": "sqlite", "label": "SQLite"},
            ],
            "recommend": "pg",
            "allow_freeform": false,
        })
    );
    let events = fx.events();
    assert_eq!(kinds(&events), [kind_key("decision_open", "db")]);
    assert_eq!(events[0]["task"], TASK_ID);

    // Listed from the worker, then repo-wide from outside the task.
    let list = fx.worker(&["decision", "list", "--json"]);
    assert_success(&list);
    let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["task"], TASK_ID);
    assert_eq!(listed[0]["key"], "db");
    assert!(listed[0].get("answer").is_none());
    let table = fx
        .bare(&["decision", "list", "--open", "--repo", REPO_ID])
        .output()
        .unwrap();
    assert_success(&table);
    assert_eq!(
        stdout(&table),
        format!(
            "TASK          KEY  STATE  TITLE            ANSWER\n{TASK_ID}  db   open   Which database?  -\n"
        )
    );

    // The deck answers from outside the task's environment.
    let answer = fx.outside(&[
        "decision",
        "answer",
        "db",
        "sqlite",
        "--note",
        "keep CI simple",
        "--by",
        "deck",
    ]);
    assert_success(&answer);
    assert_eq!(stdout(&answer), "");
    let file = fx.decision_file("db");
    assert_eq!(file["answer"]["value"], "sqlite");
    assert_eq!(file["answer"]["note"], "keep CI simple");
    assert_eq!(file["answer"]["answered_by"], "deck");
    assert!(file["answer"]["ts"].is_string());
    assert_eq!(
        kinds(&fx.events()),
        [
            kind_key("decision_open", "db"),
            kind_key("decision_answered", "db")
        ]
    );

    let open_only = fx.worker(&["decision", "list", "--open", "--json"]);
    assert_success(&open_only);
    assert_eq!(stdout(&open_only), "[]\n");
    let open_table = fx.worker(&["decision", "list", "--open"]);
    assert_eq!(stdout(&open_table), "no open decisions\n");
    let all = fx.worker(&["decision", "list"]);
    assert_success(&all);
    assert!(
        stdout(&all).contains("db   answered  Which database?  sqlite (deck)"),
        "{}",
        stdout(&all)
    );
}

#[test]
fn list_with_no_decisions() {
    let fx = Fixture::new();
    let table = fx.worker(&["decision", "list"]);
    assert_success(&table);
    assert_eq!(stdout(&table), "no decisions\n");
    let json = fx.outside(&["decision", "list", "--json"]);
    assert_success(&json);
    assert_eq!(stdout(&json), "[]\n");
}

#[test]
fn list_whole_repo_spans_tasks() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.task_dir("add-docs-02")).unwrap();
    assert_success(&fx.open_db());
    let other = fx
        .bare(&[
            "decision",
            "open",
            "tone",
            "--title",
            "Tone?",
            "--allow-freeform",
            "--task",
            "add-docs-02",
        ])
        .env("CONDUCTOR_REPO_ID", REPO_ID)
        .output()
        .unwrap();
    assert_success(&other);

    let list = fx
        .bare(&["decision", "list", "--json", "--repo", REPO_ID])
        .output()
        .unwrap();
    assert_success(&list);
    let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
    let pairs: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["task"].as_str().unwrap(), d["key"].as_str().unwrap()))
        .collect();
    assert_eq!(pairs, [("add-docs-02", "tone"), (TASK_ID, "db")]);
}

#[test]
fn answering_unknown_key_fails() {
    let fx = Fixture::new();
    let output = fx.outside(&["decision", "answer", "nope", "x"]);
    assert_fails_with(
        &output,
        &format!("conductor: no decision `nope` in task `{TASK_ID}`"),
    );
    assert_eq!(fx.events(), Vec::<Value>::new());
}

#[test]
fn answering_twice_fails() {
    let fx = Fixture::new();
    assert_success(&fx.open_db());
    assert_success(&fx.outside(&["decision", "answer", "db", "pg", "--by", "user"]));
    let before = fx.decision_file("db");

    let again = fx.outside(&["decision", "answer", "db", "sqlite"]);
    assert_fails_with(
        &again,
        "conductor: decision `db` was already answered: `pg` by user at ",
    );
    assert_eq!(fx.decision_file("db"), before);
    assert_eq!(fx.events().len(), 2);
}

#[test]
fn answer_must_be_an_option_unless_freeform() {
    let fx = Fixture::new();
    assert_success(&fx.open_db());
    let output = fx.outside(&["decision", "answer", "db", "mysql"]);
    assert_fails_with(
        &output,
        "`mysql` is not an option of `db`; expected one of: pg, sqlite",
    );
    assert!(fx.decision_file("db").get("answer").is_none());

    let open = fx.worker(&[
        "decision",
        "open",
        "name",
        "--title",
        "Name the flag",
        "--option",
        "a=Alpha",
        "--allow-freeform",
    ]);
    assert_success(&open);
    assert_success(&fx.outside(&["decision", "answer", "name", "call it verbose"]));
    assert_eq!(
        fx.decision_file("name")["answer"]["value"],
        "call it verbose"
    );
}

#[test]
fn answered_by_defaults_from_role() {
    let fx = Fixture::new();
    assert_success(&fx.open_db());
    let output = fx
        .bare(&["decision", "answer", "db", "pg"])
        .env("CONDUCTOR_ROLE", "conductor")
        .env("CONDUCTOR_REPO_ID", REPO_ID)
        .args(["--task", TASK_ID])
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(fx.decision_file("db")["answer"]["answered_by"], "conductor");

    assert_success(&fx.worker(&["decision", "open", "x", "--title", "X", "--option", "a=A"]));
    assert_success(&fx.outside(&["decision", "answer", "x", "a"]));
    assert_eq!(fx.decision_file("x")["answer"]["answered_by"], "user");
}

#[test]
fn answering_the_last_open_decision_resumes_the_task() {
    let fx = Fixture::new();
    fx.write_task("needs_decision");
    assert_success(&fx.open_db());
    assert_success(&fx.worker(&["decision", "open", "x", "--title", "X", "--option", "a=A"]));

    let first = fx.outside(&["decision", "answer", "db", "pg", "--by", "deck"]);
    assert_success(&first);
    assert_eq!(fx.phase(), "needs_decision");

    let last = fx.outside(&["decision", "answer", "x", "a", "--by", "deck"]);
    assert_success(&last);
    assert!(last.stdout.is_empty() && last.stderr.is_empty());
    assert_eq!(fx.phase(), "working");
    let events = fx.events();
    assert_eq!(
        kinds(&events),
        [
            kind_key("decision_open", "db"),
            kind_key("decision_open", "x"),
            kind_key("decision_answered", "db"),
            kind_key("decision_answered", "x"),
            kind_key("phase", ""),
        ]
    );
    assert_eq!(events[4]["from"], "needs_decision");
    assert_eq!(events[4]["to"], "working");
}

#[test]
fn answering_leaves_other_phases_alone() {
    let fx = Fixture::new();
    fx.write_task("working");
    assert_success(&fx.open_db());
    assert_success(&fx.outside(&["decision", "answer", "db", "pg"]));
    assert_eq!(fx.phase(), "working");
    assert_eq!(
        kinds(&fx.events()),
        [
            kind_key("decision_open", "db"),
            kind_key("decision_answered", "db"),
        ]
    );
}

#[test]
fn worker_records_the_users_answer_from_its_pane() {
    let fx = Fixture::new();
    fx.write_task("needs_decision");
    assert_success(&fx.open_db());

    let as_conductor = fx.worker(&["decision", "answer", "db", "pg", "--by", "conductor"]);
    assert_fails_with(
        &as_conductor,
        "conductor: a worker can only record the user's answer: use --by user",
    );
    let as_deck = fx.worker(&["decision", "answer", "db", "pg", "--by", "deck"]);
    assert_fails_with(&as_deck, "use --by user");
    let other_task = fx.worker(&["decision", "answer", "db", "pg", "--task", "add-docs-02"]);
    assert_fails_with(
        &other_task,
        "conductor: a worker can only answer its own task's decisions",
    );
    assert!(fx.decision_file("db").get("answer").is_none());
    assert_eq!(fx.phase(), "needs_decision");

    let answer = fx.worker(&[
        "decision",
        "answer",
        "db",
        "pg",
        "--by",
        "user",
        "--note",
        "Plan is approved",
        "--task",
        TASK_ID,
    ]);
    assert_success(&answer);
    let file = fx.decision_file("db");
    assert_eq!(file["answer"]["value"], "pg");
    assert_eq!(file["answer"]["answered_by"], "user");
    assert_eq!(file["answer"]["note"], "Plan is approved");
    assert_eq!(fx.phase(), "working");
}

#[test]
fn opening_an_open_key_fails_but_an_answered_one_reopens() {
    let fx = Fixture::new();
    assert_success(&fx.open_db());
    let again = fx.open_db();
    assert_fails_with(&again, "conductor: decision `db` is already open");
    assert_eq!(fx.events().len(), 1);

    assert_success(&fx.outside(&["decision", "answer", "db", "pg"]));
    assert_success(&fx.open_db());
    assert!(fx.decision_file("db").get("answer").is_none());
    assert_eq!(
        kinds(&fx.events()),
        [
            kind_key("decision_open", "db"),
            kind_key("decision_answered", "db"),
            kind_key("decision_open", "db"),
        ]
    );
}

#[test]
fn open_rejects_bad_input() {
    let fx = Fixture::new();
    let cases: [(&[&str], &str); 6] = [
        (
            &["../x", "--title", "T", "--option", "a=A"],
            "invalid decision key \"../x\"",
        ),
        (
            &["k", "--title", "T", "--option", "a"],
            "expected <value>=<label>",
        ),
        (
            &["k", "--title", "T", "--option", "a="],
            "needs both a value and a label",
        ),
        (
            &["k", "--title", "T", "--option", "a=A", "--recommend", "b"],
            "--recommend `b` is not one of the options",
        ),
        (
            &["k", "--title", "T", "--option", "a=A", "--option", "a=B"],
            "option `a` is given more than once",
        ),
        (&["k", "--title", "T"], "give at least one --option"),
    ];
    for (args, error) in cases {
        let mut full = vec!["decision", "open"];
        full.extend(args);
        let output = fx.worker(&full);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            stderr(&output).contains(error),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    assert!(!fx.task_dir(TASK_ID).join("decisions").exists());
    assert_eq!(fx.events(), Vec::<Value>::new());
}

#[test]
fn report_appends_status() {
    let fx = Fixture::new();
    assert_success(&fx.worker(&["report", "working", "Wiring the login form"]));
    let url = "https://github.com/erd/my-repo/pull/42";
    let done = fx.outside(&["report", "done", "--pr", url]);
    assert_success(&done);
    assert!(done.stdout.is_empty() && done.stderr.is_empty());

    let events = fx.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "status");
    assert_eq!(events[0]["task"], TASK_ID);
    assert_eq!(events[0]["verb"], "working");
    assert_eq!(events[0]["text"], "Wiring the login form");
    assert!(events[0].get("pr_url").is_none());
    assert_eq!(events[1]["verb"], "done");
    assert!(events[1].get("text").is_none());
    assert_eq!(events[1]["pr_url"], url);
}

#[test]
fn report_needs_a_known_task() {
    let fx = Fixture::new();
    let no_task = fx
        .bare(&["report", "working", "x"])
        .env("CONDUCTOR_REPO_ID", REPO_ID)
        .output()
        .unwrap();
    assert_fails_with(
        &no_task,
        "conductor: no task: pass --task or set CONDUCTOR_TASK_ID",
    );

    let unknown = fx
        .bare(&["report", "working", "x", "--task", "nope-01"])
        .env("CONDUCTOR_REPO_ID", REPO_ID)
        .output()
        .unwrap();
    assert_fails_with(
        &unknown,
        &format!("conductor: no task `nope-01` in repo `{REPO_ID}`"),
    );

    let escape = fx
        .bare(&["report", "working", "x", "--task", ".."])
        .env("CONDUCTOR_REPO_ID", REPO_ID)
        .output()
        .unwrap();
    assert_fails_with(&escape, "conductor: invalid task id \"..\"");

    let no_repo = fx
        .bare(&["report", "working", "x", "--task", TASK_ID])
        .output()
        .unwrap();
    assert_fails_with(&no_repo, "pass --repo or set CONDUCTOR_REPO_ID");
}

#[test]
fn report_rejects_bad_input() {
    let fx = Fixture::new();
    let verb = fx.worker(&["report", "finished"]);
    assert_eq!(verb.status.code(), Some(2));
    assert!(stderr(&verb).contains("invalid value 'finished'"));

    let pr = fx.worker(&["report", "done", "--pr", "#42"]);
    assert_fails_with(&pr, "--pr must be a URL");
    assert_eq!(fx.events(), Vec::<Value>::new());
}
