//! `conductor task new|phase|show|list|abandon`, which the conductor uses to create task records
//! and move them through the lifecycle. A phase change writes `task.json` first and then appends
//! its `phase` event, so whoever sees the event finds the file already updated.

use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand, ValueEnum};
use conductor_state::{
    Event, EventPayload, Phase, SchemaV1, Task, TaskKind, append_event, brief_path, read_task,
    task_ids, write_task_atomic,
};
use jiff::Timestamp;

use crate::context::{Ctx, Target, TargetArgs};
use crate::status::{self, snake_case};

/// Longest slug a task id starts with.
const MAX_SLUG: usize = 24;

#[derive(Subcommand)]
pub enum TaskCommand {
    /// Create a task in phase `intake` with a `brief.md` scaffold, and print its id and the
    /// brief's path.
    New(NewArgs),
    /// Move a task to another phase and log a `phase` event.
    ///
    /// Only moves the lifecycle allows are accepted; `--force` overrides that.
    Phase(PhaseArgs),
    /// Show a task's record.
    Show(ShowArgs),
    /// List this repo's tasks, the same as `conductor status`.
    List(ListArgs),
    /// Move a task to `abandoned`.
    Abandon(AbandonArgs),
}

#[derive(Args)]
pub struct NewArgs {
    /// What the task is, in one line. The task id is a slug of it.
    #[arg(long)]
    title: String,
    /// `ship` produces a change as a PR; `scout` produces `report.md` and no change.
    #[arg(long)]
    kind: Kind,
    /// The request this task comes from, copied verbatim into the brief.
    #[arg(long, value_name = "TEXT")]
    intent: Option<String>,
}

#[derive(Args)]
pub struct PhaseArgs {
    id: String,
    /// The new phase, e.g. `briefed` or `working`, as `conductor status` shows it.
    #[arg(value_parser = parse_phase)]
    phase: Phase,
    /// Make the move even if the lifecycle doesn't allow it, with a warning.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    repo: RepoArg,
}

#[derive(Args)]
pub struct ShowArgs {
    id: String,
    /// Print the task record as JSON, as in `task.json`.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    repo: RepoArg,
}

#[derive(Args)]
pub struct ListArgs {
    /// Print a JSON array instead of a table.
    #[arg(long)]
    json: bool,
    /// Include torn-down tasks.
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
pub struct AbandonArgs {
    id: String,
    #[command(flatten)]
    repo: RepoArg,
}

#[derive(Args)]
struct RepoArg {
    /// The repo id, e.g. `my-repo-3f9a1c`. Defaults to `CONDUCTOR_REPO_ID`, else the repo
    /// containing the current directory.
    #[arg(long, value_name = "ID")]
    repo: Option<String>,
}

impl RepoArg {
    fn target(&self, id: &str) -> anyhow::Result<Target> {
        Target::resolve(&TargetArgs {
            task: Some(id.to_owned()),
            repo: self.repo.clone(),
        })
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Ship,
    Scout,
}

impl From<Kind> for TaskKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Ship => Self::Ship,
            Kind::Scout => Self::Scout,
        }
    }
}

pub fn run(command: &TaskCommand) -> anyhow::Result<()> {
    match command {
        TaskCommand::New(args) => new(&Ctx::from_cwd()?, args),
        TaskCommand::Phase(args) => set_phase(&args.repo, &args.id, args.phase, args.force),
        TaskCommand::Show(args) => show(args),
        TaskCommand::List(args) => status::run(&Ctx::from_cwd()?, args.json, args.all),
        TaskCommand::Abandon(args) => set_phase(&args.repo, &args.id, Phase::Abandoned, false),
    }
}

