mod context;
mod decision;
mod hook;
mod init;
mod report;
mod start;
mod status;
mod table;
mod task;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::context::Ctx;
use crate::decision::DecisionCommand;
use crate::report::ReportArgs;
use crate::task::TaskCommand;

/// Multi-agent orchestrator for Claude Code and herdr.
///
/// Works from anywhere inside a repo's main checkout or one of its worktrees; state lives in
/// `$CONDUCTOR_HOME`, else `~/.config/conductor`.
#[derive(Parser)]
#[command(version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or refresh this repo's `repo.json` and print its path.
    Init,
    /// Start this repo's conductor session in Claude Code.
    ///
    /// Runs `init`, then replaces this process with `claude`, run from the repo's main
    /// checkout.
    Start {
        /// Print the `claude` command and its environment instead of running it. Writes
        /// nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show this repo's tasks: phase, live activity, time in that activity, branch and PR.
    Status {
        /// Print a JSON array instead of a table.
        #[arg(long)]
        json: bool,
        /// Include torn-down tasks.
        #[arg(long)]
        all: bool,
    },
    /// Record a Claude Code hook event (run by the plugin's hooks).
    ///
    /// Does nothing unless `CONDUCTOR_TASK_ID` or `CONDUCTOR_ROLE` is set. Always exits 0 and
    /// prints nothing; errors go to `logs/hook.log` in the state dir.
    Hook(HookArgs),
    /// Report a worker's progress: append a `status` event to the task's log.
    ///
    /// The task comes from `--task`, else `CONDUCTOR_TASK_ID`.
    Report(ReportArgs),
    /// Open, answer or list a task's decisions.
    Decision {
        #[command(subcommand)]
        command: DecisionCommand,
    },
    /// Create tasks, move them between phases, and show them.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Create a task's worktree, herdr workspace and worker session. Not implemented yet.
    Spawn(Stub),
    /// Remove a finished task's worktree, session and workspace. Not implemented yet.
    Teardown(Stub),
    /// Follow every task and wake the conductor when one needs it. Not implemented yet.
    Watch(Stub),
    /// Reopen the workspaces and sessions of live tasks. Not implemented yet.
    Resume(Stub),
}

#[derive(Args)]
struct HookArgs {
    /// The hook event, e.g. `session-start` or `pre-tool-use`.
    event: Option<String>,
    /// Ignored, so an unexpected argument can't make the hook fail.
    #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
    _rest: Vec<String>,
}

/// Arguments for a command that isn't implemented yet; its own issue defines them.
#[derive(Args)]
struct Stub {
    #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
    _args: Vec<String>,
}

fn main() -> ExitCode {
    match run(&Cli::parse().command) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("conductor: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: &Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Init => {
            let path = init::run(&Ctx::from_cwd()?)?;
            println!("{}", path.display());
        }
        Command::Start { dry_run } => start::run(&Ctx::from_cwd()?, *dry_run)?,
        Command::Status { json, all } => status::run(&Ctx::from_cwd()?, *json, *all)?,
        // The plugin's hook shim runs this in every Claude session, so it must never fail.
        Command::Hook(args) => hook::run(args.event.as_deref()),
        Command::Report(args) => report::run(args)?,
        Command::Decision { command } => decision::run(command)?,
        Command::Task { command } => task::run(command)?,
        Command::Spawn(_) => return Ok(not_implemented("spawn")),
        Command::Teardown(_) => return Ok(not_implemented("teardown")),
        Command::Watch(_) => return Ok(not_implemented("watch")),
        Command::Resume(_) => return Ok(not_implemented("resume")),
    }
    Ok(ExitCode::SUCCESS)
}

fn not_implemented(name: &str) -> ExitCode {
    eprintln!("conductor: `conductor {name}` is not implemented yet");
    ExitCode::FAILURE
}
