//! `conductor teardown <task>`: once a task is finished, pulls the default branch, stops the
//! worker, removes the worktree and branch, closes the herdr workspace, and moves the task to
//! `torn_down` (or `abandoned` with `--abandon`).
//!
//! A ship task is torn down only after GitHub reports its PR merged; nothing is merged locally.
//! Each step clears the `task.json` field naming what it removed as soon as it's gone, so after
//! a failure a rerun picks up where the last one stopped.

use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use clap::Args;
use conductor_state::{
    Activity, EventPayload, Phase, Task, TaskKind, events_path, fold, read_events, read_task,
    write_task_atomic,
};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::context::{Ctx, Target};
use crate::runner::{Cmd, RunOutput, Runner, System, Tools, check, check_wt, failure, parse_json};
use crate::status::snake_case;
use crate::task::move_phase;

/// How long the worker gets to exit after `/exit` before its pane is closed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often to look for the worker's `session_end` while waiting.
const EXIT_POLL: Duration = Duration::from_millis(200);
/// The `wt remove` branch outcome for a branch worktrunk doesn't see as merged.
const RETAINED_UNMERGED: &str = "retained_unmerged";

#[derive(Args)]
pub struct TeardownArgs {
    /// The task to tear down. A ship task's PR must be merged on GitHub; a scout task must be
    /// in `review` or later.
    id: String,
    /// Tear the task down whatever its state, and move it to `abandoned`. Keeps the branch
    /// unless worktrunk sees it merged.
    #[arg(long)]
    abandon: bool,
    /// With `--abandon`, delete the branch even if it isn't merged.
    #[arg(long, requires = "abandon")]
    force_delete_branch: bool,
}

