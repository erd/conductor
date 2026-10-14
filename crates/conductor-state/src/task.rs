use std::path::PathBuf;

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Phase, SchemaV1};

/// The task record in `task.json`. Written only by `conductor` CLI subcommands, with a
/// write-temp-then-rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    pub schema: SchemaV1,
    /// Short slug from the title plus a counter, e.g. `fix-login-07`.
    pub id: String,
    pub title: String,
    pub kind: TaskKind,
    pub phase: Phase,
    /// Git branch, `<branch_prefix><task-id>`. Set by `conductor spawn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Absolute path of the task's worktree. Set by `conductor spawn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Claude session name, `<repo>-<task-id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What a task produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Produces a change, delivered as a GitHub PR.
    Ship,
    /// Produces `report.md` and no change.
    Scout,
}
