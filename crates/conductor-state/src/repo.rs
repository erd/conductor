use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SchemaV2;

/// Per-repo record in `repos/<repo-id>/repo.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoRecord {
    pub schema: SchemaV2,
    /// Canonical path of the main checkout.
    pub repo_root: PathBuf,
    /// `<repo dir name>-<short hash of repo_root>`, e.g. `hamlet-web-v2-3f9a1c`.
    pub repo_id: String,
    /// The conductor's Claude session name, `conductor-<repo>`.
    pub conductor_session: String,
    /// The hc terminal the conductor session runs in, recorded by its `SessionStart` hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    /// The Herdr space the conductor's terminal is in, from its `herdr.space` label. Workers
    /// are started there, each in its own tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
}
