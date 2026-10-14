use std::path::PathBuf;

use anyhow::Context as _;
use conductor_state::{
    RepoRecord, SchemaV1, read_repo_record, repo_record_path, write_repo_record_atomic,
};

use crate::context::{Ctx, env_var};

/// Creates or refreshes `repos/<repo-id>/repo.json` and returns its path.
///
/// The herdr workspace comes from `HERDR_WORKSPACE_ID`; outside herdr the one already
/// recorded is kept, so running `init` from a plain terminal doesn't lose it.
pub fn run(ctx: &Ctx) -> anyhow::Result<PathBuf> {
    let repo_dir = ctx.repo_dir();
    let herdr_workspace_id = match env_var("HERDR_WORKSPACE_ID") {
        Some(id) => Some(id),
        None => read_repo_record(&repo_dir)?.and_then(|record| record.herdr_workspace_id),
    };
    let record = RepoRecord {
        schema: SchemaV1,
        repo_root: ctx.repo.root.clone(),
        repo_id: ctx.repo.id.clone(),
        conductor_session: ctx.conductor_session(),
        herdr_workspace_id,
    };
    std::fs::create_dir_all(&repo_dir)
        .with_context(|| format!("cannot create {}", repo_dir.display()))?;
    write_repo_record_atomic(&repo_dir, &record)?;
    Ok(repo_record_path(&repo_dir))
}
