use std::path::PathBuf;

/// An error from reading or writing conductor's state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{}: {source}", path.display())]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// `git` failed, or the directory isn't inside a git repo.
    #[error("git in {}: {message}", dir.display())]
    Git { dir: PathBuf, message: String },
    /// Neither `CONDUCTOR_HOME` nor `HOME` is set.
    #[error("cannot find conductor's home: neither CONDUCTOR_HOME nor HOME is set")]
    NoHome,
    /// An event line is longer than [`MAX_EVENT_LINE`](crate::MAX_EVENT_LINE) even after
    /// truncating its text fields.
    #[error(
        "event line is {len} bytes, over the {} byte limit",
        crate::MAX_EVENT_LINE
    )]
    EventTooLarge { len: usize },
    /// A read started past the end of an append-only log, so the file was truncated or
    /// replaced.
    #[error("{}: offset {offset} is past the end of the file ({len} bytes)", path.display())]
    OffsetPastEnd {
        path: PathBuf,
        offset: u64,
        len: u64,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }

    pub(crate) fn json(path: impl Into<PathBuf>) -> impl FnOnce(serde_json::Error) -> Self {
        let path = path.into();
        move |source| Self::Json { path, source }
    }
}
