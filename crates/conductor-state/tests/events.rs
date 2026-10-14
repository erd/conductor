use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::sync::Barrier;
use std::thread;

use conductor_state::{
    Error, Event, EventPayload, MAX_EVENT_LINE, MAX_TEXT_FIELD, append_event, events_path,
    read_events,
};
use serde_json::{Map, Value};

fn event(payload: EventPayload) -> Event {
    Event {
        ts: "2026-10-14T12:00:00Z".parse().unwrap(),
        task: "fix-login-07".into(),
        session_id: Some("s1".into()),
        payload,
    }
}

fn prompt(text: impl Into<String>) -> Event {
    event(EventPayload::Prompt { text: text.into() })
}

fn append_raw(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn concurrent_appends_stay_whole() {
    const THREADS: usize = 16;
    const PER_THREAD: usize = 50;

    let dir = tempfile::tempdir().unwrap();
    let barrier = Barrier::new(THREADS);
    thread::scope(|s| {
        for t in 0..THREADS {
            let (dir, barrier) = (dir.path(), &barrier);
            s.spawn(move || {
                barrier.wait();
                for i in 0..PER_THREAD {
                    // Mix short lines with ones near the size cap.
                    let pad = if i % 3 == 0 { 3500 } else { 10 };
                    let text = format!("{t}:{i}:{}", "x".repeat(pad));
                    let summary = Some(text[..text.len().min(MAX_TEXT_FIELD)].to_owned());
                    let ev = event(EventPayload::ToolStart {
                        tool_name: format!("t{t}"),
                        summary,
                        tool_use_id: Some(format!("{t}-{i}")),
                    });
                    append_event(dir, &ev).unwrap();
                }
            });
        }
    });

    let path = events_path(dir.path());
    let raw = std::fs::read_to_string(&path).unwrap();
    assert_eq!(raw.lines().count(), THREADS * PER_THREAD);
    let batch = read_events(&path, 0).unwrap();
    assert!(batch.skipped.is_empty(), "{:?}", batch.skipped);
    assert_eq!(batch.events.len(), THREADS * PER_THREAD);
    assert_eq!(batch.offset, raw.len() as u64);

    // Every (thread, index) pair landed exactly once.
    let mut ids: Vec<String> = batch
        .events
        .iter()
        .map(|e| match &e.payload {
            EventPayload::ToolStart { tool_use_id, .. } => tool_use_id.clone().unwrap(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), THREADS * PER_THREAD);
}

#[test]
fn resumes_from_saved_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = events_path(dir.path());
    append_event(dir.path(), &prompt("one")).unwrap();
    append_event(dir.path(), &prompt("two")).unwrap();

    let first = read_events(&path, 0).unwrap();
    assert_eq!(first.events, [prompt("one"), prompt("two")]);

    append_event(dir.path(), &prompt("three")).unwrap();
    let second = read_events(&path, first.offset).unwrap();
    assert_eq!(second.events, [prompt("three")]);
    assert_eq!(second.offset, std::fs::metadata(&path).unwrap().len());

    let third = read_events(&path, second.offset).unwrap();
    assert_eq!(third.events, []);
    assert_eq!(third.offset, second.offset);
}

#[test]
fn ignores_trailing_partial_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = events_path(dir.path());
    append_event(dir.path(), &prompt("one")).unwrap();
    let complete = std::fs::metadata(&path).unwrap().len();

    let line = serde_json::to_string(&prompt("two")).unwrap();
    let (head, tail) = line.split_at(line.len() / 2);
    append_raw(&path, head.as_bytes());

    let batch = read_events(&path, 0).unwrap();
    assert_eq!(batch.events, [prompt("one")]);
    assert_eq!(batch.offset, complete);
    assert_eq!(batch.skipped, []);

    // Finishing the line makes it visible to the next read.
    append_raw(&path, format!("{tail}\n").as_bytes());
    let batch = read_events(&path, batch.offset).unwrap();
    assert_eq!(batch.events, [prompt("two")]);
}

#[test]
fn skips_malformed_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = events_path(dir.path());
    append_event(dir.path(), &prompt("one")).unwrap();
    let bad_offset = std::fs::metadata(&path).unwrap().len();
    append_raw(&path, b"{not json\n\n");
    append_event(dir.path(), &prompt("two")).unwrap();

    let batch = read_events(&path, 0).unwrap();
    assert_eq!(batch.events, [prompt("one"), prompt("two")]);
    assert_eq!(batch.skipped.len(), 1);
    assert_eq!(batch.skipped[0].offset, bad_offset);
}

#[test]
fn missing_file_reads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let batch = read_events(&events_path(dir.path()), 0).unwrap();
    assert_eq!(batch.events, []);
    assert_eq!(batch.offset, 0);
}

#[test]
fn offset_past_end_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    append_event(dir.path(), &prompt("one")).unwrap();
    let err = read_events(&events_path(dir.path()), 10_000).unwrap_err();
    assert!(matches!(err, Error::OffsetPastEnd { .. }), "{err}");
}

#[test]
fn long_text_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    append_event(dir.path(), &prompt("é".repeat(5000))).unwrap();

    let batch = read_events(&events_path(dir.path()), 0).unwrap();
    let EventPayload::Prompt { text } = &batch.events[0].payload else {
        panic!("not a prompt");
    };
    assert!(text.len() <= MAX_TEXT_FIELD);
    assert!(text.ends_with('…'));
    assert!(batch.offset <= MAX_EVENT_LINE as u64);
}

#[test]
fn oversize_line_is_rejected_and_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = Map::new();
    fields.insert("blob".into(), Value::String("x".repeat(MAX_EVENT_LINE)));
    let ev = event(EventPayload::Unknown {
        kind: "future_kind".into(),
        fields,
    });

    let err = append_event(dir.path(), &ev).unwrap_err();
    assert!(matches!(err, Error::EventTooLarge { .. }), "{err}");
    assert!(!events_path(dir.path()).exists());
}

#[test]
fn missing_task_dir_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = append_event(&dir.path().join("nope"), &prompt("one")).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
}
