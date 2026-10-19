//! `conductor decision open|answer|list`. A worker opens a decision; the conductor or the deck
//! answers it, or the worker records the answer the user gave in its pane. Each writes
//! `decisions/<key>.json` first and then appends its event, so whoever sees the event finds the
//! file already updated. Answering a task's last open decision moves it from `needs_decision`
//! back to `working`.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand, ValueEnum};
use conductor_state::{
    Answer, AnsweredBy, Decision, DecisionOption, Event, EventPayload, Phase, SchemaV1,
    append_event, decision_path, read_decision, read_decisions, read_task, task_ids,
    write_decision_atomic,
};
use jiff::Timestamp;
use serde::Serialize;

use crate::context::{Target, TargetArgs, env_var};
use crate::status::snake_case;
use crate::table::{self, cut};
use crate::task::move_phase;

/// Longest title the list table shows before cutting it with `…`.
const MAX_TITLE: usize = 50;

#[derive(Subcommand)]
pub enum DecisionCommand {
    /// Open a decision for the task and print its file's path.
    ///
    /// Fails if a decision with the same key is still open; an answered one is replaced.
    Open(OpenArgs),
    /// Answer an open decision. Answering the task's last open one moves it from
    /// `needs_decision` back to `working`.
    ///
    /// A worker may only record an answer the user gave in its pane: `--by user`, for its own
    /// task.
    Answer(AnswerArgs),
    /// List decisions: the task's, or every task's in the repo when no task is given.
    List(ListArgs),
}

#[derive(Args)]
pub struct OpenArgs {
    /// Names the decision within the task: letters, digits, `-` and `_`.
    key: String,
    /// The question, in one line.
    #[arg(long)]
    title: String,
    /// Context for whoever answers.
    #[arg(long, value_name = "TEXT")]
    about: Option<String>,
    /// A choice, as `<value>=<label>` or `<value>=<label>:<hint>`. Repeat for each choice.
    #[arg(long = "option", value_name = "VALUE=LABEL[:HINT]", value_parser = parse_option)]
    options: Vec<DecisionOption>,
    /// The value of the option you recommend.
    #[arg(long, value_name = "VALUE")]
    recommend: Option<String>,
    /// Accept a free-text answer as well as the options.
    #[arg(long)]
    allow_freeform: bool,
    #[command(flatten)]
    target: TargetArgs,
}

#[derive(Args)]
pub struct AnswerArgs {
    key: String,
    /// An option's value, or free text if the decision allows it.
    value: String,
    /// Why, or anything the worker should know.
    #[arg(long, value_name = "TEXT")]
    note: Option<String>,
    /// Who is answering. Defaults to `conductor` in the conductor's session, else `user`.
    #[arg(long, value_name = "WHO")]
    by: Option<By>,
    #[command(flatten)]
    target: TargetArgs,
}

#[derive(Args)]
pub struct ListArgs {
    /// Only decisions not yet answered.
    #[arg(long)]
    open: bool,
    /// Print a JSON array instead of a table.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    target: TargetArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum By {
    Conductor,
    Deck,
    User,
}

impl From<By> for AnsweredBy {
    fn from(by: By) -> Self {
        match by {
            By::Conductor => Self::Conductor,
            By::Deck => Self::Deck,
            By::User => Self::User,
        }
    }
}

pub fn run(command: &DecisionCommand) -> anyhow::Result<()> {
    match command {
        DecisionCommand::Open(args) => open(args),
        DecisionCommand::Answer(args) => answer(args),
        DecisionCommand::List(args) => list(args),
    }
}

