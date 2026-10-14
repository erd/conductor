use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::{Error, Event, RepoRecord, Result, Task, events_path, repo_record_path, task_path};

/// The longest event line [`append_event`] writes, including its `\n`.
///
/// Each line goes out in a single `write(2)` on a file opened with `O_APPEND`. On local
/// filesystems (APFS, ext4, xfs) such a write lands as one unbroken run even with other
/// processes appending at the same time; keeping lines under one page stays well inside that
/// behavior.
pub const MAX_EVENT_LINE: usize = 4096;

/// The longest free-text field (prompt text, tool summary, status text, …) [`append_event`]
/// writes, in bytes. Longer fields are cut and end with `…`.
pub const MAX_TEXT_FIELD: usize = 1024;

/// Appends `event` to the task's `events.jsonl` as one JSON line.
///
/// Free-text fields are first cut to [`MAX_TEXT_FIELD`]. The line is written in a single
/// `write` call, so readers and other appenders never see part of it. Doesn't create
/// `task_dir`: a missing task dir means a bad task id.
///
/// # Errors
///
/// [`Error::EventTooLarge`] if the line is still over [`MAX_EVENT_LINE`], in which case
/// nothing is written; [`Error::Io`] if the file can't be opened or the write comes up short.
pub fn append_event(task_dir: &Path, event: &Event) -> Result<()> {
    let path = events_path(task_dir);
    let mut event = event.clone();
    event.truncate_text(MAX_TEXT_FIELD);
    let mut line = serde_json::to_vec(&event).map_err(Error::json(&path))?;
    line.push(b'\n');
    if line.len() > MAX_EVENT_LINE {
        return Err(Error::EventTooLarge { len: line.len() });
    }

    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(Error::io(&path))?;
    // One `write`, not `write_all`: retrying a short write would split the line.
    let written = file.write(&line).map_err(Error::io(&path))?;
    if written != line.len() {
        return Err(Error::Io {
            path,
            source: std::io::Error::new(
                ErrorKind::WriteZero,
                format!("short write: {written} of {} bytes", line.len()),
            ),
        });
    }
    Ok(())
}

/// The result of [`read_events`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventBatch {
    /// Every complete, well-formed line after the starting offset, in order.
    pub events: Vec<Event>,
    /// Where the next read should start: just past the last complete line.
    pub offset: u64,
    /// Lines that didn't parse.
    pub skipped: Vec<SkippedLine>,
}

/// A line [`read_events`] couldn't parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedLine {
    /// Byte offset of the line's start.
    pub offset: u64,
    pub error: String,
}

/// Reads the events in `path` from byte `from_offset` on, for tailing an `events.jsonl`.
///
/// Start at 0, then pass each returned [`EventBatch::offset`] to the next call. A trailing
/// line without its `\n` is still being written, so it's left for the next call. Malformed
/// lines are returned in [`EventBatch::skipped`] rather than failing the read. A missing file
/// reads as empty.
///
/// # Errors
///
/// [`Error::OffsetPastEnd`] if the file is shorter than `from_offset`, meaning it was
/// truncated or replaced; [`Error::Io`] if it can't be read.
pub fn read_events(path: &Path, from_offset: u64) -> Result<EventBatch> {
    let mut batch = EventBatch {
        offset: from_offset,
        ..EventBatch::default()
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(batch),
        Err(e) => return Err(Error::io(path)(e)),
    };
    let len = file.metadata().map_err(Error::io(path))?.len();
    if from_offset > len {
        return Err(Error::OffsetPastEnd {
            path: path.to_owned(),
            offset: from_offset,
            len,
        });
    }
    file.seek(SeekFrom::Start(from_offset))
        .map_err(Error::io(path))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(Error::io(path))?;

    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(batch);
    };
    let mut line_offset = from_offset;
    for line in buf[..=end].split_inclusive(|&b| b == b'\n') {
        let text = line.trim_ascii();
        if !text.is_empty() {
            match serde_json::from_slice::<Event>(text) {
                Ok(event) => batch.events.push(event),
                Err(e) => batch.skipped.push(SkippedLine {
                    offset: line_offset,
                    error: e.to_string(),
                }),
            }
        }
        line_offset += line.len() as u64;
    }
    batch.offset = line_offset;
    Ok(batch)
}

/// Writes `task.json` in `task_dir`: a temp file in the same directory, `fsync`, then
/// `rename` over the old one, so a reader sees either the old record or the new one.
///
/// # Errors
///
/// [`Error::Io`] if any step fails; the temp file is removed.
pub fn write_task_atomic(task_dir: &Path, task: &Task) -> Result<()> {
    write_json_atomic(&task_path(task_dir), task)
}

/// Reads `task.json` in `task_dir`.
///
/// # Errors
///
/// [`Error::Io`] if it can't be read, and [`Error::Json`] if it doesn't parse, including a
/// schema marker other than `conductor-state.v1`.
pub fn read_task(task_dir: &Path) -> Result<Task> {
    let path = task_path(task_dir);
    let bytes = fs::read(&path).map_err(Error::io(&path))?;
    serde_json::from_slice(&bytes).map_err(Error::json(&path))
}

/// Writes `repo.json` in `repo_dir`, the same way as [`write_task_atomic`].
///
/// # Errors
///
/// [`Error::Io`] if any step fails; the temp file is removed.
pub fn write_repo_record_atomic(repo_dir: &Path, record: &RepoRecord) -> Result<()> {
    write_json_atomic(&repo_record_path(repo_dir), record)
}

/// Reads `repo.json` in `repo_dir`, or `None` if there isn't one yet.
///
/// # Errors
///
/// [`Error::Io`] if it can't be read, and [`Error::Json`] if it doesn't parse.
pub fn read_repo_record(repo_dir: &Path) -> Result<Option<RepoRecord>> {
    let path = repo_record_path(repo_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&path)(e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(Error::json(&path))
}

/// The ids of the tasks in `repo_dir`: the names of the directories under `tasks/`, sorted.
/// A repo with no `tasks/` directory has none.
///
/// # Errors
///
/// [`Error::Io`] if `tasks/` can't be listed.
pub fn task_ids(repo_dir: &Path) -> Result<Vec<String>> {
    let dir = repo_dir.join("tasks");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&dir)(e)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::io(&dir))?;
        if entry.file_type().map_err(Error::io(entry.path()))?.is_dir() {
            ids.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    ids.sort();
    Ok(ids)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut json = serde_json::to_vec_pretty(value).map_err(Error::json(path))?;
    json.push(b'\n');

    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));

    let write = |tmp: &PathBuf| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(tmp, path)?;
        // Make the rename itself durable.
        File::open(dir)?.sync_all()
    };
    write(&tmp).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(path)(e)
    })
}
