//! `conductor spawn <task>`: gives a briefed task its worktree, its herdr workspace (folded under
//! the conductor's) and its worker session, then records them in `task.json`.
//!
//! The task moves `briefed → spawning → working`. If a step fails it moves to `failed` instead,
//! with the error in a `status` event. Either way `task.json` is written once at the end, and it
//! names only what was actually created, so teardown knows what to remove.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use clap::Args;
use conductor_state::{
    Event, EventPayload, Phase, StatusVerb, Task, append_event, brief_path, read_task,
};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::context::{Ctx, Target};
use crate::runner::{Cmd, Runner, System, Tools, check, check_wt, failure, parse_json};
use crate::shell;
use crate::status::snake_case;
use crate::task::move_phase;

#[derive(Args)]
pub struct SpawnArgs {
    /// The task to spawn. It must be in phase `briefed`.
    id: String,
    /// Use the task's branch if it already exists, instead of refusing.
    #[arg(long)]
    reuse_branch: bool,
    /// Print the commands instead of running them. Writes nothing.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: &SpawnArgs) -> anyhow::Result<()> {
    let ctx = Ctx::from_cwd()?;
    spawn(
        &ctx,
        args,
        &System,
        &Tools::resolve(),
        &mut std::io::stdout().lock(),
    )
}

/// What the steps created so far, recorded in `task.json` whether or not they all succeed.
#[derive(Default)]
struct Created {
    branch: Option<String>,
    worktree_path: Option<PathBuf>,
    herdr_workspace_id: Option<String>,
    herdr_pane_id: Option<String>,
}

/// The fields of `wt switch --format=json` that spawn uses. It also prints `action`,
/// `created_branch` and `base_branch`.
#[derive(Deserialize)]
struct WtSwitch {
    branch: String,
    path: PathBuf,
}

fn spawn(
    ctx: &Ctx,
    args: &SpawnArgs,
    runner: &dyn Runner,
    tools: &Tools,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let id = args.id.as_str();
    let target = Target {
        home: ctx.home.clone(),
        repo_id: ctx.repo.id.clone(),
        task_id: None,
    };
    let task_dir = target.task_dir_for(id)?;
    let mut task = read_task(&task_dir)?;
    if task.phase != Phase::Briefed {
        bail!("{id} is {}; spawn needs briefed", snake_case(&task.phase));
    }
    if let Some(pane) = &task.herdr_pane_id {
        bail!("{id} already has a worker in herdr pane {pane}");
    }

    let root = &ctx.repo.root;
    let branch = format!("{}{id}", ctx.settings.branch_prefix);
    let branch_exists = branch_exists(runner, tools, root, &branch)?;
    if branch_exists && !args.reuse_branch {
        bail!("branch {branch} already exists; pass --reuse-branch to spawn on it");
    }
    let session = ctx.worker_session(id);
    let worker = worker_command(ctx, id, &session, &brief_path(&task_dir))?;
    let wt = wt_switch(tools, root, &branch, !branch_exists);

    if args.dry_run {
        writeln!(out, "{}", wt.display())?;
        writeln!(
            out,
            "{}",
            worktree_open(tools, root, "<path>", id).display()
        )?;
        writeln!(out, "{}", pane_run(tools, "<pane>", &worker).display())?;
        return Ok(());
    }

    move_phase(&task_dir, &mut task, Phase::Spawning)?;
    let mut created = Created::default();
    let result = run_steps(runner, tools, root, id, &wt, &worker, &mut created);
    task.branch = created.branch;
    task.worktree_path = created.worktree_path;
    task.herdr_workspace_id = created.herdr_workspace_id;
    task.herdr_pane_id = created.herdr_pane_id;

    if let Err(err) = result {
        move_phase(&task_dir, &mut task, Phase::Failed)?;
        let event = Event {
            ts: Timestamp::now(),
            task: id.to_owned(),
            session_id: None,
            payload: EventPayload::Status {
                verb: StatusVerb::Failed,
                text: Some(format!("spawn failed: {err:#}")),
                pr_url: None,
            },
        };
        append_event(&task_dir, &event)?;
        return Err(err.context(format!("cannot spawn {id}; it is now failed")));
    }

    task.session_name = Some(session);
    move_phase(&task_dir, &mut task, Phase::Working)?;
    print_spawned(out, &task)?;
    Ok(())
}

