//! Each test parses a hand-written literal, so these pin the wire format, then checks that
//! serializing the result gives the same document back.

use std::collections::BTreeSet;
use std::fmt::Debug;

use conductor_state::{
    Answer, AnsweredBy, CheckState, Config, Decision, DecisionOption, Event, EventPayload, Phase,
    PrState, RepoOverrides, RepoRecord, ReviewDecision, SchemaV1, Settings, StatusVerb, Task,
    TaskKind,
};
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const TS: &str = "2026-10-14T12:00:00Z";

fn ts() -> Timestamp {
    TS.parse().unwrap()
}

/// Parses `doc`, compares it with `expected`, and checks that it serializes back to `doc`.
fn check<T>(doc: &Value, expected: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let parsed: T = serde_json::from_str(&doc.to_string()).expect("deserialize");
    assert_eq!(&parsed, expected);
    assert_eq!(&serde_json::to_value(&parsed).expect("serialize"), doc);
}

fn minimal_task() -> Task {
    Task {
        schema: SchemaV1,
        id: "fix-login-07".into(),
        title: "Fix login".into(),
        kind: TaskKind::Ship,
        phase: Phase::Intake,
        branch: None,
        worktree_path: None,
        session_name: None,
        claude_session_id: None,
        herdr_workspace_id: None,
        herdr_pane_id: None,
        pr_url: None,
        created_at: ts(),
        updated_at: ts(),
    }
}

#[test]
fn task_minimal() {
    let doc = json!({
        "schema": "conductor-state.v1",
        "id": "fix-login-07",
        "title": "Fix login",
        "kind": "ship",
        "phase": "intake",
        "created_at": TS,
        "updated_at": TS,
    });
    check(&doc, &minimal_task());
}

#[test]
fn task_full() {
    let doc = json!({
        "schema": "conductor-state.v1",
        "id": "map-auth-02",
        "title": "Map the auth flow",
        "kind": "scout",
        "phase": "awaiting_merge",
        "branch": "conductor/map-auth-02",
        "worktree_path": "/src/app/.worktrees/conductor-map-auth-02",
        "session_name": "app-map-auth-02",
        "claude_session_id": "5f0c",
        "herdr_workspace_id": "ws-3",
        "herdr_pane_id": "pane-9",
        "pr_url": "https://github.com/erd/app/pull/12",
        "created_at": TS,
        "updated_at": "2026-10-14T13:30:05.25Z",
    });
    let task = Task {
        id: "map-auth-02".into(),
        title: "Map the auth flow".into(),
        kind: TaskKind::Scout,
        phase: Phase::AwaitingMerge,
        branch: Some("conductor/map-auth-02".into()),
        worktree_path: Some("/src/app/.worktrees/conductor-map-auth-02".into()),
        session_name: Some("app-map-auth-02".into()),
        claude_session_id: Some("5f0c".into()),
        herdr_workspace_id: Some("ws-3".into()),
        herdr_pane_id: Some("pane-9".into()),
        pr_url: Some("https://github.com/erd/app/pull/12".into()),
        updated_at: "2026-10-14T13:30:05.25Z".parse().unwrap(),
        ..minimal_task()
    };
    check(&doc, &task);
}

#[test]
fn every_phase_is_snake_case() {
    let names: Vec<Value> = Phase::ALL
        .iter()
        .map(|phase| serde_json::to_value(phase).unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "intake",
            "briefed",
            "spawning",
            "working",
            "needs_decision",
            "review",
            "awaiting_merge",
            "merged",
            "torn_down",
            "failed",
            "abandoned",
        ]
    );
    for phase in Phase::ALL {
        let back: Phase = serde_json::from_value(serde_json::to_value(phase).unwrap()).unwrap();
        assert_eq!(back, phase);
    }
}

