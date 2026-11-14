//! `conductor spawn <task>`: gives a briefed task its worktree and its worker, an hc terminal
//! running the worker's Claude session in a tab of its own in the conductor's Herdr space, then
//! records them in `task.json`.
//!
//! The task moves `briefed → spawning → working`. If a step fails it moves to `failed` instead,
//! with the error in a `status` event. Either way `task.json` is written once at the end, and it
//! names only what was actually created, so teardown knows what to remove.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::Args;
use conductor_state::{
    Event, EventPayload, Phase, StatusVerb, Task, append_event, brief_path, read_repo_record,
    read_task,
};
use jiff::Timestamp;
use serde::Deserialize;

use crate::context::{Ctx, Target};
use crate::hc;
use crate::runner::{Cmd, Runner, System, Tools, check, check_wt, failure, parse_json};
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
        &Tools::from_env(),
        hc::TAB_TIMEOUT,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// What the steps created so far, recorded in `task.json` whether or not they all succeed.
#[derive(Default)]
struct Created {
    branch: Option<String>,
    worktree_path: Option<PathBuf>,
    terminal_id: Option<String>,
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
    tab_timeout: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
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
    if let Some(terminal) = &task.terminal_id {
        bail!("{id} already has a worker in terminal {terminal}");
    }

    let root = &ctx.repo.root;
    let branch = format!("{}{id}", ctx.settings.branch_prefix);
    let branch_exists = branch_exists(runner, tools, root, &branch)?;
    if branch_exists && !args.reuse_branch {
        bail!("branch {branch} already exists; pass --reuse-branch to spawn on it");
    }
    let space = conductor_space(ctx)?;
    let session = ctx.worker_session(id);
    let brief = brief_path(&task_dir);
    let worker = worker_command(ctx, tools, id, &session, &Launch::Brief(&brief));
    let wt = wt_switch(tools, root, &branch, !branch_exists);

    if args.dry_run {
        writeln!(out, "{}", wt.display())?;
        let run = hc::run(tools, Path::new("<path>"), space.as_deref(), &worker);
        writeln!(out, "{}", run.display())?;
        return Ok(());
    }

    move_phase(&task_dir, &mut task, Phase::Spawning)?;
    let mut created = Created::default();
    let result = run_steps(
        runner,
        tools,
        root,
        &wt,
        space.as_deref(),
        &worker,
        &mut created,
    );
    task.branch = created.branch;
    task.worktree_path = created.worktree_path;
    task.terminal_id = created.terminal_id;

    let terminal = match result {
        Ok(terminal) => terminal,
        Err(e) => {
            move_phase(&task_dir, &mut task, Phase::Failed)?;
            let event = Event {
                ts: Timestamp::now(),
                task: id.to_owned(),
                session_id: None,
                payload: EventPayload::Status {
                    verb: StatusVerb::Failed,
                    text: Some(format!("spawn failed: {e:#}")),
                    pr_url: None,
                },
            };
            append_event(&task_dir, &event)?;
            return Err(e.context(format!("cannot spawn {id}; it is now failed")));
        }
    };

    task.session_name = Some(session);
    move_phase(&task_dir, &mut task, Phase::Working)?;
    show_worker(
        runner,
        tools,
        id,
        &terminal,
        space.is_some(),
        tab_timeout,
        err,
    )?;
    print_spawned(out, &task)?;
    Ok(())
}

/// The Herdr space the conductor runs in, as its `SessionStart` hook recorded it in
/// `repo.json`. Workers are started there.
pub fn conductor_space(ctx: &Ctx) -> anyhow::Result<Option<String>> {
    Ok(read_repo_record(&ctx.repo_dir())?.and_then(|record| record.space_id))
}

/// Names the tab Herdr shows the worker in after its task, or, with no space to show it in,
/// says how to watch it instead. Either way a failure is only a warning: the worker is running.
pub fn show_worker(
    runner: &dyn Runner,
    tools: &Tools,
    id: &str,
    terminal: &str,
    in_space: bool,
    tab_timeout: Duration,
    err: &mut dyn Write,
) -> anyhow::Result<()> {
    if !in_space {
        writeln!(
            err,
            "conductor: warning: no Herdr space is recorded for this repo (run the conductor \
             with `conductor start` in Herdr), so {id}'s worker has no tab; watch it with \
             `hc attach {terminal}`"
        )?;
    } else if let Err(e) = hc::name_tab(runner, tools, terminal, id, tab_timeout) {
        writeln!(err, "conductor: warning: cannot name {id}'s tab: {e:#}")?;
    }
    Ok(())
}