pub fn run(args: &TeardownArgs) -> anyhow::Result<()> {
    let ctx = Ctx::from_cwd()?;
    teardown(
        &ctx,
        args,
        &System,
        &Tools::resolve(),
        EXIT_TIMEOUT,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// The fields of `gh pr view --json state,mergedAt` that teardown uses.
#[derive(Deserialize)]
struct PrView {
    state: String,
}

/// The fields teardown uses from an entry of `wt remove --format=json`, which prints an array
/// with one entry per branch it removed.
#[derive(Deserialize)]
struct WtRemoved {
    branch: String,
    branch_outcome: String,
}

/// What the steps need besides the task.
struct Steps<'a> {
    runner: &'a dyn Runner,
    tools: &'a Tools,
    root: &'a Path,
    task_dir: &'a Path,
    exit_timeout: Duration,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}

fn teardown(
    ctx: &Ctx,
    args: &TeardownArgs,
    runner: &dyn Runner,
    tools: &Tools,
    exit_timeout: Duration,
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
    if task.phase == Phase::TornDown {
        writeln!(out, "{id} is already torn down")?;
        return Ok(());
    }
    let events = read_events(&events_path(&task_dir), 0)?;
    let live = fold(&events.events);

    // A merged PR's branch may be force-deleted: GitHub already has its changes.
    let mut force_delete = args.force_delete_branch;
    if !args.abandon {
        let phase = task.phase;
        let ship = task.kind == TaskKind::Ship;
        // A ship task still awaiting merge first moves to `merged` once GitHub confirms it.
        let reachable = Phase::can_transition(phase, Phase::TornDown)
            || (ship && Phase::can_transition(phase, Phase::Merged));
        if !reachable {
            bail!(
                "{id} is {}; teardown needs review or later (or pass --abandon)",
                snake_case(&phase)
            );
        }
        if ship {
            let url = task.pr_url.clone().or(live.pr_url).ok_or_else(|| {
                anyhow!("{id} has no PR; teardown needs it merged (or pass --abandon)")
            })?;
            let state = pr_state(runner, tools, &url)?;
            if state != "MERGED" {
                bail!("{id}'s PR is {state}; teardown needs it merged (or pass --abandon)");
            }
            force_delete = true;
            if phase != Phase::Merged && Phase::can_transition(phase, Phase::Merged) {
                move_phase(&task_dir, &mut task, Phase::Merged)?;
            }
        }
    }

    let mut steps = Steps {
        runner,
        tools,
        root: &ctx.repo.root,
        task_dir: &task_dir,
        exit_timeout,
        out,
        err,
    };
    let exited = matches!(live.activity, Activity::Exited { .. });
    let result = (|| {
        steps.pull()?;
        steps.stop_worker(&mut task, exited, events.offset)?;
        steps.remove_worktree(&mut task, force_delete)?;
        steps.close_workspace(&mut task)
    })();
    result.with_context(|| {
        format!("cannot tear down {id}; rerun `conductor teardown` to continue")
    })?;

    let to = if args.abandon {
        Phase::Abandoned
    } else {
        Phase::TornDown
    };
    if task.phase != to {
        move_phase(&task_dir, &mut task, to)?;
    }
    writeln!(steps.out, "{id} {}", snake_case(&to))?;
    Ok(())
}

/// The PR's state as `gh` prints it: `OPEN`, `CLOSED` or `MERGED`.
fn pr_state(runner: &dyn Runner, tools: &Tools, url: &str) -> anyhow::Result<String> {
    let cmd = Cmd::new(&tools.gh).args(["pr", "view", url, "--json", "state,mergedAt"]);
    let output = check("gh pr view", runner.run(&cmd)?)?;
    let view: PrView = parse_json(&output.stdout).context("unexpected `gh pr view` output")?;
    Ok(view.state)
}

impl Steps<'_> {
    /// Pulls the default branch in the main checkout if it's checked out there and clean, so
    /// worktrunk can see the task's branch merged. Otherwise fetches, and says why.
    fn pull(&mut self) -> anyhow::Result<()> {
        let get = self.wt().args(["config", "state", "default-branch", "get"]);
        let default = check("wt config state default-branch get", self.runner.run(&get)?)?;
        let default = default.stdout.trim();

        let head = self
            .runner
            .run(&self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
        let reason = match head.code {
            Some(0) if head.stdout.trim() == default => {
                let status = self.git(&["status", "--porcelain"]);
                let status = check("git status", self.runner.run(&status)?)?;
                if status.stdout.trim().is_empty() {
                    let pull = self.git(&["pull", "--ff-only"]);
                    check("git pull", self.runner.run(&pull)?)?;
                    writeln!(self.out, "pulled {default}")?;
                    return Ok(());
                }
                "it has uncommitted changes".to_owned()
            }
            Some(0) => format!("it is on {}, not {default}", head.stdout.trim()),
            Some(1) => format!("its HEAD is detached, not on {default}"),
            _ => return Err(failure("git symbolic-ref", &head)),
        };
        check("git fetch", self.runner.run(&self.git(&["fetch"]))?)?;
        writeln!(
            self.err,
            "conductor: warning: fetched instead of pulling {default} in {}: {reason}",
            self.root.display()
        )?;
        Ok(())
    }

    /// Asks the worker to `/exit`, waits for its `session_end`, and closes its pane if it
    /// doesn't come. `exited` says the session had already ended; `offset` is where the events
    /// read so far end.
    fn stop_worker(&mut self, task: &mut Task, exited: bool, offset: u64) -> anyhow::Result<()> {
        let Some(pane) = task.herdr_pane_id.clone() else {
            return Ok(());
        };
        if !exited && self.send_exit(&pane)? {
            if self.wait_exited(offset)? {
                writeln!(self.out, "stopped the worker in pane {pane}")?;
            } else {
                let close = self.herdr(&["pane", "close", &pane]);
                let output = self.runner.run(&close)?;
                if !not_found(&output) {
                    check("herdr pane close", output)?;
                }
                writeln!(self.out, "closed pane {pane}: the worker didn't exit")?;
            }
        }
        task.herdr_pane_id = None;
        save(self.task_dir, task)
    }

    /// Types `/exit` and Enter into `pane`. False if the pane is gone.
    fn send_exit(&mut self, pane: &str) -> anyhow::Result<bool> {
        let steps = [
            (
                "herdr pane send-text",
                self.herdr(&["pane", "send-text", pane, "/exit"]),
            ),
            (
                "herdr pane send-keys",
                self.herdr(&["pane", "send-keys", pane, "enter"]),
            ),
        ];
        for (what, cmd) in steps {
            let output = self.runner.run(&cmd)?;
            if not_found(&output) {
                return Ok(false);
            }
            check(what, output)?;
        }
        Ok(true)
    }

    /// Whether a `session_end` event shows up after `offset` within the exit timeout.
    fn wait_exited(&self, mut offset: u64) -> anyhow::Result<bool> {
        let path = events_path(self.task_dir);
        let deadline = Instant::now() + self.exit_timeout;
        loop {
            let batch = read_events(&path, offset)?;
            if batch
                .events
                .iter()
                .any(|e| matches!(e.payload, EventPayload::SessionEnd { .. }))
            {
                return Ok(true);
            }
            offset = batch.offset;
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            thread::sleep(EXIT_POLL.min(deadline - now));
        }
    }

    /// Removes the worktree with `wt remove`, which deletes the branch if it sees it merged.
    /// If it doesn't and `force_delete`, removes the branch again with `-D`.
    fn remove_worktree(&mut self, task: &mut Task, force_delete: bool) -> anyhow::Result<()> {
        let (Some(branch), Some(path)) = (task.branch.clone(), task.worktree_path.clone()) else {
            return Ok(());
        };
        let mut outcome = self.wt_remove(&branch, false)?;
        if outcome == RETAINED_UNMERGED && force_delete {
            outcome = self.wt_remove(&branch, true)?;
        }
        match outcome.as_str() {
            "deleted" => writeln!(
                self.out,
                "removed worktree {} and branch {branch}",
                path.display()
            )?,
            "not_attempted" => writeln!(self.out, "removed worktree {}", path.display())?,
            RETAINED_UNMERGED if !force_delete => {
                writeln!(self.out, "removed worktree {}", path.display())?;
                writeln!(
                    self.err,
                    "conductor: warning: kept branch {branch}: worktrunk doesn't see it merged; \
                     pass --abandon --force-delete-branch to delete it"
                )?;
            }
            other => bail!("`wt remove` kept branch {branch} ({other})"),
        }
        task.worktree_path = None;
        save(self.task_dir, task)
    }

    /// Runs `wt remove` for `branch`, with `-D` if `force`, and returns what happened to the
    /// branch.
    fn wt_remove(&mut self, branch: &str, force: bool) -> anyhow::Result<String> {
        let cmd = self
            .wt()
            .args(["remove", branch, "--foreground", "--format=json"]);
        let cmd = if force { cmd.arg("-D") } else { cmd };
        let output = check_wt("wt remove", self.runner.run(&cmd)?, self.root)?;
        let removed: Vec<WtRemoved> =
            parse_json(&output.stdout).context("unexpected `wt remove` output")?;
        removed
            .into_iter()
            .find(|r| r.branch == branch)
            .map(|r| r.branch_outcome)
            .ok_or_else(|| anyhow!("`wt remove` printed no entry for branch {branch}"))
    }

    /// Closes the task's herdr workspace, which may already be gone.
    fn close_workspace(&mut self, task: &mut Task) -> anyhow::Result<()> {
        let Some(workspace) = task.herdr_workspace_id.clone() else {
            return Ok(());
        };
        let output = self
            .runner
            .run(&self.herdr(&["workspace", "close", &workspace]))?;
        if !not_found(&output) {
            check("herdr workspace close", output)?;
        }
        writeln!(self.out, "closed workspace {workspace}")?;
        task.herdr_workspace_id = None;
        save(self.task_dir, task)
    }

    fn git(&self, args: &[&str]) -> Cmd {
        Cmd::new(&self.tools.git)
            .arg("-C")
            .arg(self.root.to_string_lossy())
            .args(args.iter().copied())
    }

    fn wt(&self) -> Cmd {
        Cmd::new(&self.tools.wt)
            .arg("-C")
            .arg(self.root.to_string_lossy())
    }

    fn herdr(&self, args: &[&str]) -> Cmd {
        Cmd::new(&self.tools.herdr).args(args.iter().copied())
    }
}

/// Whether a herdr command failed because its pane or workspace doesn't exist. herdr prints
/// errors as JSON on stderr, e.g. `{"error":{"code":"pane_not_found",…}}`.
fn not_found(output: &RunOutput) -> bool {
    !output.success()
        && serde_json::from_str::<Value>(output.stderr.trim())
            .ok()
            .and_then(|v| v.pointer("/error/code")?.as_str().map(str::to_owned))
            .is_some_and(|code| code.ends_with("_not_found"))
}

/// Writes `task.json` with what a step changed.
fn save(task_dir: &Path, task: &mut Task) -> anyhow::Result<()> {
    task.updated_at = Timestamp::now();
    write_task_atomic(task_dir, task)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use conductor_state::{
        Event, Home, RepoIdent, SchemaV1, Settings, StatusVerb, append_event, write_task_atomic,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::runner::Fake;

    const ID: &str = "fix-login-07";
    const ROOT: &str = "/src/my-repo";
    const BRANCH: &str = "conductor/fix-login-07";
    const WORKTREE: &str = "/src/my-repo/.worktrees/conductor-fix-login-07";
    const PR: &str = "https://github.com/o/my-repo/pull/12";
    const PANE_GONE: &str = r#"{"error":{"code":"pane_not_found","message":"pane w9:p1 not found"},"id":"cli:request"}"#;
    const WORKSPACE_GONE: &str = r#"{"error":{"code":"workspace_not_found","message":"workspace w9 not found"},"id":"cli:request"}"#;

    struct Fixture {
        _tmp: TempDir,
        ctx: Ctx,
        exit_timeout: Duration,
    }

    /// What a teardown run returned and printed.
    struct Run {
        result: anyhow::Result<()>,
        out: String,
        err: String,
    }

    impl Fixture {
        /// A spawned task whose worker reported a PR.
        fn new(kind: TaskKind, phase: Phase) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = Ctx {
                home: Home::new(tmp.path().join("home")),
                repo: RepoIdent {
                    root: ROOT.into(),
                    id: "my-repo-abc123".into(),
                },
                settings: Settings::default(),
            };
            let fixture = Self {
                _tmp: tmp,
                ctx,
                exit_timeout: Duration::ZERO,
            };
            let ts = "2026-10-14T12:00:00Z".parse().unwrap();
            let task = Task {
                schema: SchemaV1,
                id: ID.into(),
                title: "Fix login".into(),
                kind,
                phase,
                branch: Some(BRANCH.into()),
                worktree_path: Some(WORKTREE.into()),
                session_name: Some("my-repo-fix-login-07".into()),
                claude_session_id: None,
                herdr_workspace_id: Some("w9".into()),
                herdr_pane_id: Some("w9:p1".into()),
                pr_url: None,
                created_at: ts,
                updated_at: ts,
            };
            std::fs::create_dir_all(fixture.task_dir()).unwrap();
            write_task_atomic(&fixture.task_dir(), &task).unwrap();
            let pr_url = (kind == TaskKind::Ship).then(|| PR.to_owned());
            fixture.append(EventPayload::Status {
                verb: StatusVerb::Done,
                text: None,
                pr_url,
            });
            fixture
        }

        fn task_dir(&self) -> PathBuf {
            self.ctx.home.task_dir(&self.ctx.repo.id, ID)
        }

        fn task(&self) -> Task {
            read_task(&self.task_dir()).unwrap()
        }

        fn append(&self, payload: EventPayload) {
            append_event(&self.task_dir(), &event(payload)).unwrap();
        }

        fn phases(&self) -> Vec<(Phase, Phase)> {
            read_events(&events_path(&self.task_dir()), 0)
                .unwrap()
                .events
                .into_iter()
                .filter_map(|e| match e.payload {
                    EventPayload::Phase { from, to } => Some((from, to)),
                    _ => None,
                })
                .collect()
        }

        fn teardown(&self, fake: &Fake, extra: &[&str]) -> Run {
            let args = parse(extra).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let result = teardown(
                &self.ctx,
                &args,
                fake,
                &Tools::default(),
                self.exit_timeout,
                &mut out,
                &mut err,
            );
            Run {
                result,
                out: String::from_utf8(out).unwrap(),
                err: String::from_utf8(err).unwrap(),
            }
        }
    }

    fn parse(extra: &[&str]) -> Result<TeardownArgs, clap::Error> {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: TeardownArgs,
        }
        let argv = ["teardown", ID].iter().chain(extra);
        Cli::try_parse_from(argv).map(|cli| cli.args)
    }

    fn event(payload: EventPayload) -> Event {
        Event {
            ts: Timestamp::now(),
            task: ID.into(),
            session_id: None,
            payload,
        }
    }

    fn merged(fake: Fake) -> Fake {
        fake.ok(
            &["gh", "pr", "view", PR],
            r#"{"mergedAt":"2026-10-14T12:00:00Z","state":"MERGED"}"#,
        )
    }

    /// The main checkout's default branch and current branch.
    fn on_branch(fake: Fake, branch: &str) -> Fake {
        fake.ok(
            &["wt", "-C", ROOT, "config", "state", "default-branch"],
            "main\n",
        )
        .ok(&["git", "-C", ROOT, "symbolic-ref"], &format!("{branch}\n"))
    }

    fn pull(fake: Fake) -> Fake {
        on_branch(fake, "main")
            .ok(&["git", "-C", ROOT, "status", "--porcelain"], "")
            .ok(&["git", "-C", ROOT, "pull", "--ff-only"], "")
    }

    /// `/exit` that the worker ignores, so its pane is closed.
    fn stop(fake: Fake) -> Fake {
        fake.ok(&["herdr", "pane", "send-text", "w9:p1", "/exit"], "")
            .ok(&["herdr", "pane", "send-keys", "w9:p1", "enter"], "")
            .ok(&["herdr", "pane", "close", "w9:p1"], "")
    }

    fn remove(fake: Fake, outcome: &str) -> Fake {
        fake.ok(&["wt", "-C", ROOT, "remove", BRANCH], &wt_json(outcome))
    }

    /// `wt remove` once the worktree is already gone, as on a rerun after it removed the
    /// worktree but kept or failed to delete the branch.
    fn remove_branch_only(fake: Fake, outcome: &str) -> Fake {
        fake.ok(
            &["wt", "-C", ROOT, "remove", BRANCH],
            &wt_branch_only_json(outcome),
        )
    }

    /// `wt remove --format=json` output (wt 0.80), pretty-printed as wt prints it.
    fn wt_json(outcome: &str) -> String {
        format!(
            r#"[
  {{
    "branch": "{BRANCH}",
    "branch_checked_out_at": null,
    "branch_outcome": "{outcome}",
    "kind": "worktree",
    "path": "{WORKTREE}"
  }}
]
"#
        )
    }

    fn wt_branch_only_json(outcome: &str) -> String {
        format!(
            r#"[
  {{
    "branch": "{BRANCH}",
    "branch_checked_out_at": null,
    "branch_outcome": "{outcome}",
    "kind": "branch_only",
    "pruned": false
  }}
]
"#
        )
    }

    fn close(fake: Fake) -> Fake {
        fake.ok(&["herdr", "workspace", "close", "w9"], "")
    }

    fn assert_torn_down(fx: &Fixture) {
        let task = fx.task();
        assert_eq!(task.phase, Phase::TornDown);
        assert_eq!(task.herdr_pane_id, None);
        assert_eq!(task.worktree_path, None);
        assert_eq!(task.herdr_workspace_id, None);
        assert_eq!(
            task.branch.as_deref(),
            Some(BRANCH),
            "kept for status --all"
        );
    }

    #[test]
    fn merged_with_clean_default_branch_pulls() {
        let fx = Fixture::new(TaskKind::Ship, Phase::AwaitingMerge);
        let fake = close(remove(stop(pull(merged(Fake::default()))), "deleted"));
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_torn_down(&fx);
        assert_eq!(
            fx.phases(),
            [
                (Phase::AwaitingMerge, Phase::Merged),
                (Phase::Merged, Phase::TornDown)
            ]
        );
        let argvs = fake.argvs();
        assert_eq!(
            argvs[0],
            ["gh", "pr", "view", PR, "--json", "state,mergedAt"]
        );
        assert_eq!(
            argvs[8],
            [
                "wt",
                "-C",
                ROOT,
                "remove",
                BRANCH,
                "--foreground",
                "--format=json"
            ]
        );
        assert_eq!(
            run.out,
            format!(
                "pulled main\n\
                 closed pane w9:p1: the worker didn't exit\n\
                 removed worktree {WORKTREE} and branch {BRANCH}\n\
                 closed workspace w9\n\
                 {ID} torn_down\n"
            )
        );
        assert_eq!(run.err, "");
    }

    #[test]
    fn main_checkout_on_another_branch_fetches_and_warns() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Merged);
        let fake =
            on_branch(merged(Fake::default()), "feat-x").ok(&["git", "-C", ROOT, "fetch"], "");
        let fake = close(remove(stop(fake), "deleted"));
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(
            run.err,
            "conductor: warning: fetched instead of pulling main in /src/my-repo: \
             it is on feat-x, not main\n"
        );
        assert_torn_down(&fx);
        assert_eq!(fx.phases(), [(Phase::Merged, Phase::TornDown)]);
    }

    #[test]
    fn dirty_or_detached_main_checkout_fetches_and_warns() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Merged);
        let fake = on_branch(merged(Fake::default()), "main")
            .ok(&["git", "-C", ROOT, "status"], " M src/x.rs\n")
            .ok(&["git", "-C", ROOT, "fetch"], "");
        let fake = close(remove(stop(fake), "deleted"));
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert!(
            run.err.ends_with(": it has uncommitted changes\n"),
            "{}",
            run.err
        );

        let fx = Fixture::new(TaskKind::Ship, Phase::Merged);
        let fake = merged(Fake::default())
            .ok(&["wt", "-C", ROOT, "config"], "main\n")
            .fail(&["git", "-C", ROOT, "symbolic-ref"], 1, "")
            .ok(&["git", "-C", ROOT, "fetch"], "");
        let fake = close(remove(stop(fake), "deleted"));
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert!(
            run.err.ends_with(": its HEAD is detached, not on main\n"),
            "{}",
            run.err
        );
    }

    #[test]
    fn worktrunk_keeping_a_merged_branch_falls_back_to_force_delete() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Merged);
        let fake = remove(stop(pull(merged(Fake::default()))), RETAINED_UNMERGED);
        let fake = close(remove(fake, "deleted"));
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        let argvs = fake.argvs();
        assert_eq!(argvs[9].last().map(String::as_str), Some("-D"));
        assert_eq!(argvs[8].last().map(String::as_str), Some("--format=json"));
        assert_torn_down(&fx);
    }

    #[test]
    fn unmerged_pr_is_refused() {
        let fx = Fixture::new(TaskKind::Ship, Phase::AwaitingMerge);
        let before = fx.task();
        let fake = Fake::default().ok(&["gh"], r#"{"mergedAt":null,"state":"OPEN"}"#);
        let run = fx.teardown(&fake, &[]);
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "fix-login-07's PR is OPEN; teardown needs it merged (or pass --abandon)"
        );
        assert!(fake.done());
        assert_eq!(fx.task(), before);
        assert_eq!(fx.phases(), []);
    }

    #[test]
    fn refusals_change_nothing() {
        // A ship task still working, even with its PR merged.
        let fx = Fixture::new(TaskKind::Ship, Phase::Working);
        let fake = Fake::default();
        let run = fx.teardown(&fake, &[]);
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "fix-login-07 is working; teardown needs review or later (or pass --abandon)"
        );
        assert_eq!(fake.calls(), []);

        // A scout still working.
        let fx = Fixture::new(TaskKind::Scout, Phase::Working);
        let run = fx.teardown(&fake, &[]);
        assert!(run.result.is_err());
        assert_eq!(fake.calls(), []);
        assert_eq!(fx.task().phase, Phase::Working);

        // A ship task without a PR.
        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let mut task = fx.task();
        task.kind = TaskKind::Ship;
        write_task_atomic(&fx.task_dir(), &task).unwrap();
        let run = fx.teardown(&fake, &[]);
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "fix-login-07 has no PR; teardown needs it merged (or pass --abandon)"
        );
        assert_eq!(fake.calls(), []);
        assert_eq!(fx.phases(), []);

        // --force-delete-branch only goes with --abandon.
        assert!(parse(&["--force-delete-branch"]).is_err());
        assert!(parse(&["--abandon", "--force-delete-branch"]).is_ok());
    }

    #[test]
    fn scout_in_review_tears_down_without_checking_github() {
        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let fake = close(remove(stop(pull(Fake::default())), "deleted"));
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert!(fake.done());
        assert!(fake.argvs().iter().all(|argv| argv[0] != "gh"));
        assert_torn_down(&fx);
        assert_eq!(fx.phases(), [(Phase::Review, Phase::TornDown)]);
    }

    #[test]
    fn abandon_skips_github_and_keeps_an_unmerged_branch() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Working);
        let fake = close(remove(stop(pull(Fake::default())), RETAINED_UNMERGED));
        let run = fx.teardown(&fake, &["--abandon"]);
        run.result.unwrap();
        assert!(fake.done(), "no gh, no -D");
        assert_eq!(
            run.err,
            "conductor: warning: kept branch conductor/fix-login-07: worktrunk doesn't see it \
             merged; pass --abandon --force-delete-branch to delete it\n"
        );
        assert!(
            run.out.ends_with(&format!("{ID} abandoned\n")),
            "{}",
            run.out
        );
        let task = fx.task();
        assert_eq!(task.phase, Phase::Abandoned);
        assert_eq!(task.worktree_path, None);
        assert_eq!(task.herdr_workspace_id, None);
        assert_eq!(fx.phases(), [(Phase::Working, Phase::Abandoned)]);
    }

    #[test]
    fn abandon_with_force_delete_branch_deletes_it() {
        let fx = Fixture::new(TaskKind::Ship, Phase::Abandoned);
        let fake = remove(stop(pull(Fake::default())), RETAINED_UNMERGED);
        let fake = close(remove(fake, "deleted"));
        let run = fx.teardown(&fake, &["--abandon", "--force-delete-branch"]);
        run.result.unwrap();
        assert!(fake.done());
        assert_eq!(fake.argvs()[8].last().map(String::as_str), Some("-D"));
        assert_eq!(run.err, "");
        assert_eq!(fx.task().phase, Phase::Abandoned);
        assert_eq!(fx.phases(), [], "already abandoned");
    }

    #[test]
    fn worker_that_exits_keeps_its_pane() {
        let mut fx = Fixture::new(TaskKind::Scout, Phase::Review);
        fx.exit_timeout = Duration::from_secs(5);
        let fake = pull(Fake::default())
            .ok(&["herdr", "pane", "send-text"], "")
            .ok(&["herdr", "pane", "send-keys"], "");
        let fake = close(remove(fake, "deleted"));
        let task_dir = fx.task_dir();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let end = EventPayload::SessionEnd {
                reason: "prompt_input_exit".into(),
            };
            append_event(&task_dir, &event(end)).unwrap();
        });
        let run = fx.teardown(&fake, &[]);
        worker.join().unwrap();
        run.result.unwrap();
        assert!(fake.done(), "no pane close");
        assert!(
            run.out.contains("stopped the worker in pane w9:p1\n"),
            "{}",
            run.out
        );
        assert_torn_down(&fx);
    }

    #[test]
    fn worker_that_already_exited_gets_no_exit() {
        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        fx.append(EventPayload::SessionEnd {
            reason: "other".into(),
        });
        let fake = close(remove(pull(Fake::default()), "deleted"));
        fx.teardown(&fake, &[]).result.unwrap();
        assert!(fake.done());
        assert_torn_down(&fx);
    }

    #[test]
    fn missing_pane_and_workspace_are_fine() {
        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let fake = pull(Fake::default()).fail(&["herdr", "pane", "send-text"], 1, PANE_GONE);
        let fake =
            remove(fake, "deleted").fail(&["herdr", "workspace", "close"], 1, WORKSPACE_GONE);
        fx.teardown(&fake, &[]).result.unwrap();
        assert!(fake.done());
        assert_torn_down(&fx);

        // The pane goes away during the wait.
        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let fake = pull(Fake::default())
            .ok(&["herdr", "pane", "send-text"], "")
            .ok(&["herdr", "pane", "send-keys"], "")
            .fail(&["herdr", "pane", "close"], 1, PANE_GONE);
        let fake = close(remove(fake, "deleted"));
        fx.teardown(&fake, &[]).result.unwrap();
        assert!(fake.done());
        assert_torn_down(&fx);
    }

    #[test]
    fn other_failures_stop_teardown() {
        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let fake = pull(Fake::default()).fail(&["herdr", "pane", "send-text"], 1, "socket closed");
        let err = format!("{:#}", fx.teardown(&fake, &[]).result.unwrap_err());
        assert_eq!(
            err,
            "cannot tear down fix-login-07; rerun `conductor teardown` to continue: \
             `herdr pane send-text` failed (exit 1): socket closed"
        );

        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let fake = remove(stop(pull(Fake::default())), "retained_checked_out");
        let err = format!("{:#}", fx.teardown(&fake, &[]).result.unwrap_err());
        assert!(
            err.ends_with("`wt remove` kept branch conductor/fix-login-07 (retained_checked_out)"),
            "{err}"
        );
        assert_eq!(
            fx.task().worktree_path.as_deref(),
            Some(Path::new(WORKTREE))
        );

        let fx = Fixture::new(TaskKind::Scout, Phase::Review);
        let fake = stop(pull(Fake::default())).ok(&["wt", "-C", ROOT, "remove", BRANCH], "[]\n");
        let err = format!("{:#}", fx.teardown(&fake, &[]).result.unwrap_err());
        assert!(
            err.ends_with("`wt remove` printed no entry for branch conductor/fix-login-07"),
            "{err}"
        );
    }

    #[test]
    fn already_torn_down_does_nothing() {
        let fx = Fixture::new(TaskKind::Ship, Phase::TornDown);
        let fake = Fake::default();
        let run = fx.teardown(&fake, &[]);
        run.result.unwrap();
        assert_eq!(run.out, "fix-login-07 is already torn down\n");
        assert_eq!(fake.calls(), []);
    }

    /// Each case fails one step, then reruns with only the calls still needed.
    #[test]
    fn retrying_after_a_failure_finishes() {
        fn fail(fake: Fake, prefix: &[&str]) -> Fake {
            fake.fail(prefix, 1, "boom")
        }
        type Script = fn(Fake) -> Fake;
        let cases: [(&str, Script, Script); 5] = [
            (
                "pull",
                |f| {
                    fail(
                        on_branch(f, "main").ok(&["git", "-C", ROOT, "status"], ""),
                        &["git", "-C", ROOT, "pull"],
                    )
                },
                |f| close(remove(stop(pull(f)), "deleted")),
            ),
            (
                "stop",
                |f| fail(pull(f), &["herdr", "pane", "send-text"]),
                |f| close(remove(stop(pull(f)), "deleted")),
            ),
            (
                "remove",
                |f| fail(stop(pull(f)), &["wt", "-C", ROOT, "remove"]),
                |f| close(remove(pull(f), "deleted")),
            ),
            (
                "remove -D",
                |f| {
                    fail(
                        remove(stop(pull(f)), RETAINED_UNMERGED),
                        &["wt", "-C", ROOT, "remove"],
                    )
                },
                |f| {
                    close(remove_branch_only(
                        remove_branch_only(pull(f), RETAINED_UNMERGED),
                        "deleted",
                    ))
                },
            ),
            (
                "close",
                |f| {
                    fail(
                        remove(stop(pull(f)), "deleted"),
                        &["herdr", "workspace", "close"],
                    )
                },
                |f| close(pull(f)),
            ),
        ];
        for (step, first, second) in cases {
            let fx = Fixture::new(TaskKind::Ship, Phase::AwaitingMerge);
            let fake = first(merged(Fake::default()));
            let run = fx.teardown(&fake, &[]);
            let err = format!("{:#}", run.result.unwrap_err());
            assert!(err.contains("boom"), "{step}: {err}");
            assert!(fake.done(), "{step}");
            assert_eq!(fx.task().phase, Phase::Merged, "{step}");

            let fake = second(merged(Fake::default()));
            fx.teardown(&fake, &[]).result.unwrap();
            assert!(fake.done(), "{step}");
            assert_torn_down(&fx);
            assert_eq!(
                fx.phases(),
                [
                    (Phase::AwaitingMerge, Phase::Merged),
                    (Phase::Merged, Phase::TornDown)
                ],
                "{step}"
            );
        }
    }
}
