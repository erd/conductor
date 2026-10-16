//! `conductor resume [<task>...]`: reopens the herdr workspace of each live task whose pane is
//! gone, and resumes its worker's Claude session there.
//!
//! Closing the conductor's workspace row in herdr closes every worker workspace with it, but the
//! worktrees, branches and task state survive. Resume opens the worktree again with `herdr
//! worktree open`, runs `claude --resume` with the latest session id from the task's
//! `session_start` events, and records the new workspace and pane in `task.json`. The phase
//! doesn't change.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail};
use clap::Args;
use conductor_state::{
    Phase, Task, events_path, fold, read_events, read_task, task_ids, write_task_atomic,
};
use jiff::Timestamp;

use crate::context::{Ctx, Target};
use crate::deck::list_panes;
use crate::runner::{Runner, System, Tools, check};
use crate::spawn::{Launch, pane_run, parse_worktree_open, worker_command, worktree_open};
use crate::status::snake_case;

/// The phases in which a task has a worker that should be running.
const LIVE: [Phase; 4] = [
    Phase::Working,
    Phase::NeedsDecision,
    Phase::Review,
    Phase::AwaitingMerge,
];

#[derive(Args)]
pub struct ResumeArgs {
    /// The tasks to resume. Defaults to every task in phase `working`, `needs_decision`,
    /// `review` or `awaiting_merge`.
    #[arg(value_name = "TASK")]
    ids: Vec<String>,
    /// Print the commands instead of running them. Writes nothing.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: &ResumeArgs) -> anyhow::Result<()> {
    let ctx = Ctx::from_cwd()?;
    resume(
        &ctx,
        args,
        &System,
        &Tools::resolve(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

fn resume(
    ctx: &Ctx,
    args: &ResumeArgs,
    runner: &dyn Runner,
    tools: &Tools,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<()> {
    let target = Target {
        home: ctx.home.clone(),
        repo_id: ctx.repo.id.clone(),
        task_id: None,
    };
    let ids = if args.ids.is_empty() {
        live_ids(&ctx.repo_dir(), err)?
    } else {
        args.ids.clone()
    };
    if ids.is_empty() {
        writeln!(out, "no tasks to resume")?;
        return Ok(());
    }

    let open: BTreeSet<String> = list_panes(runner, tools, None)?
        .into_iter()
        .map(|pane| pane.pane_id)
        .collect();
    let resumer = Resumer {
        ctx,
        runner,
        tools,
        open: &open,
        dry_run: args.dry_run,
    };
    let mut failed = 0;
    for id in &ids {
        let result = target
            .task_dir_for(id)
            .and_then(|task_dir| resumer.resume(&task_dir, out));
        if let Err(e) = result {
            writeln!(err, "conductor: cannot resume {id}: {e:#}")?;
            failed += 1;
        }
    }
    match failed {
        0 => Ok(()),
        1 => bail!("cannot resume 1 task"),
        n => bail!("cannot resume {n} tasks"),
    }
}

/// The ids of the repo's tasks in a live phase. A task that can't be read is reported on `err`
/// and left out.
fn live_ids(repo_dir: &Path, err: &mut dyn Write) -> anyhow::Result<Vec<String>> {
    let mut ids = Vec::new();
    for id in task_ids(repo_dir)? {
        match read_task(&repo_dir.join("tasks").join(&id)) {
            Ok(task) if LIVE.contains(&task.phase) => ids.push(id),
            Ok(_) => {}
            Err(e) => writeln!(err, "conductor: warning: skipping task {id}: {e}")?,
        }
    }
    Ok(ids)
}

/// What resuming one task needs.
struct Resumer<'a> {
    ctx: &'a Ctx,
    runner: &'a dyn Runner,
    tools: &'a Tools,
    /// The ids of the panes herdr has open.
    open: &'a BTreeSet<String>,
    dry_run: bool,
}

impl Resumer<'_> {
    /// Resumes the task in `task_dir`, unless its pane is still open.
    fn resume(&self, task_dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
        let mut task = read_task(task_dir)?;
        let id = task.id.clone();
        if !LIVE.contains(&task.phase) {
            bail!(
                "it is {}; resume needs working, needs_decision, review or awaiting_merge",
                snake_case(&task.phase)
            );
        }
        if let Some(pane) = task
            .herdr_pane_id
            .as_ref()
            .filter(|p| self.open.contains(*p))
        {
            writeln!(out, "{id} skipped: pane {pane} is still open")?;
            return Ok(());
        }
        let worktree = task
            .worktree_path
            .clone()
            .ok_or_else(|| anyhow!("it has no worktree"))?;
        let events = read_events(&events_path(task_dir), 0)?;
        let session_id = fold(&events.events)
            .session_id
            .ok_or_else(|| anyhow!("no session_start event recorded its Claude session id"))?;
        let name = task
            .session_name
            .clone()
            .unwrap_or_else(|| self.ctx.worker_session(&id));
        let worker = worker_command(self.ctx, &id, &name, &Launch::Resume(&session_id))?;
        let open = worktree_open(
            self.tools,
            &self.ctx.repo.root,
            &worktree.to_string_lossy(),
            &id,
        );

        if self.dry_run {
            writeln!(out, "{}", open.display())?;
            writeln!(out, "{}", pane_run(self.tools, "<pane>", &worker).display())?;
            return Ok(());
        }

        let output = check("herdr worktree open", self.runner.run(&open)?)?;
        let (workspace, pane) = parse_worktree_open(&output.stdout)?;
        // Recorded before the worker starts, so teardown can close the workspace either way.
        task.herdr_workspace_id = Some(workspace);
        task.herdr_pane_id = Some(pane.clone());
        task.session_name = Some(name);
        save(task_dir, &mut task)?;

        let output = self.runner.run(&pane_run(self.tools, &pane, &worker))?;
        check("herdr pane run", output).map_err(|e| {
            e.context(format!(
                "opened pane {pane} but couldn't start the worker there; close the pane and \
                 rerun `conductor resume {id}`"
            ))
        })?;
        writeln!(out, "{id} resumed in pane {pane}")?;
        Ok(())
    }
}

/// Writes `task.json` with the new workspace and pane.
fn save(task_dir: &Path, task: &mut Task) -> anyhow::Result<()> {
    task.updated_at = Timestamp::now();
    write_task_atomic(task_dir, task)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use conductor_state::{
        Event, EventPayload, Home, RepoIdent, SchemaV1, Settings, TaskKind, append_event,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::runner::Fake;

    const ID: &str = "fix-login-07";
    const ROOT: &str = "/src/my-repo";
    const WORKTREE: &str = "/src/my-repo/.worktrees/conductor-fix-login-07";
    const NO_PANES: &str = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w1:p1"}]}}"#;
    const PANE_OPEN: &str =
        r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w9:p1"}]}}"#;
    const HERDR_JSON: &str = r#"{"id":"1","result":{"workspace":{"workspace_id":"w12","label":"fix-login-07"},"root_pane":{"pane_id":"w12:p1"}}}"#;

    struct Fixture {
        tmp: TempDir,
        ctx: Ctx,
    }

    /// What a resume run returned and printed.
    struct Run {
        result: anyhow::Result<()>,
        out: String,
        err: String,
    }

    impl Fixture {
        /// A `working` task `ID` in pane `w9:p1`, whose session `s1` started there.
        fn new() -> Self {
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
            fixture.add(ID, Phase::Working);
            fixture.session_start(ID, "s1");
            fixture
        }

        /// Adds a spawned task `id` in `phase`, with pane `w9:p1` and no events.
        fn add(&self, id: &str, phase: Phase) {
            let ts = "2026-10-14T12:00:00Z".parse().unwrap();
            let task = Task {
                schema: SchemaV1,
                id: id.into(),
                title: "Fix login".into(),
                kind: TaskKind::Ship,
                phase,
                branch: Some(format!("conductor/{id}")),
                worktree_path: Some(format!("/src/my-repo/.worktrees/conductor-{id}").into()),
                session_name: Some(format!("my-repo-{id}")),
                claude_session_id: None,
                herdr_workspace_id: Some("w9".into()),
                herdr_pane_id: Some("w9:p1".into()),
                pr_url: None,
                created_at: ts,
                updated_at: ts,
            };
            std::fs::create_dir_all(self.task_dir(id)).unwrap();
            write_task_atomic(&self.task_dir(id), &task).unwrap();
        }

        fn session_start(&self, id: &str, session_id: &str) {
            let event = Event {
                ts: "2026-10-14T12:00:00Z".parse().unwrap(),
                task: id.into(),
                session_id: Some(session_id.into()),
                payload: EventPayload::SessionStart {
                    source: "startup".into(),
                    herdr_pane_id: Some("w9:p1".into()),
                    herdr_workspace_id: Some("w9".into()),
                    transcript_path: None,
                },
            };
            append_event(&self.task_dir(id), &event).unwrap();
        }

        fn task_dir(&self, id: &str) -> PathBuf {
            self.ctx.home.task_dir(&self.ctx.repo.id, id)
        }

        fn task(&self, id: &str) -> Task {
            read_task(&self.task_dir(id)).unwrap()
        }

        fn set_task(&self, id: &str, change: impl FnOnce(&mut Task)) {
            let mut task = self.task(id);
            change(&mut task);
            write_task_atomic(&self.task_dir(id), &task).unwrap();
        }

        fn resume(&self, fake: &Fake, argv: &[&str]) -> Run {
            use clap::Parser;
            #[derive(Parser)]
            struct Cli {
                #[command(flatten)]
                args: ResumeArgs,
            }
            let cli = Cli::try_parse_from(std::iter::once(&"resume").chain(argv)).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let result = resume(
                &self.ctx,
                &cli.args,
                fake,
                &Tools::default(),
                &mut out,
                &mut err,
            );
            let home = self.tmp.path().join("home").to_string_lossy().into_owned();
            Run {
                result,
                out: String::from_utf8(out).unwrap().replace(&home, "<home>"),
                err: String::from_utf8(err).unwrap(),
            }
        }
    }

    /// `fake` expecting the worker in `id` to be resumed in pane `w12:p1`.
    fn reopen(fake: Fake) -> Fake {
        fake.ok(&["herdr", "worktree", "open"], HERDR_JSON)
            .ok(&["herdr", "pane", "run", "w12:p1"], "")
    }

    #[test]
    fn a_task_whose_pane_is_gone_is_reopened() {
        let fx = Fixture::new();
        let fake = reopen(Fake::default().ok(&["herdr", "pane", "list"], NO_PANES));
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in pane w12:p1\n");
        assert_eq!(run.err, "");

        let argvs = fake.argvs();
        assert_eq!(argvs[0], ["herdr", "pane", "list"]);
        assert_eq!(
            argvs[1],
            [
                "herdr",
                "worktree",
                "open",
                "--cwd",
                ROOT,
                "--path",
                WORKTREE,
                "--label",
                ID,
                "--no-focus",
                "--json"
            ]
        );
        assert_eq!(argvs[2].len(), 5);
        let home = fx.tmp.path().join("home").to_string_lossy().into_owned();
        insta::assert_snapshot!("resume_command", argvs[2][4].replace(&home, "<home>"));

        let task = fx.task(ID);
        assert_eq!(task.phase, Phase::Working);
        assert_eq!(task.herdr_workspace_id.as_deref(), Some("w12"));
        assert_eq!(task.herdr_pane_id.as_deref(), Some("w12:p1"));
        assert_eq!(task.session_name.as_deref(), Some("my-repo-fix-login-07"));
    }

    #[test]
    fn a_task_whose_pane_is_open_is_skipped() {
        let fx = Fixture::new();
        let before = fx.task(ID);
        let fake = Fake::default().ok(&["herdr", "pane", "list"], PANE_OPEN);
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(run.out, "fix-login-07 skipped: pane w9:p1 is still open\n");
        assert_eq!(fx.task(ID), before);
    }

    #[test]
    fn a_task_with_no_session_id_gets_a_clear_error() {
        let fx = Fixture::new();
        // Sorts before `fix-login-07`, and has never started a session.
        fx.add("add-docs-01", Phase::Review);
        let fake = reopen(Fake::default().ok(&["herdr", "pane", "list"], NO_PANES));
        let run = fx.resume(&fake, &[]);
        assert_eq!(run.result.unwrap_err().to_string(), "cannot resume 1 task");
        assert_eq!(
            run.err,
            "conductor: cannot resume add-docs-01: no session_start event recorded its Claude \
             session id\n"
        );
        // The other task still resumes.
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in pane w12:p1\n");
        assert_eq!(
            fx.task("add-docs-01").herdr_pane_id.as_deref(),
            Some("w9:p1")
        );
    }

    #[test]
    fn the_latest_session_is_resumed() {
        let fx = Fixture::new();
        fx.session_start(ID, "s2");
        let fake = reopen(Fake::default().ok(&["herdr", "pane", "list"], NO_PANES));
        fx.resume(&fake, &[]).result.unwrap();
        let line = &fake.argvs()[2][4];
        assert!(line.contains(" claude --resume s2 -n "), "{line}");
    }

    #[test]
    fn model_and_effort_come_from_config() {
        let mut fx = Fixture::new();
        fx.ctx.settings.model = Some("opus".into());
        fx.ctx.settings.effort = Some("high".into());
        let fake = Fake::default().ok(&["herdr", "pane", "list"], NO_PANES);
        let run = fx.resume(&fake, &["--dry-run"]);
        run.result.unwrap();
        assert!(
            run.out
                .ends_with(" --permission-mode auto --model opus --effort high'\n"),
            "{}",
            run.out
        );
    }

    #[test]
    fn only_live_phases_are_resumed() {
        let fx = Fixture::new();
        for (id, phase) in [
            ("a-briefed", Phase::Briefed),
            ("b-torn-down", Phase::TornDown),
            ("c-failed", Phase::Failed),
        ] {
            fx.add(id, phase);
            fx.session_start(id, "s9");
        }
        let fake = reopen(Fake::default().ok(&["herdr", "pane", "list"], NO_PANES));
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in pane w12:p1\n");
    }

    #[test]
    fn a_named_task_must_be_live() {
        let fx = Fixture::new();
        fx.add("a-briefed", Phase::Briefed);
        let fake = reopen(Fake::default().ok(&["herdr", "pane", "list"], NO_PANES));
        let run = fx.resume(&fake, &["a-briefed", ID, "nope"]);
        assert_eq!(run.result.unwrap_err().to_string(), "cannot resume 2 tasks");
        assert_eq!(
            run.err,
            "conductor: cannot resume a-briefed: it is briefed; resume needs working, \
             needs_decision, review or awaiting_merge\n\
             conductor: cannot resume nope: no task `nope` in repo `my-repo-abc123`\n"
        );
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in pane w12:p1\n");
    }

    #[test]
    fn a_task_with_no_pane_is_resumed() {
        let fx = Fixture::new();
        fx.set_task(ID, |task| task.herdr_pane_id = None);
        let fake = reopen(Fake::default().ok(&["herdr", "pane", "list"], PANE_OPEN));
        fx.resume(&fake, &[]).result.unwrap();
        assert!(fake.done());
    }

    #[test]
    fn a_task_with_no_worktree_fails() {
        let fx = Fixture::new();
        fx.set_task(ID, |task| task.worktree_path = None);
        let fake = Fake::default().ok(&["herdr", "pane", "list"], NO_PANES);
        let run = fx.resume(&fake, &[]);
        assert!(run.result.is_err());
        assert_eq!(
            run.err,
            "conductor: cannot resume fix-login-07: it has no worktree\n"
        );
        assert!(fake.done());
    }

    #[test]
    fn pane_run_failure_keeps_the_new_workspace_and_pane() {
        let fx = Fixture::new();
        let fake = Fake::default()
            .ok(&["herdr", "pane", "list"], NO_PANES)
            .ok(&["herdr", "worktree", "open"], HERDR_JSON)
            .fail(&["herdr", "pane", "run"], 2, "pane gone");
        let run = fx.resume(&fake, &[]);
        assert!(run.result.is_err());
        assert_eq!(
            run.err,
            "conductor: cannot resume fix-login-07: opened pane w12:p1 but couldn't start the \
             worker there; close the pane and rerun `conductor resume fix-login-07`: \
             `herdr pane run` failed (exit 2): pane gone\n"
        );
        let task = fx.task(ID);
        assert_eq!(task.herdr_workspace_id.as_deref(), Some("w12"));
        assert_eq!(task.herdr_pane_id.as_deref(), Some("w12:p1"));
    }

    #[test]
    fn worktree_open_failure_changes_nothing() {
        let fx = Fixture::new();
        let before = fx.task(ID);
        let fake = Fake::default()
            .ok(&["herdr", "pane", "list"], NO_PANES)
            .fail(&["herdr", "worktree", "open"], 1, "no such worktree");
        let run = fx.resume(&fake, &[]);
        assert!(run.result.is_err());
        assert!(run.err.contains("no such worktree"), "{}", run.err);
        assert_eq!(fx.task(ID), before);
    }

    #[test]
    fn no_live_tasks_calls_nothing() {
        let fx = Fixture::new();
        fx.set_task(ID, |task| task.phase = Phase::TornDown);
        let fake = Fake::default();
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert_eq!(run.out, "no tasks to resume\n");
        assert_eq!(fake.calls(), []);
    }

    #[test]
    fn dry_run_prints_commands_and_writes_nothing() {
        let fx = Fixture::new();
        let before = fx.task(ID);
        let fake = Fake::default().ok(&["herdr", "pane", "list"], NO_PANES);
        let run = fx.resume(&fake, &["--dry-run"]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(fx.task(ID), before);
        insta::assert_snapshot!(run.out);
    }
}