fn new(ctx: &Ctx, args: &NewArgs) -> anyhow::Result<()> {
    let title = args.title.trim();
    if title.is_empty() {
        bail!("the title is empty");
    }
    let repo_dir = ctx.repo_dir();
    let parent = repo_dir.join("tasks");
    fs::create_dir_all(&parent).with_context(|| format!("cannot create {}", parent.display()))?;

    // `create_dir` fails if the dir exists, so two `task new` racing for a number can't both
    // get it; the loser takes the next one.
    let slug = slug(title);
    let mut counter = next_counter(&task_ids(&repo_dir)?);
    let (id, task_dir) = loop {
        let id = format!("{slug}-{counter:02}");
        let dir = parent.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => break (id, dir),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => counter += 1,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
    };

    let kind = TaskKind::from(args.kind);
    let repo_name = ctx
        .repo
        .root
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy());
    let brief = render_brief(&Brief {
        title,
        id: &id,
        kind,
        intent: args.intent.as_deref(),
        branch: &format!("{}{id}", ctx.settings.branch_prefix),
        worker_session: &format!("{repo_name}-{id}"),
        conductor_session: &ctx.conductor_session(),
    });
    // The brief goes first, so a task that `status` can read always has one.
    let brief_file = brief_path(&task_dir);
    fs::write(&brief_file, brief)
        .with_context(|| format!("cannot write {}", brief_file.display()))?;
    let now = Timestamp::now();
    let task = Task {
        schema: SchemaV1,
        id: id.clone(),
        title: title.to_owned(),
        kind,
        phase: Phase::Intake,
        branch: None,
        worktree_path: None,
        session_name: None,
        claude_session_id: None,
        herdr_workspace_id: None,
        herdr_pane_id: None,
        pr_url: None,
        created_at: now,
        updated_at: now,
    };
    write_task_atomic(&task_dir, &task)?;
    println!("{id}");
    println!("{}", brief_file.display());
    Ok(())
}

fn set_phase(repo: &RepoArg, id: &str, to: Phase, force: bool) -> anyhow::Result<()> {
    let target = repo.target(id)?;
    let task_dir = target.task_dir()?;
    let mut task = read_task(&task_dir)?;
    let from = task.phase;
    if !Phase::can_transition(from, to) {
        let (from_name, to_name) = (snake_case(&from), snake_case(&to));
        if !force {
            let allowed: Vec<_> = Phase::ALL
                .iter()
                .filter(|&&next| Phase::can_transition(from, next))
                .map(snake_case)
                .collect();
            if allowed.is_empty() {
                bail!("cannot move {id} from {from_name} to {to_name}: nothing leaves {from_name}");
            }
            bail!(
                "cannot move {id} from {from_name} to {to_name}; allowed next phases: {}",
                allowed.join(", ")
            );
        }
        eprintln!("conductor: warning: forcing {id} from {from_name} to {to_name}");
    }

    task.phase = to;
    task.updated_at = Timestamp::now();
    write_task_atomic(&task_dir, &task)?;
    let event = Event {
        ts: task.updated_at,
        task: id.to_owned(),
        session_id: None,
        payload: EventPayload::Phase { from, to },
    };
    append_event(&task_dir, &event)?;
    Ok(())
}

fn show(args: &ShowArgs) -> anyhow::Result<()> {
    let target = args.repo.target(&args.id)?;
    let task_dir = target.task_dir()?;
    let task = read_task(&task_dir)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&task)?);
    } else {
        print!("{}", render_show(&task, &task_dir));
    }
    Ok(())
}

/// The task's set fields as aligned `name  value` lines, then the brief's path.
fn render_show(task: &Task, task_dir: &Path) -> String {
    let path = |p: &Path| p.display().to_string();
    let fields = [
        ("id", Some(task.id.clone())),
        ("title", Some(task.title.clone())),
        ("kind", Some(snake_case(&task.kind))),
        ("phase", Some(snake_case(&task.phase))),
        ("branch", task.branch.clone()),
        ("worktree", task.worktree_path.as_deref().map(path)),
        ("session", task.session_name.clone()),
        ("claude_session", task.claude_session_id.clone()),
        ("herdr_workspace", task.herdr_workspace_id.clone()),
        ("herdr_pane", task.herdr_pane_id.clone()),
        ("pr", task.pr_url.clone()),
        ("created", Some(task.created_at.to_string())),
        ("updated", Some(task.updated_at.to_string())),
        ("brief", Some(path(&brief_path(task_dir)))),
    ];
    let set: Vec<_> = fields
        .into_iter()
        .filter_map(|(name, value)| Some((name, value?)))
        .collect();
    let width = set.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, value) in set {
        let _ = writeln!(out, "{name:width$}  {value}");
    }
    out
}

/// `title` lowercased, with each run of anything but ASCII letters and digits turned into one
/// `-`, cut to [`MAX_SLUG`] characters. `task` if nothing is left.
fn slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(MAX_SLUG);
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "task".to_owned()
    } else {
        slug.to_owned()
    }
}

