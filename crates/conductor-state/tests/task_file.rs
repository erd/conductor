use conductor_state::{
    Error, Phase, SchemaV1, Task, TaskKind, read_task, task_path, write_task_atomic,
};

fn task(phase: Phase) -> Task {
    let ts = "2026-10-14T12:00:00Z".parse().unwrap();
    Task {
        schema: SchemaV1,
        id: "fix-login-07".into(),
        title: "Fix login".into(),
        kind: TaskKind::Ship,
        phase,
        branch: Some("conductor/fix-login-07".into()),
        worktree_path: None,
        session_name: None,
        claude_session_id: None,
        herdr_workspace_id: None,
        herdr_pane_id: None,
        pr_url: None,
        created_at: ts,
        updated_at: ts,
    }
}

fn file_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn round_trips_and_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    write_task_atomic(dir.path(), &task(Phase::Intake)).unwrap();
    assert_eq!(read_task(dir.path()).unwrap(), task(Phase::Intake));

    write_task_atomic(dir.path(), &task(Phase::Briefed)).unwrap();
    assert_eq!(read_task(dir.path()).unwrap(), task(Phase::Briefed));

    // No temp files left behind.
    assert_eq!(file_names(dir.path()), ["task.json"]);
    let text = std::fs::read_to_string(task_path(dir.path())).unwrap();
    assert!(text.ends_with("}\n"));
}

#[test]
fn missing_dir_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let err = write_task_atomic(&dir.path().join("nope"), &task(Phase::Intake)).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert!(file_names(dir.path()).is_empty());
}

#[test]
fn rejects_other_schema() {
    let dir = tempfile::tempdir().unwrap();
    write_task_atomic(dir.path(), &task(Phase::Intake)).unwrap();
    let path = task_path(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("conductor-state.v1", "conductor-state.v2"),
    )
    .unwrap();
    let err = read_task(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Json { .. }), "{err}");
}