/// Creates the worktree, opens its herdr workspace and starts the worker, filling in `created`
/// as each thing comes into existence.
fn run_steps(
    runner: &dyn Runner,
    tools: &Tools,
    root: &Path,
    id: &str,
    wt: &Cmd,
    worker: &str,
    created: &mut Created,
) -> anyhow::Result<()> {
    let output = check_wt("wt switch", runner.run(wt)?, root)?;
    let switch: WtSwitch = parse_json(&output.stdout).context("unexpected `wt switch` output")?;
    created.branch = Some(switch.branch);
    created.worktree_path = Some(switch.path.clone());

    let open = worktree_open(tools, root, &switch.path.to_string_lossy(), id);
    let output = check("herdr worktree open", runner.run(&open)?)?;
    let json: Value =
        parse_json(&output.stdout).context("unexpected `herdr worktree open` output")?;
    let field = |pointer: &str| {
        json.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("`herdr worktree open` printed no {pointer}"))
    };
    created.herdr_workspace_id = Some(field("/result/workspace/workspace_id")?);
    let pane = field("/result/root_pane/pane_id")?;
    created.herdr_pane_id = Some(pane.clone());

    check(
        "herdr pane run",
        runner.run(&pane_run(tools, &pane, worker))?,
    )?;
    Ok(())
}

/// Whether `refs/heads/<branch>` exists in the repo at `root`.
fn branch_exists(
    runner: &dyn Runner,
    tools: &Tools,
    root: &Path,
    branch: &str,
) -> anyhow::Result<bool> {
    let cmd = Cmd::new(&tools.git)
        .arg("-C")
        .arg(root.to_string_lossy())
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"));
    let output = runner.run(&cmd)?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure("git rev-parse", &output)),
    }
}

/// `wt switch` for `branch`, creating it if `create`. Never `-y`: approving project hooks is
/// left to the user.
fn wt_switch(tools: &Tools, root: &Path, branch: &str, create: bool) -> Cmd {
    let cmd = Cmd::new(&tools.wt)
        .arg("-C")
        .arg(root.to_string_lossy())
        .arg("switch");
    let cmd = if create { cmd.arg("--create") } else { cmd };
    cmd.arg(branch).args(["--no-cd", "--format=json"])
}

/// `herdr worktree open` for the worktree at `path`. herdr refuses a linked worktree as `--cwd`,
/// so this always passes the main checkout; that also folds the new workspace under the
/// conductor's.
fn worktree_open(tools: &Tools, root: &Path, path: &str, id: &str) -> Cmd {
    Cmd::new(&tools.herdr)
        .args(["worktree", "open", "--cwd"])
        .arg(root.to_string_lossy())
        .args(["--path", path, "--label", id, "--no-focus", "--json"])
}

/// `herdr pane run`, which types `line` into `pane`'s shell.
fn pane_run(tools: &Tools, pane: &str, line: &str) -> Cmd {
    Cmd::new(&tools.herdr).args(["pane", "run", pane, line])
}

