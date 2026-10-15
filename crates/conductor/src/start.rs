use std::os::unix::process::CommandExt as _;
use std::process::Command;

use anyhow::Context as _;

use crate::context::{Ctx, env_var};
use crate::init;
use crate::shell;

/// The conductor's first prompt: invoke the plugin's conductor skill.
const PROMPT: &str = "/conductor:conductor";

/// Runs `init`, then replaces this process with the conductor's `claude` session. With
/// `dry_run`, prints the command instead and writes nothing.
pub fn run(ctx: &Ctx, dry_run: bool) -> anyhow::Result<()> {
    let home = ctx.home.root().to_string_lossy().into_owned();
    let env = [
        ("CONDUCTOR_ROLE", "conductor"),
        ("CONDUCTOR_REPO_ID", &ctx.repo.id),
        ("CONDUCTOR_HOME", &home),
    ];
    let session = ctx.conductor_session();
    let args = ["-n", &session, "--permission-mode", "auto", PROMPT];

    if env_var("HERDR_PANE_ID").is_none() {
        eprintln!(
            "conductor: warning: not running inside herdr (HERDR_PANE_ID is unset), so workers \
             can't be spawned into folded workspaces and the deck won't open"
        );
    }

    if dry_run {
        let env = env.map(|(key, value)| format!("{key}={}", shell::quote(value)));
        println!("cd {}", shell::quote(&ctx.repo.root.to_string_lossy()));
        println!("{} claude {}", env.join(" "), shell::join(&args));
        return Ok(());
    }

    init::run(ctx)?;
    let err = Command::new("claude")
        .args(args)
        .envs(env)
        .current_dir(&ctx.repo.root)
        .exec();
    Err(err).context("failed to run claude")
}