/// Creates the worktree and starts the worker's terminal in it, filling in `created` as each
/// thing comes into existence. Returns the terminal's address.
fn run_steps(
    runner: &dyn Runner,
    tools: &Tools,
    root: &Path,
    wt: &Cmd,
    space: Option<&str>,
    worker: &[String],
    created: &mut Created,
) -> anyhow::Result<String> {
    let output = check_wt("wt switch", runner.run(wt)?, root)?;
    let switch: WtSwitch = parse_json(&output.stdout).context("unexpected `wt switch` output")?;
    created.branch = Some(switch.branch);
    created.worktree_path = Some(switch.path.clone());

    let run = hc::run(tools, &switch.path, space, worker);
    let output = check("hc run", runner.run(&run)?)?;
    let terminal = hc::parse_run(&output.stdout)?;
    created.terminal_id = Some(terminal.clone());
    Ok(terminal)
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

/// What a worker's Claude session starts with.
pub enum Launch<'a> {
    /// A new session, told to read the brief at this path.
    Brief(&'a Path),
    /// An existing session, resumed by its id.
    Resume(&'a str),
}

/// The program and arguments that start the worker. `hc run` runs them directly, with no shell
/// in between, in hc's environment. They go through `env` to set the variables the worker's
/// hooks and `conductor` calls need, and `PATH` if given (see [`Tools::path`]).
pub fn worker_command(
    ctx: &Ctx,
    tools: &Tools,
    id: &str,
    session: &str,
    launch: &Launch,
) -> Vec<String> {
    let mut words = vec!["env".to_owned()];
    if let Some(path) = &tools.path {
        words.push(format!("PATH={path}"));
    }
    words.extend([
        format!("CONDUCTOR_TASK_ID={id}"),
        format!("CONDUCTOR_REPO_ID={}", ctx.repo.id),
        format!("CONDUCTOR_HOME={}", ctx.home.root().display()),
        "claude".to_owned(),
    ]);
    if let Launch::Resume(session_id) = launch {
        words.extend(["--resume".to_owned(), (*session_id).to_owned()]);
    }
    words.extend([
        "-n".to_owned(),
        session.to_owned(),
        "--permission-mode".to_owned(),
        "auto".to_owned(),
    ]);
    if let Some(model) = &ctx.settings.model {
        words.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(effort) = &ctx.settings.effort {
        words.extend(["--effort".to_owned(), effort.clone()]);
    }
    if let Launch::Brief(brief) = launch {
        words.push(format!(
            "Read your brief at {} and follow the conductor worker skill.",
            brief.display()
        ));
    }
    words
}

fn print_spawned(out: &mut dyn Write, task: &Task) -> anyhow::Result<()> {
    let show = |v: &Option<String>| v.clone().unwrap_or_default();
    writeln!(out, "{}", task.id)?;
    writeln!(out, "session    {}", show(&task.session_name))?;
    writeln!(out, "branch     {}", show(&task.branch))?;
    if let Some(path) = &task.worktree_path {
        writeln!(out, "worktree   {}", path.display())?;
    }
    writeln!(out, "terminal   {}", show(&task.terminal_id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use conductor_state::{
        Home, RepoIdent, RepoRecord, SchemaV2, Settings, TaskKind, read_events,
        write_repo_record_atomic, write_task_atomic,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::hc::tests::ls;
    use crate::runner::{Fake, WT_NEEDS_APPROVAL};
    use crate::shell;

    const ID: &str = "fix-login-07";
    const ROOT: &str = "/src/my-repo";
    const WORKTREE: &str = "/src/my-repo/.worktrees/conductor-fix-login-07";
    const WT_JSON: &str = r#"{"action":"created","branch":"conductor/fix-login-07","path":"/src/my-repo/.worktrees/conductor-fix-login-07","created_branch":true,"base_branch":"main"}"#;
    const HC_RUN_JSON: &str = r#"{"address":"t7","agent":"env","cwd":"/src/my-repo/.worktrees/conductor-fix-login-07","parent":"t1","state":"live","turn":0}"#;
    const APPROVAL_STDERR: &str = "✗ Cannot prompt for approval in non-interactive environment\n\
                                   ↳ Project hooks in .config/wt.toml need approval\n";

    struct Fixture {
        tmp: TempDir,
        ctx: Ctx,
    }

    /// What a spawn run returned and printed.
    struct Run {
        result: anyhow::Result<()>,
        out: String,
        err: String,
    }

    impl Fixture {
        /// A task in `phase`, in a repo whose conductor runs in space `sp1`.
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
                schema: SchemaV2,
                id: ID.into(),
                title: "Fix login".into(),
                kind: TaskKind::Ship,
                phase,
                branch: None,
                worktree_path: None,
                session_name: None,
                claude_session_id: None,
                terminal_id: None,
                pr_url: None,
                created_at: ts,
                updated_at: ts,
            };
            std::fs::create_dir_all(fixture.task_dir()).unwrap();
            write_task_atomic(&fixture.task_dir(), &task).unwrap();
            fixture.set_space(Some("sp1"));
            fixture
        }

        fn set_space(&self, space: Option<&str>) {
            let record = RepoRecord {
                schema: SchemaV2,
                repo_root: ROOT.into(),
                repo_id: self.ctx.repo.id.clone(),
                conductor_session: "conductor-my-repo".into(),
                terminal_id: Some("t1".into()),
                space_id: space.map(str::to_owned),
            };
            write_repo_record_atomic(&self.ctx.repo_dir(), &record).unwrap();
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

        /// Runs spawn with `extra` arguments.
        fn spawn(&self, fake: &Fake, extra: &[&str]) -> Run {
            use clap::Parser;
            #[derive(Parser)]
            struct Cli {
                #[command(flatten)]
                args: SpawnArgs,
            }
            let argv = ["spawn", ID].iter().chain(extra);
            let cli = Cli::try_parse_from(argv).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let result = spawn(
                &self.ctx,
                &cli.args,
                fake,
                &Tools::default(),
                Duration::ZERO,
                &mut out,
                &mut err,
            );
            Run {
                result,
                out: String::from_utf8(out)
                    .unwrap()
                    .replace(&self.home(), "<home>"),
                err: String::from_utf8(err).unwrap(),
            }
        }

        fn home(&self) -> String {
            self.tmp.path().join("home").to_string_lossy().into_owned()
        }
    }

    fn no_branch(fake: Fake) -> Fake {
        fake.fail(&["git", "-C", ROOT, "rev-parse"], 1, "")
    }

    /// The worker's terminal starts as `t7`, and Herdr shows it in tab `tb9`.
    fn started(fake: Fake) -> Fake {
        fake.ok(&["hc", "run"], HC_RUN_JSON)
            .ok(&["herdr-app", "ls"], &ls(Some("t7")))
            .ok(&["herdr-app", "do", "tab.rename"], r#"{"ok":true}"#)
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
        let fake = started(no_branch(Fake::default()).ok(&["wt"], WT_JSON));
        let run = fx.spawn(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(run.err, "");

        let task = fx.task();
        assert_eq!(task.phase, Phase::Working);
        assert_eq!(task.branch.as_deref(), Some("conductor/fix-login-07"));
        assert_eq!(task.worktree_path.as_deref(), Some(Path::new(WORKTREE)));
        assert_eq!(task.session_name.as_deref(), Some("my-repo-fix-login-07"));
        assert_eq!(task.terminal_id.as_deref(), Some("t7"));
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
            argvs[2][..10],
            [
                "hc",
                "run",
                "--bg",
                "--json",
                "--cwd",
                WORKTREE,
                "--label",
                "herdr.space=sp1",
                "--no-notify",
                "--"
            ]
        );
        assert_eq!(
            argvs[4],
            ["herdr-app", "do", "tab.rename", "--id", "tb9", "--name", ID]
        );
        assert!(argvs.iter().flatten().all(|a| a != "-y"));
        let worker = shell::join(&argvs[2][10..]).replace(&fx.home(), "<home>");
        insta::assert_snapshot!("worker_command", worker);
        insta::assert_snapshot!("spawned_output", run.out);
    }

    #[test]
    fn worker_command_passes_path_on() {
        let fx = Fixture::new(Phase::Briefed);
        let tools = Tools {
            path: Some("/home/me/.cargo/bin:/usr/bin".to_owned()),
            ..Tools::default()
        };
        let words = worker_command(&fx.ctx, &tools, ID, "s", &Launch::Resume("abc"));
        assert_eq!(words[..2], ["env", "PATH=/home/me/.cargo/bin:/usr/bin"]);
        assert!(
            worker_command(&fx.ctx, &Tools::default(), ID, "s", &Launch::Resume("abc"))
                .iter()
                .all(|w| !w.starts_with("PATH="))
        );
    }

    #[test]
    fn worktrunk_approval_gives_hint_and_fails_the_task() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default()).fail(&["wt"], 1, APPROVAL_STDERR);
        let run = fx.spawn(&fake, &[]);
        let err = format!("{:#}", run.result.unwrap_err());
        assert!(err.contains(WT_NEEDS_APPROVAL), "{err}");
        assert!(
            err.contains("Run: wt -C /src/my-repo config approvals add"),
            "{err}"
        );
        assert!(fake.done(), "hc must not run");
        assert_eq!(run.out, "");

        let task = fx.task();
        assert_eq!(task.phase, Phase::Failed);
        assert_eq!(task.branch, None);
        assert_eq!(task.worktree_path, None);
        assert_eq!(task.terminal_id, None);
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
    fn hc_failure_records_only_the_worktree() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default()).ok(&["wt"], WT_JSON).fail(
            &["hc", "run"],
            1,
            "cannot reach herdr-core",
        );
        let run = fx.spawn(&fake, &[]);
        let err = format!("{:#}", run.result.unwrap_err());
        assert!(
            err.contains("`hc run` failed (exit 1): cannot reach herdr-core"),
            "{err}"
        );

        let task = fx.task();
        assert_eq!(task.phase, Phase::Failed);
        assert_eq!(task.branch.as_deref(), Some("conductor/fix-login-07"));
        assert!(task.worktree_path.is_some());
        assert_eq!(task.terminal_id, None);
        assert_eq!(task.session_name, None);
        assert!(failed_status(&fx.events()).contains("cannot reach herdr-core"));
    }

    #[test]
    fn unnamed_tab_is_only_a_warning() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default())
            .ok(&["wt"], WT_JSON)
            .ok(&["hc", "run"], HC_RUN_JSON)
            .fail(&["herdr-app", "ls"], 1, "Herdr isn't running");
        let run = fx.spawn(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(
            run.err,
            "conductor: warning: cannot name fix-login-07's tab: `herdr-app ls` failed (exit 1): \
             Herdr isn't running\n"
        );
        let task = fx.task();
        assert_eq!(task.phase, Phase::Working);
        assert_eq!(task.terminal_id.as_deref(), Some("t7"));
    }

    #[test]
    fn no_space_starts_the_worker_without_a_tab() {
        let fx = Fixture::new(Phase::Briefed);
        fx.set_space(None);
        let fake = no_branch(Fake::default())
            .ok(&["wt"], WT_JSON)
            .ok(&["hc", "run"], HC_RUN_JSON);
        let run = fx.spawn(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done(), "no herdr-app calls");
        assert!(fake.argvs()[2].iter().all(|a| a != "--label"));
        assert!(
            run.err.contains("watch it with `hc attach t7`"),
            "{}",
            run.err
        );
        assert_eq!(fx.task().terminal_id.as_deref(), Some("t7"));
    }

    #[test]
    fn refusals_change_nothing() {
        // Not briefed.
        let fx = Fixture::new(Phase::Intake);
        let fake = Fake::default();
        let run = fx.spawn(&fake, &[]);
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "fix-login-07 is intake; spawn needs briefed"
        );
        assert_eq!(fake.calls(), []);
        assert_eq!(fx.task().phase, Phase::Intake);

        // Already has a terminal.
        let fx = Fixture::new(Phase::Briefed);
        let mut task = fx.task();
        task.terminal_id = Some("t3".into());
        write_task_atomic(&fx.task_dir(), &task).unwrap();
        let run = fx.spawn(&fake, &[]);
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "fix-login-07 already has a worker in terminal t3"
        );
        assert_eq!(fake.calls(), []);

        // The branch exists.
        let fx = Fixture::new(Phase::Briefed);
        let fake = Fake::default().ok(&["git"], "");
        let run = fx.spawn(&fake, &[]);
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "branch conductor/fix-login-07 already exists; pass --reuse-branch to spawn on it"
        );
        assert!(fake.done());
        assert_eq!(fx.task().phase, Phase::Briefed);
        assert_eq!(fx.events(), []);
    }

    #[test]
    fn reuse_branch_switches_without_create() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = started(Fake::default().ok(&["git"], "").ok(
            &["wt"],
            &WT_JSON.replace("\"created_branch\":true", "\"created_branch\":false"),
        ));
        let run = fx.spawn(&fake, &["--reuse-branch"]);
        run.result.unwrap();
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
        let run = fx.spawn(&fake, &["--dry-run"]);
        run.result.unwrap();
        assert!(
            run.out
                .contains(" --permission-mode auto --model opus --effort high "),
            "{}",
            run.out
        );
    }

    #[test]
    fn dry_run_prints_commands_and_writes_nothing() {
        let fx = Fixture::new(Phase::Briefed);
        let fake = no_branch(Fake::default());
        let run = fx.spawn(&fake, &["--dry-run"]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(fake.calls().len(), 1, "only git");
        assert_eq!(fx.task().phase, Phase::Briefed);
        assert_eq!(fx.events(), []);
        insta::assert_snapshot!(run.out);
    }

    #[test]
    fn parse_json_skips_leading_noise() {
        let switch: WtSwitch = parse_json(&format!("hook output\n{WT_JSON}\n")).unwrap();
        assert_eq!(switch.branch, "conductor/fix-login-07");
        let err = parse_json::<WtSwitch>("not json").err().unwrap();
        assert!(err.to_string().contains("\"not json\""), "{err}");
    }
}