/// The shell line that starts the worker. It goes through `env` so it reads the same in bash,
/// zsh and fish, and it sets the variables the worker's hooks and `conductor` calls need.
fn worker_command(ctx: &Ctx, id: &str, session: &str, brief: &Path) -> anyhow::Result<String> {
    let prompt = format!(
        "Read your brief at {} and follow the conductor worker skill.",
        brief.display()
    );
    let mut words = vec![
        "env".to_owned(),
        format!("CONDUCTOR_TASK_ID={id}"),
        format!("CONDUCTOR_REPO_ID={}", ctx.repo.id),
        format!("CONDUCTOR_HOME={}", ctx.home.root().display()),
        "claude".to_owned(),
        "-n".to_owned(),
        session.to_owned(),
        "--permission-mode".to_owned(),
        "auto".to_owned(),
    ];
    if let Some(model) = &ctx.settings.model {
        words.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(effort) = &ctx.settings.effort {
        words.extend(["--effort".to_owned(), effort.clone()]);
    }
    words.push(prompt);
    for word in &words {
        shell::check_typeable(word)?;
    }
    Ok(shell::join(&words))
}

fn print_spawned(out: &mut dyn Write, task: &Task) -> anyhow::Result<()> {
    let show = |v: &Option<String>| v.clone().unwrap_or_default();
    writeln!(out, "{}", task.id)?;
    writeln!(out, "session    {}", show(&task.session_name))?;
    writeln!(out, "branch     {}", show(&task.branch))?;
    if let Some(path) = &task.worktree_path {
        writeln!(out, "worktree   {}", path.display())?;
    }
    writeln!(out, "workspace  {}", show(&task.herdr_workspace_id))?;
    writeln!(out, "pane       {}", show(&task.herdr_pane_id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use conductor_state::{
        Home, RepoIdent, SchemaV1, Settings, TaskKind, read_events, write_task_atomic,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::runner::{Fake, WT_NEEDS_APPROVAL};

    const ID: &str = "fix-login-07";
    const ROOT: &str = "/src/my-repo";
    const WT_JSON: &str = r#"{"action":"created","branch":"conductor/fix-login-07","path":"/src/my-repo/.worktrees/conductor-fix-login-07","created_branch":true,"base_branch":"main"}"#;
    const HERDR_JSON: &str = r#"{"id":"1","result":{"workspace":{"workspace_id":"w9","label":"fix-login-07"},"root_pane":{"pane_id":"w9:p1"}}}"#;
    const APPROVAL_STDERR: &str = "✗ Cannot prompt for approval in non-interactive environment\n\
                                   ↳ Project hooks in .config/wt.toml need approval\n";

    struct Fixture {
        tmp: TempDir,
        ctx: Ctx,
    }

    impl Fixture {
        fn new(phase: Phase) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = Ctx {
                home: Home::new(tmp.path().join("home")),
                repo: RepoIdent {
                    root: ROOT.into(),
                    id: "my-repo-abc123".into(),
                },
                settings: Settings::default(),
            };
            let fixture = Self { tmp, ctx };
            let ts = "2026-10-14T12:00:00Z".parse().unwrap();
            let task = Task {
                schema: SchemaV1,
                id: ID.into(),
                title: "Fix login".into(),
                kind: TaskKind::Ship,
                phase,
                branch: None,
                worktree_path: None,
                session_name: None,
                claude_session_id: None,
                herdr_workspace_id: None,
                herdr_pane_id: None,
                pr_url: None,
                created_at: ts,
                updated_at: ts,
            };
            std::fs::create_dir_all(fixture.task_dir()).unwrap();
            write_task_atomic(&fixture.task_dir(), &task).unwrap();
            fixture
        }

        fn task_dir(&self) -> PathBuf {
            self.ctx.home.task_dir(&self.ctx.repo.id, ID)
        }

        fn task(&self) -> Task {
            read_task(&self.task_dir()).unwrap()
        }

        fn events(&self) -> Vec<EventPayload> {
            let path = conductor_state::events_path(&self.task_dir());
            if !path.exists() {
                return Vec::new();
            }
            read_events(&path, 0)
                .unwrap()
                .events
                .into_iter()
                .map(|e| e.payload)
                .collect()
        }

        /// Runs spawn with `args`, returning its result and what it printed.
        fn spawn(&self, fake: &Fake, extra: &[&str]) -> (anyhow::Result<()>, String) {
            use clap::Parser;
            #[derive(Parser)]
            struct Cli {
                #[command(flatten)]
                args: SpawnArgs,
            }
            let argv = ["spawn", ID].iter().chain(extra);
            let cli = Cli::try_parse_from(argv).unwrap();
            let mut out = Vec::new();
            let result = spawn(&self.ctx, &cli.args, fake, &Tools::default(), &mut out);
            let out = String::from_utf8(out).unwrap();
            (result, out.replace(&self.home(), "<home>"))
        }

        fn home(&self) -> String {
            self.tmp.path().join("home").to_string_lossy().into_owned()
        }
    }

    fn no_branch(fake: Fake) -> Fake {
        fake.fail(&["git", "-C", ROOT, "rev-parse"], 1, "")
    }

    fn phases(events: &[EventPayload]) -> Vec<(Phase, Phase)> {
        events
            .iter()
            .filter_map(|e| match e {
                EventPayload::Phase { from, to } => Some((*from, *to)),
                _ => None,
            })
            .collect()
    }

    fn failed_status(events: &[EventPayload]) -> String {
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                EventPayload::Status {
                    verb: StatusVerb::Failed,
                    text,
                    ..
                } => text.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1, "{events:?}");
        texts[0].clone()
    }

    #[test]
    fn happy_path_records_every_field() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default())
            .ok(&["wt"], WT_JSON)
            .ok(&["herdr", "worktree", "open"], HERDR_JSON)
            .ok(&["herdr", "pane", "run"], "");
        let (result, out) = fx.spawn(&fake, &[]);
        result.unwrap();
        assert!(fake.done());

        let task = fx.task();
        assert_eq!(task.phase, Phase::Working);
        assert_eq!(task.branch.as_deref(), Some("conductor/fix-login-07"));
        assert_eq!(
            task.worktree_path.as_deref(),
            Some(Path::new("/src/my-repo/.worktrees/conductor-fix-login-07"))
        );
        assert_eq!(task.session_name.as_deref(), Some("my-repo-fix-login-07"));
        assert_eq!(task.herdr_workspace_id.as_deref(), Some("w9"));
        assert_eq!(task.herdr_pane_id.as_deref(), Some("w9:p1"));
        assert_eq!(
            phases(&fx.events()),
            [
                (Phase::Briefed, Phase::Spawning),
                (Phase::Spawning, Phase::Working)
            ]
        );

        let argvs = fake.argvs();
        assert_eq!(
            argvs[1],
            [
                "wt",
                "-C",
                ROOT,
                "switch",
                "--create",
                "conductor/fix-login-07",
                "--no-cd",
                "--format=json"
            ]
        );
        assert_eq!(
            argvs[2],
            [
                "herdr",
                "worktree",
                "open",
                "--cwd",
                ROOT,
                "--path",
                "/src/my-repo/.worktrees/conductor-fix-login-07",
                "--label",
                ID,
                "--no-focus",
                "--json"
            ]
        );
        assert_eq!(argvs[3][..4], ["herdr", "pane", "run", "w9:p1"]);
        assert_eq!(argvs[3].len(), 5);
        assert!(argvs.iter().flatten().all(|a| a != "-y"));
        insta::assert_snapshot!("worker_command", argvs[3][4].replace(&fx.home(), "<home>"));
        insta::assert_snapshot!("spawned_output", out);
    }

    #[test]
    fn worktrunk_approval_gives_hint_and_fails_the_task() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default()).fail(&["wt"], 1, APPROVAL_STDERR);
        let (result, out) = fx.spawn(&fake, &[]);
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains(WT_NEEDS_APPROVAL), "{err}");
        assert!(
            err.contains("Run: wt -C /src/my-repo config approvals add"),
            "{err}"
        );
        assert!(fake.done(), "herdr must not run");
        assert_eq!(out, "");

        let task = fx.task();
        assert_eq!(task.phase, Phase::Failed);
        assert_eq!(task.branch, None);
        assert_eq!(task.worktree_path, None);
        assert_eq!(task.herdr_pane_id, None);
        let events = fx.events();
        assert_eq!(
            phases(&events),
            [
                (Phase::Briefed, Phase::Spawning),
                (Phase::Spawning, Phase::Failed)
            ]
        );
        let text = failed_status(&events);
        assert!(text.contains(WT_NEEDS_APPROVAL), "{text}");
        assert!(text.contains("config approvals add"), "{text}");
    }

    #[test]
    fn herdr_failure_records_only_the_worktree() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default()).ok(&["wt"], WT_JSON).fail(
            &["herdr", "worktree", "open"],
            1,
            "error: no such workspace",
        );
        let (result, _) = fx.spawn(&fake, &[]);
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("`herdr worktree open` failed (exit 1): error: no such workspace"),
            "{err}"
        );

        let task = fx.task();
        assert_eq!(task.phase, Phase::Failed);
        assert_eq!(task.branch.as_deref(), Some("conductor/fix-login-07"));
        assert!(task.worktree_path.is_some());
        assert_eq!(task.herdr_workspace_id, None);
        assert_eq!(task.herdr_pane_id, None);
        assert_eq!(task.session_name, None);
        assert!(failed_status(&fx.events()).contains("no such workspace"));
    }

    #[test]
    fn pane_run_failure_records_the_workspace_and_pane() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default())
            .ok(&["wt"], WT_JSON)
            .ok(&["herdr", "worktree", "open"], HERDR_JSON)
            .fail(&["herdr", "pane", "run"], 2, "pane gone");
        let (result, _) = fx.spawn(&fake, &[]);
        assert!(result.is_err());
        let task = fx.task();
        assert_eq!(task.phase, Phase::Failed);
        assert_eq!(task.herdr_workspace_id.as_deref(), Some("w9"));
        assert_eq!(task.herdr_pane_id.as_deref(), Some("w9:p1"));
        assert_eq!(task.session_name, None);
    }

    #[test]
    fn refusals_change_nothing() {
        // Not briefed.
        let fx = Fixture::new(Phase::Intake);
        let fake = Fake::default();
        let (result, _) = fx.spawn(&fake, &[]);
        assert_eq!(
            result.unwrap_err().to_string(),
            "fix-login-07 is intake; spawn needs briefed"
        );
        assert_eq!(fake.calls(), []);
        assert_eq!(fx.task().phase, Phase::Intake);

        // Already has a pane.
        let fx = Fixture::new(Phase::Briefed);
        let mut task = fx.task();
        task.herdr_pane_id = Some("w1:p1".into());
        write_task_atomic(&fx.task_dir(), &task).unwrap();
        let (result, _) = fx.spawn(&fake, &[]);
        assert_eq!(
            result.unwrap_err().to_string(),
            "fix-login-07 already has a worker in herdr pane w1:p1"
        );
        assert_eq!(fake.calls(), []);

        // The branch exists.
        let fx = Fixture::new(Phase::Briefed);
        let fake = Fake::default().ok(&["git"], "");
        let (result, _) = fx.spawn(&fake, &[]);
        assert_eq!(
            result.unwrap_err().to_string(),
            "branch conductor/fix-login-07 already exists; pass --reuse-branch to spawn on it"
        );
        assert!(fake.done());
        assert_eq!(fx.task().phase, Phase::Briefed);
        assert_eq!(fx.events(), []);
    }

    #[test]
    fn reuse_branch_switches_without_create() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = Fake::default()
            .ok(&["git"], "")
            .ok(
                &["wt"],
                &WT_JSON.replace("\"created_branch\":true", "\"created_branch\":false"),
            )
            .ok(&["herdr", "worktree", "open"], HERDR_JSON)
            .ok(&["herdr", "pane", "run"], "");
        let (result, _) = fx.spawn(&fake, &["--reuse-branch"]);
        result.unwrap();
        assert_eq!(
            fake.argvs()[1],
            [
                "wt",
                "-C",
                ROOT,
                "switch",
                "conductor/fix-login-07",
                "--no-cd",
                "--format=json"
            ]
        );
        assert_eq!(fx.task().phase, Phase::Working);
    }

    #[test]
    fn model_and_effort_come_from_config() {
        let mut fx = Fixture::new(Phase::Briefed);
        fx.ctx.settings.model = Some("opus".into());
        fx.ctx.settings.effort = Some("high".into());
        let fake = no_branch(Fake::default());
        let (result, out) = fx.spawn(&fake, &["--dry-run"]);
        result.unwrap();
        assert!(
            out.contains(" --permission-mode auto --model opus --effort high "),
            "{out}"
        );
    }

    #[test]
    fn dry_run_prints_commands_and_writes_nothing() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default());
        let (result, out) = fx.spawn(&fake, &["--dry-run"]);
        result.unwrap();
        assert!(fake.done());
        assert_eq!(fake.calls().len(), 1, "only git");
        assert_eq!(fx.task().phase, Phase::Briefed);
        assert_eq!(fx.events(), []);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn parse_json_skips_leading_noise() {
        let switch: WtSwitch = parse_json(&format!("hook output\n{WT_JSON}\n")).unwrap();
        assert_eq!(switch.branch, "conductor/fix-login-07");
        let err = parse_json::<WtSwitch>("not json").err().unwrap();
        assert!(err.to_string().contains("\"not json\""), "{err}");
    }
}
