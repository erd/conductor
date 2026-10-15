//! Log files under `logs/` in the state dir, for commands with nowhere else to report errors.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

/// A log is moved to `<name>.1` once it reaches this size.
pub const LOG_LIMIT: u64 = 1 << 20;

/// Appends `line` to `logs_dir/<name>`, first moving a full log to `<name>.1`.
pub fn append_log(logs_dir: &Path, name: &str, line: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() >= LOG_LIMIT) {
        std::fs::rename(&path, logs_dir.join(format!("{name}.1")))?;
    }
    let mut file = OpenOptions::new().append(true).create(true).open(&path)?;
    file.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_rotates_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hook.log");
        let full = vec![b'x'; usize::try_from(LOG_LIMIT).unwrap()];
        std::fs::write(&log, &full).unwrap();

        append_log(dir.path(), "hook.log", "next").unwrap();
        assert_eq!(std::fs::read(dir.path().join("hook.log.1")).unwrap(), full);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "next\n");

        append_log(dir.path(), "hook.log", "again").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "next\nagain\n");
    }
}