fn open(args: &OpenArgs) -> anyhow::Result<()> {
    check_key(&args.key)?;
    let mut seen = HashSet::new();
    for option in &args.options {
        if !seen.insert(option.value.as_str()) {
            bail!("option `{}` is given more than once", option.value);
        }
    }
    if args.options.is_empty() && !args.allow_freeform {
        bail!("give at least one --option, or pass --allow-freeform");
    }
    if let Some(recommend) = &args.recommend
        && !seen.contains(recommend.as_str())
    {
        bail!("--recommend `{recommend}` is not one of the options");
    }

    let target = Target::resolve(&args.target)?;
    let task_dir = target.task_dir()?;
    if let Some(existing) = read_decision(&task_dir, &args.key)?
        && existing.answer.is_none()
    {
        bail!("decision `{}` is already open", args.key);
    }
    let decision = Decision {
        schema: SchemaV1,
        key: args.key.clone(),
        title: args.title.clone(),
        about: args.about.clone().unwrap_or_default(),
        options: args.options.clone(),
        recommend: args.recommend.clone(),
        allow_freeform: args.allow_freeform,
        answer: None,
    };
    write_decision_atomic(&task_dir, &decision)?;
    append(
        &task_dir,
        target.task_id()?,
        EventPayload::DecisionOpen {
            key: args.key.clone(),
        },
    )?;
    println!("{}", decision_path(&task_dir, &args.key).display());
    Ok(())
}

fn answer(args: &AnswerArgs) -> anyhow::Result<()> {
    check_key(&args.key)?;
    let role = env_var("CONDUCTOR_ROLE");
    let worker_task = env_var("CONDUCTOR_TASK_ID").filter(|_| role.as_deref() != Some("conductor"));
    if let Some(own) = &worker_task {
        if args.target.task.as_ref().is_some_and(|task| task != own) {
            bail!("a worker can only answer its own task's decisions");
        }
        if args.by.is_some_and(|by| !matches!(by, By::User)) {
            bail!("a worker can only record the user's answer: use --by user");
        }
    }
    let target = Target::resolve(&args.target)?;
    let task_id = target.task_id()?;
    let task_dir = target.task_dir()?;
    let mut decision = read_decision(&task_dir, &args.key)?
        .with_context(|| format!("no decision `{}` in task `{task_id}`", args.key))?;
    if let Some(answer) = &decision.answer {
        bail!(
            "decision `{}` was already answered: `{}` by {} at {}",
            args.key,
            answer.value,
            snake_case(&answer.answered_by),
            answer.ts,
        );
    }
    if args.value.is_empty() {
        bail!("the answer is empty");
    }
    if !decision.allow_freeform && !decision.options.iter().any(|o| o.value == args.value) {
        let values: Vec<_> = decision.options.iter().map(|o| o.value.as_str()).collect();
        bail!(
            "`{}` is not an option of `{}`; expected one of: {}",
            args.value,
            args.key,
            values.join(", ")
        );
    }

    let by = args.by.map_or_else(
        || {
            if role.as_deref() == Some("conductor") {
                AnsweredBy::Conductor
            } else {
                AnsweredBy::User
            }
        },
        AnsweredBy::from,
    );
    decision.answer = Some(Answer {
        value: args.value.clone(),
        note: args.note.clone().filter(|n| !n.is_empty()),
        answered_by: by,
        ts: Timestamp::now(),
    });
    write_decision_atomic(&task_dir, &decision)?;
    append(
        &task_dir,
        task_id,
        EventPayload::DecisionAnswered {
            key: args.key.clone(),
        },
    )?;
    resume_if_answered(&task_dir)
}

/// Moves the task in `task_dir` from `needs_decision` back to `working` once none of its
/// decisions is open. A missing `task.json` is skipped: the answer is already recorded.
fn resume_if_answered(task_dir: &Path) -> anyhow::Result<()> {
    if read_decisions(task_dir)?.iter().any(|d| d.answer.is_none()) {
        return Ok(());
    }
    let mut task = match read_task(task_dir) {
        Ok(task) => task,
        Err(conductor_state::Error::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(());
        }
        Err(err) => {
            eprintln!("conductor: warning: not moving the task back to working: {err}");
            return Ok(());
        }
    };
    if task.phase == Phase::NeedsDecision {
        move_phase(task_dir, &mut task, Phase::Working)?;
    }
    Ok(())
}

/// One element of `decision list --json`: the decision's own fields plus its task.
#[derive(Serialize)]
struct Listed {
    task: String,
    #[serde(flatten)]
    decision: Decision,
}

