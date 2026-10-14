use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SchemaV1;

/// Per-repo record in `repos/<repo-id>/repo.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoRecord {
    pub schema: SchemaV1,
    /// Canonical path of the main checkout.
    pub repo_root: PathBuf,
    /// `<repo dir name>-<short hash of repo_root>`, e.g. `hamlet-web-v2-3f9a1c`.
    pub repo_id: String,
    /// The conductor's Claude session name, `conductor-<repo>`.
    pub conductor_session: String,
    /// The repo's primary herdr workspace, which worker workspaces fold under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_workspace_id: Option<String>,
    /// The herdr pane the conductor session runs in, recorded by its `SessionStart` hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_pane_id: Option<String>,
}
