//! `conductor report <verb>`, which a worker runs at milestones and when it finishes.
//!
//! It only appends a `status` event. `--pr` puts the URL in that event and nowhere else: the
//! worker doesn't write `task.json`, whose other writes come from the conductor and aren't
//! locked against this one. Readers get the URL from [`LiveStatus::pr_url`], which
//! `conductor status` falls back to when `task.json` has none.
//!
//! [`LiveStatus::pr_url`]: conductor_state::LiveStatus::pr_url

use anyhow::bail;
use clap::{Args, ValueEnum};
use conductor_state::{Event, EventPayload, StatusVerb, append_event};
use jiff::Timestamp;

use crate::context::{Target, TargetArgs};

#[derive(Args)]
pub struct ReportArgs {
    verb: Verb,
    /// What the worker is doing, or why it's blocked, done or failed.
    text: Option<String>,
    /// The task's PR, once it's open.
    #[arg(long, value_name = "URL")]
    pr: Option<String>,
    #[command(flatten)]
    target: TargetArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum Verb {
    Working,
    Blocked,
    Done,
    Failed,
}

impl From<Verb> for StatusVerb {
    fn from(verb: Verb) -> Self {
        match verb {
            Verb::Working => Self::Working,
            Verb::Blocked => Self::Blocked,
            Verb::Done => Self::Done,
            Verb::Failed => Self::Failed,
        }
    }
}

/// Appends the `status` event.
pub fn run(args: &ReportArgs) -> anyhow::Result<()> {
    if let Some(url) = &args.pr
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        bail!("--pr must be a URL, got {url:?}");
    }
    let target = Target::resolve(&args.target)?;
    let task_dir = target.task_dir()?;
    let event = Event {
        ts: Timestamp::now(),
        task: target.task_id()?.to_owned(),
        session_id: None,
        payload: EventPayload::Status {
            verb: args.verb.into(),
            text: args.text.clone().filter(|t| !t.is_empty()),
            pr_url: args.pr.clone(),
        },
    };
    append_event(&task_dir, &event)?;
    Ok(())
}
