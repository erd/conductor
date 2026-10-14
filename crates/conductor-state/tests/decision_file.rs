use conductor_state::{
    Answer, AnsweredBy, Decision, DecisionOption, Error, SchemaV1, decision_path, read_decision,
    read_decisions, write_decision_atomic,
};

fn decision(key: &str) -> Decision {
    Decision {
        schema: SchemaV1,
        key: key.into(),
        title: format!("Pick {key}"),
        about: String::new(),
        options: vec![DecisionOption {
            value: "a".into(),
            label: "A".into(),
            hint: None,
        }],
        recommend: None,
        allow_freeform: false,
        answer: None,
    }
}

#[test]
fn round_trips_and_overwrites() {
    let task = tempfile::tempdir().unwrap();
    assert_eq!(read_decision(task.path(), "db").unwrap(), None);

    write_decision_atomic(task.path(), &decision("db")).unwrap();
    assert_eq!(
        read_decision(task.path(), "db").unwrap(),
        Some(decision("db"))
    );

    let answered = Decision {
        answer: Some(Answer {
            value: "a".into(),
            note: None,
            answered_by: AnsweredBy::Deck,
            ts: "2026-10-14T12:00:00Z".parse().unwrap(),
        }),
        ..decision("db")
    };
    write_decision_atomic(task.path(), &answered).unwrap();
    assert_eq!(read_decision(task.path(), "db").unwrap(), Some(answered));
    assert!(decision_path(task.path(), "db").is_file());
}

#[test]
fn lists_sorted_and_skips_other_files() {
    let task = tempfile::tempdir().unwrap();
    assert_eq!(read_decisions(task.path()).unwrap(), []);

    for key in ["zeta", "alpha", "mid"] {
        write_decision_atomic(task.path(), &decision(key)).unwrap();
    }
    let dir = task.path().join("decisions");
    std::fs::write(dir.join(".alpha.json.1.0.tmp"), "partial").unwrap();
    std::fs::write(dir.join("notes.txt"), "not a decision").unwrap();

    let keys: Vec<_> = read_decisions(task.path())
        .unwrap()
        .into_iter()
        .map(|d| d.key)
        .collect();
    assert_eq!(keys, ["alpha", "mid", "zeta"]);
}

#[test]
fn bad_file_names_its_path() {
    let task = tempfile::tempdir().unwrap();
    write_decision_atomic(task.path(), &decision("db")).unwrap();
    let path = decision_path(task.path(), "db");
    std::fs::write(&path, "{").unwrap();
    let err = read_decisions(task.path()).unwrap_err();
    assert!(
        matches!(&err, Error::Json { path: p, .. } if *p == path),
        "{err}"
    );
}

#[test]
fn missing_task_dir_is_not_created() {
    let tmp = tempfile::tempdir().unwrap();
    let task = tmp.path().join("no-such-task");
    let err = write_decision_atomic(&task, &decision("db")).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert!(!task.exists());
}
