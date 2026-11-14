//! `conductor resume [<task>...]`: restarts the worker of each live task whose terminal is gone,
//! resuming its Claude session in a new hc terminal in the task's worktree.
//!
//! A worker's terminal is a child of the conductor's, so closing the conductor's tab or space
//! ends every worker, and a reboot loses them too, but the worktrees, branches and task state
//! survive. Resume runs `claude --resume` with the latest session id from the task's
//! `session_start` events in a new terminal, shown as a new tab in the conductor's space, and
//! records the terminal in `task.json`. The phase doesn't change.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail};
use clap::Args;
use conductor_state::{
    Phase, Task, events_path, fold, read_events, read_task, task_ids, write_task_atomic,
};
use jiff::Timestamp;

use crate::context::{Ctx, Target};
use crate::hc::{self, Terminal};
use crate::runner::{Cmd, Runner, System, Tools, check};
use crate::spawn::{Launch, conductor_space, show_worker, worker_command};
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
        &Tools::default(),
        hc::TAB_TIMEOUT,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

fn resume(
    ctx: &Ctx,
    args: &ResumeArgs,
    runner: &dyn Runner,
    tools: &Tools,
    tab_timeout: Duration,
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

    let resumer = Resumer {
        ctx,
        runner,
        tools,
        space: conductor_space(ctx)?,
        tab_timeout,
        dry_run: args.dry_run,
    };
    let mut failed = 0;
    for id in &ids {
        let result = target
            .task_dir_for(id)
            .and_then(|task_dir| resumer.resume(&task_dir, out, err));
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
    /// The conductor's Herdr space, where workers get their tabs.
    space: Option<String>,
    tab_timeout: Duration,
    dry_run: bool,
}

impl Resumer<'_> {
    /// Resumes the task in `task_dir`, unless its worker is still running.
    fn resume(
        &self,
        task_dir: &Path,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> anyhow::Result<()> {
        let mut task = read_task(task_dir)?;
        let id = task.id.clone();
        if !LIVE.contains(&task.phase) {
            bail!(
                "it is {}; resume needs working, needs_decision, review or awaiting_merge",
                snake_case(&task.phase)
            );
        }
        let old = match &task.terminal_id {
            Some(t) => hc::get(self.runner, self.tools, t)?.map(|term| (t.clone(), term)),
            None => None,
        };
        if let Some((t, term)) = &old
            && runs_worker(term, &id)
        {
            writeln!(out, "{id} skipped: terminal {t} is still running")?;
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
        let worker = worker_command(self.ctx, &id, &name, &Launch::Resume(&session_id));
        let run = hc::run(self.tools, &worktree, self.space.as_deref(), &worker);

        if self.dry_run {
            if let Some((t, term)) = &old {
                if term.live() {
                    writeln!(out, "{}", self.hc(&["kill", t]).display())?;
                }
                writeln!(out, "{}", self.hc(&["rm", t]).display())?;
            }
            writeln!(out, "{}", run.display())?;
            return Ok(());
        }

        if let Some((t, term)) = &old {
            self.remove(t, term)?;
            task.terminal_id = None;
            save(task_dir, &mut task)?;
        }
        let output = check("hc run", self.runner.run(&run)?)?;
        let terminal = hc::parse_run(&output.stdout)?;
        task.terminal_id = Some(terminal.clone());
        task.session_name = Some(name);
        save(task_dir, &mut task)?;
        show_worker(
            self.runner,
            self.tools,
            &id,
            &terminal,
            self.space.is_some(),
            self.tab_timeout,
            err,
        )?;
        writeln!(out, "{id} resumed in terminal {terminal}")?;
        Ok(())
    }

    /// Removes the task's old terminal: a finished one, or a live one whose worker was lost and
    /// replaced with a shell, which hc does for a kept terminal after a reboot or a core restart.
    fn remove(&self, terminal: &str, term: &Terminal) -> anyhow::Result<()> {
        if term.live() {
            let output = self.runner.run(&self.hc(&["kill", terminal]))?;
            if !hc::failed_with(&output, hc::NOT_FOUND) && !hc::failed_with(&output, hc::FINISHED) {
                check("hc kill", output)?;
            }
        }
        let output = self.runner.run(&self.hc(&["rm", terminal]))?;
        if !hc::failed_with(&output, hc::NOT_FOUND) {
            check("hc rm", output)?;
        }
        Ok(())
    }

    fn hc(&self, args: &[&str]) -> Cmd {
        Cmd::new(&self.tools.hc).args(args.iter().copied())
    }
}

/// Whether `term` is still running task `id`'s worker, rather than a shell that replaced it.
fn runs_worker(term: &Terminal, id: &str) -> bool {
    let marker = format!("CONDUCTOR_TASK_ID={id}");
    term.live() && term.argv.contains(&marker)
}

/// Writes `task.json` with the new terminal.
fn save(task_dir: &Path, task: &mut Task) -> anyhow::Result<()> {
    task.updated_at = Timestamp::now();
    write_task_atomic(task_dir, task)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use conductor_state::{
        Event, EventPayload, Home, RepoIdent, RepoRecord, SchemaV2, Settings, TaskKind,
        append_event, write_repo_record_atomic,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::hc::tests::{get_json, ls};
    use crate::runner::Fake;
    use crate::shell;

    const ID: &str = "fix-login-07";
    const ROOT: &str = "/src/my-repo";
    const WORKTREE: &str = "/src/my-repo/.worktrees/conductor-fix-login-07";
    const HC_RUN_JSON: &str = r#"{"address":"t7","agent":"env","state":"live","turn":0}"#;
    const GONE: &str = r#"{"error":{"hint":"see `hc ls --all`","kind":"not_found","message":"no terminal \"t3\""}}"#;

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
        /// A `working` task `ID` in terminal `t3`, whose session `s1` started there, in a repo
        /// whose conductor runs in space `sp1`.
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
            let record = RepoRecord {
                schema: SchemaV2,
                repo_root: ROOT.into(),
                repo_id: fixture.ctx.repo.id.clone(),
                conductor_session: "conductor-my-repo".into(),
                terminal_id: Some("t1".into()),
                space_id: Some("sp1".into()),
            };
            write_repo_record_atomic(&fixture.ctx.repo_dir(), &record).unwrap();
            fixture
        }

        /// Adds a spawned task `id` in `phase`, with terminal `t3` and no events.
        fn add(&self, id: &str, phase: Phase) {
            let ts = "2026-10-14T12:00:00Z".parse().unwrap();
            let task = Task {
                schema: SchemaV2,
                id: id.into(),
                title: "Fix login".into(),
                kind: TaskKind::Ship,
                phase,
                branch: Some(format!("conductor/{id}")),
                worktree_path: Some(format!("/src/my-repo/.worktrees/conductor-{id}").into()),
                session_name: Some(format!("my-repo-{id}")),
                claude_session_id: None,
                terminal_id: Some("t3".into()),
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
                    terminal_id: Some("t3".into()),
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

        fn home(&self) -> String {
            self.tmp.path().join("home").to_string_lossy().into_owned()
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
    }

    /// The worker's old terminal `t3` is gone.
    fn gone(fake: Fake) -> Fake {
        fake.fail(&["hc", "get", "t3"], hc::NOT_FOUND, GONE)
    }

    /// The old terminal `t3` is in `state`, running the worker if `worker`, else a shell.
    fn old(fake: Fake, state: &str, worker: bool) -> Fake {
        let argv: &[&str] = if worker {
            &["env", "CONDUCTOR_TASK_ID=fix-login-07", "claude"]
        } else {
            &["/bin/zsh", "-l"]
        };
        fake.ok(&["hc", "get", "t3", "--json"], &get_json(state, argv))
    }

    /// The worker starts again in terminal `t7`, which Herdr shows in tab `tb9`.
    fn restart(fake: Fake) -> Fake {
        fake.ok(&["hc", "run"], HC_RUN_JSON)
            .ok(&["herdr-app", "ls"], &ls(Some("t7")))
            .ok(&["herdr-app", "do", "tab.rename"], r#"{"ok":true}"#)
    }

    #[test]
    fn a_task_whose_terminal_is_gone_is_restarted() {
        let fx = Fixture::new();
        let fake = restart(gone(Fake::default()));
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in terminal t7\n");
        assert_eq!(run.err, "");

        let argvs = fake.argvs();
        assert_eq!(argvs[0], ["hc", "get", "t3", "--json"]);
        assert_eq!(
            argvs[1][..10],
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
        let worker = shell::join(&argvs[1][10..]).replace(&fx.home(), "<home>");
        insta::assert_snapshot!("resume_command", worker);
        assert_eq!(
            argvs[3][..5],
            ["herdr-app", "do", "tab.rename", "--id", "tb9"]
        );

        let task = fx.task(ID);
        assert_eq!(task.phase, Phase::Working);
        assert_eq!(task.terminal_id.as_deref(), Some("t7"));
        assert_eq!(task.session_name.as_deref(), Some("my-repo-fix-login-07"));
    }

    #[test]
    fn a_running_worker_is_skipped() {
        let fx = Fixture::new();
        let before = fx.task(ID);
        let fake = old(Fake::default(), "live", true);
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(
            run.out,
            "fix-login-07 skipped: terminal t3 is still running\n"
        );
        assert_eq!(fx.task(ID), before);
    }

    #[test]
    fn a_finished_terminal_is_removed_first() {
        let fx = Fixture::new();
        let fake = restart(old(Fake::default(), "finished", true).ok(&["hc", "rm", "t3"], ""));
        fx.resume(&fake, &[]).result.unwrap();
        assert!(fake.done());
        assert_eq!(fx.task(ID).terminal_id.as_deref(), Some("t7"));
    }

    #[test]
    fn a_shell_that_replaced_the_worker_is_killed_first() {
        let fx = Fixture::new();
        let fake = old(Fake::default(), "live", false)
            .ok(&["hc", "kill", "t3"], "")
            .ok(&["hc", "rm", "t3"], "");
        let fake = restart(fake);
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in terminal t7\n");
    }

    #[test]
    fn a_task_with_no_session_id_gets_a_clear_error() {
        let fx = Fixture::new();
        // Sorts before `fix-login-07`, and has never started a session.
        fx.add("add-docs-01", Phase::Review);
        let fake = restart(gone(gone(Fake::default())));
        let run = fx.resume(&fake, &[]);
        assert_eq!(run.result.unwrap_err().to_string(), "cannot resume 1 task");
        assert_eq!(
            run.err,
            "conductor: cannot resume add-docs-01: no session_start event recorded its Claude \
             session id\n"
        );
        // The other task still resumes.
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in terminal t7\n");
        assert_eq!(fx.task("add-docs-01").terminal_id.as_deref(), Some("t3"));
    }

    #[test]
    fn the_latest_session_is_resumed() {
        let fx = Fixture::new();
        fx.session_start(ID, "s2");
        let fake = restart(gone(Fake::default()));
        fx.resume(&fake, &[]).result.unwrap();
        let line = shell::join(&fake.argvs()[1]);
        assert!(line.contains(" claude --resume s2 -n "), "{line}");
    }

    #[test]
    fn model_and_effort_come_from_config() {
        let mut fx = Fixture::new();
        fx.ctx.settings.model = Some("opus".into());
        fx.ctx.settings.effort = Some("high".into());
        let fake = gone(Fake::default());
        let run = fx.resume(&fake, &["--dry-run"]);
        run.result.unwrap();
        assert!(
            run.out
                .ends_with(" --permission-mode auto --model opus --effort high\n"),
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
        let fake = restart(gone(Fake::default()));
        let run = fx.resume(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in terminal t7\n");
    }

    #[test]
    fn a_named_task_must_be_live() {
        let fx = Fixture::new();
        fx.add("a-briefed", Phase::Briefed);
        let fake = restart(gone(Fake::default()));
        let run = fx.resume(&fake, &["a-briefed", ID, "nope"]);
        assert_eq!(run.result.unwrap_err().to_string(), "cannot resume 2 tasks");
        assert_eq!(
            run.err,
            "conductor: cannot resume a-briefed: it is briefed; resume needs working, \
             needs_decision, review or awaiting_merge\n\
             conductor: cannot resume nope: no task `nope` in repo `my-repo-abc123`\n"
        );
        assert!(fake.done());
        assert_eq!(run.out, "fix-login-07 resumed in terminal t7\n");
    }

    #[test]
    fn a_task_with_no_terminal_is_resumed() {
        let fx = Fixture::new();
        fx.set_task(ID, |task| task.terminal_id = None);
        let fake = restart(Fake::default());
        fx.resume(&fake, &[]).result.unwrap();
        assert!(fake.done());
    }

    #[test]
    fn a_task_with_no_worktree_fails() {
        let fx = Fixture::new();
        fx.set_task(ID, |task| task.worktree_path = None);
        let fake = gone(Fake::default());
        let run = fx.resume(&fake, &[]);
        assert!(run.result.is_err());
        assert_eq!(
            run.err,
            "conductor: cannot resume fix-login-07: it has no worktree\n"
        );
        assert!(fake.done());
    }

    #[test]
    fn hc_run_failure_leaves_no_terminal() {
        let fx = Fixture::new();
        let fake = old(Fake::default(), "finished", true)
            .ok(&["hc", "rm", "t3"], "")
            .fail(&["hc", "run"], 1, "cannot reach herdr-core");
        let run = fx.resume(&fake, &[]);
        assert!(run.result.is_err());
        assert!(run.err.contains("cannot reach herdr-core"), "{}", run.err);
        let task = fx.task(ID);
        assert_eq!(task.terminal_id, None);
        assert_eq!(task.phase, Phase::Working);
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
        let fake = old(Fake::default(), "live", false);
        let run = fx.resume(&fake, &["--dry-run"]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(fx.task(ID), before);
        insta::assert_snapshot!(run.out);
    }
}