fn list(args: &ListArgs) -> anyhow::Result<()> {
    let target = Target::resolve(&args.target)?;
    let mut listed = Vec::new();
    if target.task_id.is_some() {
        let task = target.task_id()?.to_owned();
        for decision in read_decisions(&target.task_dir()?)? {
            listed.push(Listed {
                task: task.clone(),
                decision,
            });
        }
    } else {
        let repo_dir = target.repo_dir()?;
        for task in task_ids(&repo_dir)? {
            // One unreadable task shouldn't hide the others' decisions.
            match read_decisions(&repo_dir.join("tasks").join(&task)) {
                Ok(decisions) => listed.extend(decisions.into_iter().map(|decision| Listed {
                    task: task.clone(),
                    decision,
                })),
                Err(err) => eprintln!("conductor: warning: skipping task {task}: {err}"),
            }
        }
    }
    if args.open {
        listed.retain(|l| l.decision.answer.is_none());
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&listed)?);
    } else if listed.is_empty() {
        let what = if args.open {
            "open decisions"
        } else {
            "decisions"
        };
        println!("no {what}");
    } else {
        let rows: Vec<[String; 5]> = listed
            .iter()
            .map(|Listed { task, decision }| {
                let (state, answer) = match &decision.answer {
                    None => ("open", "-".to_owned()),
                    Some(a) => (
                        "answered",
                        format!("{} ({})", a.value, snake_case(&a.answered_by)),
                    ),
                };
                [
                    task.clone(),
                    decision.key.clone(),
                    state.to_owned(),
                    cut(&decision.title, MAX_TITLE),
                    answer,
                ]
            })
            .collect();
        print!(
            "{}",
            table::render(["TASK", "KEY", "STATE", "TITLE", "ANSWER"], &rows)
        );
    }
    Ok(())
}

fn append(task_dir: &Path, task_id: &str, payload: EventPayload) -> anyhow::Result<()> {
    let event = Event {
        ts: Timestamp::now(),
        task: task_id.to_owned(),
        session_id: None,
        payload,
    };
    append_event(task_dir, &event)?;
    Ok(())
}

/// A key becomes a file name, so it's limited to letters, digits, `-` and `_`, starting with a
/// letter or digit.
fn check_key(key: &str) -> anyhow::Result<()> {
    let ok = key.starts_with(|c: char| c.is_ascii_alphanumeric())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("invalid decision key {key:?}: use letters, digits, `-` and `_`");
    }
    Ok(())
}

/// Parses `<value>=<label>[:<hint>]`.
fn parse_option(s: &str) -> Result<DecisionOption, String> {
    let (value, rest) = s
        .split_once('=')
        .ok_or_else(|| format!("expected <value>=<label>[:<hint>], got {s:?}"))?;
    let (label, hint) = match rest.split_once(':') {
        Some((label, hint)) => (label, Some(hint.trim()).filter(|h| !h.is_empty())),
        None => (rest, None),
    };
    let (value, label) = (value.trim(), label.trim());
    if value.is_empty() || label.is_empty() {
        return Err(format!("option {s:?} needs both a value and a label"));
    }
    Ok(DecisionOption {
        value: value.to_owned(),
        label: label.to_owned(),
        hint: hint.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(value: &str, label: &str, hint: Option<&str>) -> DecisionOption {
        DecisionOption {
            value: value.into(),
            label: label.into(),
            hint: hint.map(Into::into),
        }
    }

    #[test]
    fn parses_options() {
        assert_eq!(
            parse_option("pg=Postgres"),
            Ok(option("pg", "Postgres", None))
        );
        assert_eq!(
            parse_option("pg=Postgres:the default"),
            Ok(option("pg", "Postgres", Some("the default")))
        );
        // Only the first `=` and `:` split.
        assert_eq!(
            parse_option("a=b=c:d:e"),
            Ok(option("a", "b=c", Some("d:e")))
        );
        assert_eq!(
            parse_option("pg=Postgres:"),
            Ok(option("pg", "Postgres", None))
        );
        for bad in ["pg", "=Postgres", "pg=", "pg=:hint"] {
            assert!(parse_option(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn checks_keys() {
        for good in ["db", "db-engine", "auth_flow2", "2fa"] {
            assert!(check_key(good).is_ok(), "{good}");
        }
        for bad in ["", "../x", ".hidden", "-x", "a/b", "a b", "é"] {
            assert!(check_key(bad).is_err(), "{bad}");
        }
    }
}