/// One sample per variant, as (payload fields, payload).
#[allow(clippy::too_many_lines, reason = "a fixture table")]
fn event_samples() -> Vec<(Value, EventPayload)> {
    let mut unknown_fields = serde_json::Map::new();
    unknown_fields.insert("depth".into(), json!(3));
    unknown_fields.insert("nested".into(), json!({"a": [1, 2]}));

    vec![
        (
            json!({"kind": "session_start", "source": "startup"}),
            EventPayload::SessionStart {
                source: "startup".into(),
                herdr_pane_id: None,
                herdr_workspace_id: None,
                transcript_path: None,
            },
        ),
        (
            json!({
                "kind": "session_start",
                "source": "resume",
                "herdr_pane_id": "pane-9",
                "herdr_workspace_id": "ws-3",
                "transcript_path": "/home/me/.claude/projects/x/5f0c.jsonl",
            }),
            EventPayload::SessionStart {
                source: "resume".into(),
                herdr_pane_id: Some("pane-9".into()),
                herdr_workspace_id: Some("ws-3".into()),
                transcript_path: Some("/home/me/.claude/projects/x/5f0c.jsonl".into()),
            },
        ),
        (
            json!({"kind": "prompt", "text": "Read your brief"}),
            EventPayload::Prompt {
                text: "Read your brief".into(),
            },
        ),
        (
            json!({
                "kind": "tool_start",
                "tool_name": "Edit",
                "summary": "src/x.rs",
                "tool_use_id": "toolu_1",
            }),
            EventPayload::ToolStart {
                tool_name: "Edit".into(),
                summary: Some("src/x.rs".into()),
                tool_use_id: Some("toolu_1".into()),
            },
        ),
        (
            json!({"kind": "tool_end", "tool_name": "Bash", "ok": false}),
            EventPayload::ToolEnd {
                tool_name: "Bash".into(),
                summary: None,
                tool_use_id: None,
                ok: false,
            },
        ),
        (
            json!({
                "kind": "attention",
                "type": "permission_prompt",
                "message": "Claude needs your permission to use Bash",
            }),
            EventPayload::Attention {
                notification_type: "permission_prompt".into(),
                message: "Claude needs your permission to use Bash".into(),
            },
        ),
        (
            json!({"kind": "turn_end"}),
            EventPayload::TurnEnd { error: None },
        ),
        (
            json!({"kind": "turn_end", "error": "rate_limit"}),
            EventPayload::TurnEnd {
                error: Some("rate_limit".into()),
            },
        ),
        (
            json!({"kind": "session_end", "reason": "logout"}),
            EventPayload::SessionEnd {
                reason: "logout".into(),
            },
        ),
        (
            json!({"kind": "status", "verb": "working", "text": "wiring the hook"}),
            EventPayload::Status {
                verb: StatusVerb::Working,
                text: Some("wiring the hook".into()),
                pr_url: None,
            },
        ),
        (
            json!({"kind": "status", "verb": "blocked", "text": "need a decision"}),
            EventPayload::Status {
                verb: StatusVerb::Blocked,
                text: Some("need a decision".into()),
                pr_url: None,
            },
        ),
        (
            json!({
                "kind": "status",
                "verb": "done",
                "pr_url": "https://github.com/erd/app/pull/12",
            }),
            EventPayload::Status {
                verb: StatusVerb::Done,
                text: None,
                pr_url: Some("https://github.com/erd/app/pull/12".into()),
            },
        ),
        (
            json!({"kind": "status", "verb": "failed", "text": "tests won't build"}),
            EventPayload::Status {
                verb: StatusVerb::Failed,
                text: Some("tests won't build".into()),
                pr_url: None,
            },
        ),
        (
            json!({"kind": "decision_open", "key": "db-choice"}),
            EventPayload::DecisionOpen {
                key: "db-choice".into(),
            },
        ),
        (
            json!({"kind": "decision_answered", "key": "db-choice"}),
            EventPayload::DecisionAnswered {
                key: "db-choice".into(),
            },
        ),
        (
            json!({"kind": "phase", "from": "needs_decision", "to": "working"}),
            EventPayload::Phase {
                from: Phase::NeedsDecision,
                to: Phase::Working,
            },
        ),
        (
            json!({"kind": "pr", "state": "draft"}),
            EventPayload::Pr {
                state: PrState::Draft,
                checks: None,
                review: None,
            },
        ),
        (
            json!({"kind": "pr", "state": "open", "checks": "pending", "review": "review_required"}),
            EventPayload::Pr {
                state: PrState::Open,
                checks: Some(CheckState::Pending),
                review: Some(ReviewDecision::ReviewRequired),
            },
        ),
        (
            json!({"kind": "pr", "state": "open", "checks": "fail", "review": "changes_requested"}),
            EventPayload::Pr {
                state: PrState::Open,
                checks: Some(CheckState::Fail),
                review: Some(ReviewDecision::ChangesRequested),
            },
        ),
        (
            json!({"kind": "pr", "state": "merged", "checks": "pass", "review": "approved"}),
            EventPayload::Pr {
                state: PrState::Merged,
                checks: Some(CheckState::Pass),
                review: Some(ReviewDecision::Approved),
            },
        ),
        (
            json!({"kind": "pr", "state": "closed"}),
            EventPayload::Pr {
                state: PrState::Closed,
                checks: None,
                review: None,
            },
        ),
        (
            json!({"kind": "subagent_start", "depth": 3, "nested": {"a": [1, 2]}}),
            EventPayload::Unknown {
                kind: "subagent_start".into(),
                fields: unknown_fields,
            },
        ),
    ]
}

