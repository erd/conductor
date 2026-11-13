use std::path::PathBuf;

use anyhow::Context as _;
use conductor_state::{
    RepoRecord, SchemaV2, read_repo_record, repo_record_path, write_repo_record_atomic,
};

use crate::context::Ctx;

/// Creates or refreshes `repos/<repo-id>/repo.json` and returns its path.
///
/// Keeps the recorded conductor terminal and Herdr space: only the conductor's `SessionStart`
/// hook sets those, since `init` may run from any terminal.
pub fn run(ctx: &Ctx) -> anyhow::Result<PathBuf> {
    let repo_dir = ctx.repo_dir();
    let existing = read_repo_record(&repo_dir)?;
    let (terminal_id, space_id) =
        existing.map_or((None, None), |record| (record.terminal_id, record.space_id));
    let record = RepoRecord {
        schema: SchemaV2,
        repo_root: ctx.repo.root.clone(),
        repo_id: ctx.repo.id.clone(),
        conductor_session: ctx.conductor_session(),
        terminal_id,
        space_id,
    };
    std::fs::create_dir_all(&repo_dir)
        .with_context(|| format!("cannot create {}", repo_dir.display()))?;
    write_repo_record_atomic(&repo_dir, &record)?;
    Ok(repo_record_path(&repo_dir))
}