/// One more than the largest counter among `ids`, or 1 if none has one. A counter is the
/// digits after an id's last `-`.
fn next_counter(ids: &[String]) -> u32 {
    ids.iter()
        .filter_map(|id| id.rsplit_once('-')?.1.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n.saturating_add(1))
}

/// Parses a phase by its `snake_case` name.
fn parse_phase(s: &str) -> Result<Phase, String> {
    Phase::ALL
        .into_iter()
        .find(|phase| snake_case(phase) == s)
        .ok_or_else(|| {
            let names: Vec<_> = Phase::ALL.iter().map(snake_case).collect();
            format!("expected one of: {}", names.join(", "))
        })
}

/// What [`render_brief`] fills in.
struct Brief<'a> {
    title: &'a str,
    id: &'a str,
    kind: TaskKind,
    intent: Option<&'a str>,
    branch: &'a str,
    worker_session: &'a str,
    conductor_session: &'a str,
}

/// The `brief.md` scaffold: the sections the conductor fills in, with the kind and the
/// worker's rules already written.
fn render_brief(brief: &Brief) -> String {
    let Brief {
        title,
        id,
        kind,
        intent,
        branch,
        worker_session,
        conductor_session,
    } = brief;
    let intent = intent
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .unwrap_or("<!-- The request, word for word. -->");
    let kind = match kind {
        TaskKind::Ship => {
            "`ship`: produce a change, pushed to the branch below and delivered as a draft PR."
        }
        TaskKind::Scout => "`scout`: write `report.md` next to this brief. Change no code.",
    };
    format!(
        "\
# {title}

## Intent (verbatim)

{intent}

## Spec

<!-- What to build or find out. -->

## Acceptance criteria

<!-- How the conductor will judge the result. -->

## Constraints

<!-- What not to touch, and anything else the worker must respect. -->

## Kind

{kind}

## Worker rules

Follow the `worker` skill.

- Task id: `{id}`
- Branch: `{branch}`
- Your session: `{worker_session}`
- Conductor session: `{conductor_session}`
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        let cases = [
            ("Fix login", "fix-login"),
            ("  Fix: the LOGIN page!! ", "fix-the-login-page"),
            ("Café crème", "caf-cr-me"),
            (
                "Rate-limit the public API endpoints",
                "rate-limit-the-public-ap",
            ),
            // Cut right after a separator: no trailing `-`.
            ("Abcdefghijk abcdefghijk defg", "abcdefghijk-abcdefghijk"),
            ("🚀", "task"),
            ("---", "task"),
        ];
        for (title, expected) in cases {
            assert_eq!(slug(title), expected, "{title}");
            assert!(slug(title).len() <= MAX_SLUG, "{title}");
        }
    }

    #[test]
    fn counters() {
        let ids = |ids: &[&str]| ids.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        assert_eq!(next_counter(&[]), 1);
        assert_eq!(next_counter(&ids(&["fix-login-07", "add-docs-02"])), 8);
        assert_eq!(next_counter(&ids(&["big-99"])), 100);
        assert_eq!(next_counter(&ids(&["no-counter", "x-y"])), 1);
    }

    #[test]
    fn parses_phases() {
        assert_eq!(parse_phase("awaiting_merge"), Ok(Phase::AwaitingMerge));
        let err = parse_phase("done").unwrap_err();
        assert!(err.contains("intake, briefed,"), "{err}");
    }

    #[test]
    fn brief_ship() {
        insta::assert_snapshot!(render_brief(&Brief {
            title: "Fix login",
            id: "fix-login-07",
            kind: TaskKind::Ship,
            intent: Some("The login page 500s when the email has a `+` in it. Fix it."),
            branch: "conductor/fix-login-07",
            worker_session: "web-fix-login-07",
            conductor_session: "conductor-web",
        }));
    }

    #[test]
    fn brief_scout() {
        insta::assert_snapshot!(render_brief(&Brief {
            title: "Audit dependencies",
            id: "audit-deps-01",
            kind: TaskKind::Scout,
            intent: None,
            branch: "conductor/audit-deps-01",
            worker_session: "web-audit-deps-01",
            conductor_session: "conductor-web",
        }));
    }
}