/// Names each variant. The match is exhaustive, so adding a variant forces a new sample.
fn variant_name(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::SessionStart { .. } => "session_start",
        EventPayload::Prompt { .. } => "prompt",
        EventPayload::ToolStart { .. } => "tool_start",
        EventPayload::ToolEnd { .. } => "tool_end",
        EventPayload::Attention { .. } => "attention",
        EventPayload::TurnEnd { .. } => "turn_end",
        EventPayload::SessionEnd { .. } => "session_end",
        EventPayload::Status { .. } => "status",
        EventPayload::DecisionOpen { .. } => "decision_open",
        EventPayload::DecisionAnswered { .. } => "decision_answered",
        EventPayload::Phase { .. } => "phase",
        EventPayload::Pr { .. } => "pr",
        EventPayload::Unknown { .. } => "unknown",
    }
}

#[test]
fn every_event_kind_round_trips() {
    let samples = event_samples();
    let covered: BTreeSet<&str> = samples.iter().map(|(_, p)| variant_name(p)).collect();
    assert_eq!(
        covered.len(),
        13,
        "every variant needs a sample: {covered:?}"
    );

    for (fields, payload) in samples {
        let mut doc = json!({"ts": TS, "task": "fix-login-07", "session_id": "5f0c"});
        doc.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let event = Event {
            ts: ts(),
            task: "fix-login-07".into(),
            session_id: Some("5f0c".into()),
            payload,
        };
        check(&doc, &event);
    }
}

#[test]
fn event_without_session_id() {
    let doc = json!({"ts": TS, "task": "fix-login-07", "kind": "turn_end"});
    let event = Event {
        ts: ts(),
        task: "fix-login-07".into(),
        session_id: None,
        payload: EventPayload::TurnEnd { error: None },
    };
    check(&doc, &event);
}

#[test]
fn malformed_known_event_is_an_error() {
    let line = json!({"ts": TS, "task": "t", "kind": "tool_end", "tool_name": "Bash"});
    let err = serde_json::from_value::<Event>(line).unwrap_err();
    assert!(err.to_string().contains("missing field `ok`"), "{err}");

    let line = json!({"ts": TS, "task": "t", "kind": "status", "verb": "celebrating"});
    assert!(serde_json::from_value::<Event>(line).is_err());

    let line = json!({"ts": TS, "task": "t", "kind": 7});
    assert!(serde_json::from_value::<Event>(line).is_err());

    let line = json!({"ts": TS, "task": "t"});
    assert!(serde_json::from_value::<Event>(line).is_err());
}

fn decision() -> Decision {
    Decision {
        schema: SchemaV1,
        key: "db-choice".into(),
        title: "Which database?".into(),
        about: "The cache needs a store.".into(),
        options: vec![
            DecisionOption {
                value: "sqlite".into(),
                label: "SQLite".into(),
                hint: Some("no server".into()),
            },
            DecisionOption {
                value: "postgres".into(),
                label: "Postgres".into(),
                hint: None,
            },
        ],
        recommend: Some("sqlite".into()),
        allow_freeform: true,
        answer: None,
    }
}

fn decision_doc() -> Value {
    json!({
        "schema": "conductor-state.v1",
        "key": "db-choice",
        "title": "Which database?",
        "about": "The cache needs a store.",
        "options": [
            {"value": "sqlite", "label": "SQLite", "hint": "no server"},
            {"value": "postgres", "label": "Postgres"},
        ],
        "recommend": "sqlite",
        "allow_freeform": true,
    })
}

#[test]
fn decision_open() {
    check(&decision_doc(), &decision());
}

#[test]
fn decision_answered() {
    for (by, by_name, note) in [
        (AnsweredBy::User, "user", Some("keep it simple")),
        (AnsweredBy::Conductor, "conductor", None),
    ] {
        let mut doc = decision_doc();
        let mut answer = json!({"value": "sqlite", "answered_by": by_name, "ts": TS});
        if let Some(note) = note {
            answer["note"] = json!(note);
        }
        doc["answer"] = answer;
        let expected = Decision {
            answer: Some(Answer {
                value: "sqlite".into(),
                note: note.map(Into::into),
                answered_by: by,
                ts: ts(),
            }),
            ..decision()
        };
        check(&doc, &expected);
    }
}

#[test]
fn decision_defaults() {
    let doc = json!({
        "schema": "conductor-state.v1",
        "key": "name",
        "title": "Name the flag",
        "about": "",
    });
    let decision: Decision = serde_json::from_value(doc).unwrap();
    assert!(decision.options.is_empty());
    assert!(!decision.allow_freeform);
    assert_eq!(decision.recommend, None);
}

