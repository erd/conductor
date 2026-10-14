//! Shared serde types for conductor's on-disk state: `task.json`, `events.jsonl`, and
//! `decisions/`. Used by the `conductor` CLI and by `conductor-deck`.

/// The task record stored in `task.json`.
pub mod task {}