#[test]
fn repo_record() {
    let doc = json!({
        "schema": "conductor-state.v1",
        "repo_root": "/src/hamlet-web-v2",
        "repo_id": "hamlet-web-v2-3f9a1c",
        "conductor_session": "conductor-hamlet-web-v2",
        "herdr_workspace_id": "ws-1",
    });
    let record = RepoRecord {
        schema: SchemaV1,
        repo_root: "/src/hamlet-web-v2".into(),
        repo_id: "hamlet-web-v2-3f9a1c".into(),
        conductor_session: "conductor-hamlet-web-v2".into(),
        herdr_workspace_id: Some("ws-1".into()),
    };
    check(&doc, &record);

    let mut doc = doc;
    doc.as_object_mut().unwrap().remove("herdr_workspace_id");
    check(
        &doc,
        &RepoRecord {
            herdr_workspace_id: None,
            ..record
        },
    );
}

#[test]
fn unknown_fields_are_ignored() {
    let mut task = json!({
        "schema": "conductor-state.v1",
        "id": "fix-login-07",
        "title": "Fix login",
        "kind": "ship",
        "phase": "intake",
        "created_at": TS,
        "updated_at": TS,
    });
    task["priority"] = json!("high");
    assert_eq!(
        serde_json::from_value::<Task>(task).unwrap(),
        minimal_task()
    );

    let mut decision_doc = decision_doc();
    decision_doc["deadline"] = json!(TS);
    decision_doc["options"][0]["color"] = json!("green");
    assert_eq!(
        serde_json::from_value::<Decision>(decision_doc).unwrap(),
        decision()
    );

    let line = json!({
        "ts": TS,
        "task": "t",
        "kind": "prompt",
        "text": "hi",
        "cwd": "/src",
    });
    assert_eq!(
        serde_json::from_value::<Event>(line).unwrap().payload,
        EventPayload::Prompt { text: "hi".into() }
    );
}

#[test]
fn wrong_schema_marker_is_rejected() {
    for marker in [json!("conductor-state.v2"), json!(1), Value::Null] {
        let doc = json!({
            "schema": marker,
            "repo_root": "/src/app",
            "repo_id": "app-000000",
            "conductor_session": "conductor-app",
        });
        assert!(serde_json::from_value::<RepoRecord>(doc).is_err());
    }

    let doc = json!({
        "repo_root": "/src/app",
        "repo_id": "app-000000",
        "conductor_session": "conductor-app",
    });
    assert!(serde_json::from_value::<RepoRecord>(doc).is_err());
}

#[test]
fn config_defaults() {
    let config: Config = toml::from_str("").unwrap();
    assert_eq!(config, Config::default());
    assert_eq!(
        config.defaults,
        Settings {
            model: None,
            effort: None,
            branch_prefix: "conductor/".into(),
            deck_width: 48,
            pr_poll_interval: SignedDuration::from_mins(2),
        }
    );
    assert_eq!(config.resolve("anything"), config.defaults);
}

const FULL_CONFIG: &str = r#"
model = "opus"
effort = "high"
branch_prefix = "erd/"
deck_width = 60
pr_poll_interval = "45s"
unknown_key = true

[repos."hamlet-web-v2-3f9a1c"]
model = "sonnet"
deck_width = 72
pr_poll_interval = "5m"

[repos."quiet-000000"]
"#;

fn full_config() -> Config {
    Config {
        defaults: Settings {
            model: Some("opus".into()),
            effort: Some("high".into()),
            branch_prefix: "erd/".into(),
            deck_width: 60,
            pr_poll_interval: SignedDuration::from_secs(45),
        },
        repos: [
            (
                "hamlet-web-v2-3f9a1c".to_owned(),
                RepoOverrides {
                    model: Some("sonnet".into()),
                    deck_width: Some(72),
                    pr_poll_interval: Some(SignedDuration::from_mins(5)),
                    ..RepoOverrides::default()
                },
            ),
            ("quiet-000000".to_owned(), RepoOverrides::default()),
        ]
        .into(),
    }
}

#[test]
fn config_round_trip() {
    let config: Config = toml::from_str(FULL_CONFIG).unwrap();
    assert_eq!(config, full_config());

    let written = toml::to_string(&config).unwrap();
    assert!(written.contains(r#"pr_poll_interval = "45s""#), "{written}");
    assert!(written.contains(r#"pr_poll_interval = "5m""#), "{written}");
    assert_eq!(toml::from_str::<Config>(&written).unwrap(), config);
}

#[test]
fn config_resolve() {
    let config = full_config();

    assert_eq!(
        config.resolve("hamlet-web-v2-3f9a1c"),
        Settings {
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            branch_prefix: "erd/".into(),
            deck_width: 72,
            pr_poll_interval: SignedDuration::from_mins(5),
        }
    );
    assert_eq!(config.resolve("quiet-000000"), config.defaults);
    assert_eq!(config.resolve("not-configured-111111"), config.defaults);
}
