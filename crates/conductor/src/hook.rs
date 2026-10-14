//! `conductor hook <event>`, which the plugin's hook shim runs for every Claude Code hook in
//! every session on the machine.
//!
//! Outside conductor sessions it returns before touching stdin or the filesystem. Inside one it
//! never fails the session: it always exits 0, prints nothing (Claude would parse stdout as hook
//! output), and logs errors to `logs/hook.log`.

use std::any::Any;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use conductor_state::{
    Event, EventPayload, Home, append_event, read_repo_record, write_repo_record_atomic,
};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::context::env_var;

/// `hook.log` is moved to `hook.log.1` once it reaches this size.
const LOG_LIMIT: u64 = 1 << 20;
/// A `prompt` event keeps this many characters of the prompt's first line.
const PROMPT_CHARS: usize = 120;
/// A Bash tool summary keeps this many characters of the command.
const COMMAND_CHARS: usize = 80;

/// Records hook `event`, reading Claude's hook JSON from stdin.
pub fn run(event: Option<&str>) {
    let task = env_var("CONDUCTOR_TASK_ID");
    let role = env_var("CONDUCTOR_ROLE");
    if task.is_none() && role.is_none() {
        return;
    }

    let event = event.unwrap_or_default();
    // A panic must not print to stderr or exit non-zero either.
    panic::set_hook(Box::new(|_| {}));
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        record(event, task.as_deref(), role.as_deref())
    }))
    .unwrap_or_else(|panic| Err(anyhow!("panicked: {}", panic_message(&*panic))));

    if let Err(err) = result
        && let Ok(home) = Home::resolve()
    {
        let line = format!(
            "{} hook {event} task={} role={} pid={}: {}",
            Timestamp::now(),
            task.as_deref().unwrap_or("-"),
            role.as_deref().unwrap_or("-"),
            std::process::id(),
            format!("{err:#}").replace('\n', " "),
        );
        // Nowhere left to report a failure to log.
        let _ = append_log(&home.logs_dir(), &line);
    }
}

fn record(event: &str, task: Option<&str>, role: Option<&str>) -> anyhow::Result<()> {
    // Drain stdin whatever happens next, so Claude's write to the pipe can't fail.
    let mut stdin = String::new();
    std::io::stdin()
        .read_to_string(&mut stdin)
        .context("cannot read stdin")?;
    let event = HookEvent::parse(event)?;

    if let Some(task) = task {
        let input: HookInput = serde_json::from_str(&stdin).context("malformed hook input")?;
        let task_dir = Home::resolve()?.task_dir(&repo_id()?, task);
        let event = Event {
            ts: Timestamp::now(),
            task: task.to_owned(),
            session_id: input.session_id.clone(),
            payload: payload(event, &input, env_var)?,
        };
        append_event(&task_dir, &event)?;
    } else if role == Some("conductor") && event == HookEvent::SessionStart {
        record_conductor_pane()?;
        // #12 opens the deck here.
    }
    Ok(())
}

/// Saves the conductor session's herdr pane and workspace in `repo.json`, keeping the recorded
/// ones when not running inside herdr.
fn record_conductor_pane() -> anyhow::Result<()> {
    let repo_dir = Home::resolve()?.repo_dir(&repo_id()?);
    let mut record = read_repo_record(&repo_dir)?.with_context(|| {
        format!(
            "no repo.json in {}; run `conductor init`",
            repo_dir.display()
        )
    })?;
    if let Some(pane) = env_var("HERDR_PANE_ID") {
        record.herdr_pane_id = Some(pane);
    }
    if let Some(workspace) = env_var("HERDR_WORKSPACE_ID") {
        record.herdr_workspace_id = Some(workspace);
    }
    write_repo_record_atomic(&repo_dir, &record)?;
    Ok(())
}

fn repo_id() -> anyhow::Result<String> {
    env_var("CONDUCTOR_REPO_ID").context("CONDUCTOR_REPO_ID is not set")
}

/// The hook events the plugin registers, named as in `hooks.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    Stop,
    StopFailure,
    SessionEnd,
}

impl HookEvent {
    /// Parses a kebab-case name, also accepting `_` for `-`.
    fn parse(name: &str) -> anyhow::Result<Self> {
        Ok(match name.replace('_', "-").as_str() {
            "session-start" => Self::SessionStart,
            "user-prompt-submit" => Self::UserPromptSubmit,
            "pre-tool-use" => Self::PreToolUse,
            "post-tool-use" => Self::PostToolUse,
            "post-tool-use-failure" => Self::PostToolUseFailure,
            "notification" => Self::Notification,
            "stop" => Self::Stop,
            "stop-failure" => Self::StopFailure,
            "session-end" => Self::SessionEnd,
            _ => bail!("unknown hook event {name:?}"),
        })
    }
}

/// The fields of Claude's hook input that conductor uses, across all events. See
/// <https://code.claude.com/docs/en/hooks>.
#[derive(Debug, Default, Deserialize)]
struct HookInput {
    session_id: Option<String>,
    transcript_path: Option<String>,
    // SessionStart
    source: Option<String>,
    // UserPromptSubmit
    prompt: Option<String>,
    // PreToolUse, PostToolUse and PostToolUseFailure
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Value,
    tool_use_id: Option<String>,
    // Notification
    notification_type: Option<String>,
    message: Option<String>,
    // StopFailure
    error: Option<String>,
    error_details: Option<String>,
    // SessionEnd
    reason: Option<String>,
}

/// The event payload for hook `event` with `input`, reading herdr ids through `env`.
fn payload(
    event: HookEvent,
    input: &HookInput,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<EventPayload> {
    let required = |value: &Option<String>, name: &str| {
        value
            .clone()
            .with_context(|| format!("hook input has no `{name}`"))
    };
    Ok(match event {
        HookEvent::SessionStart => EventPayload::SessionStart {
            source: required(&input.source, "source")?,
            herdr_pane_id: env("HERDR_PANE_ID"),
            herdr_workspace_id: env("HERDR_WORKSPACE_ID"),
            transcript_path: input.transcript_path.clone(),
        },
        HookEvent::UserPromptSubmit => {
            let prompt = required(&input.prompt, "prompt")?;
            let first_line = prompt.trim_start().lines().next().unwrap_or_default();
            EventPayload::Prompt {
                text: truncate_chars(first_line, PROMPT_CHARS),
            }
        }
        HookEvent::PreToolUse => {
            let tool_name = required(&input.tool_name, "tool_name")?;
            EventPayload::ToolStart {
                summary: tool_summary(&tool_name, &input.tool_input),
                tool_name,
                tool_use_id: input.tool_use_id.clone(),
            }
        }
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            let tool_name = required(&input.tool_name, "tool_name")?;
            EventPayload::ToolEnd {
                summary: tool_summary(&tool_name, &input.tool_input),
                tool_name,
                tool_use_id: input.tool_use_id.clone(),
                ok: event == HookEvent::PostToolUse,
            }
        }
        HookEvent::Notification => EventPayload::Attention {
            notification_type: required(&input.notification_type, "notification_type")?,
            message: required(&input.message, "message")?,
        },
        HookEvent::Stop => EventPayload::TurnEnd { error: None },
        HookEvent::StopFailure => {
            let error = input.error.as_deref().unwrap_or("unknown");
            EventPayload::TurnEnd {
                error: Some(match &input.error_details {
                    Some(details) => format!("{error}: {details}"),
                    None => error.to_owned(),
                }),
            }
        }
        HookEvent::SessionEnd => EventPayload::SessionEnd {
            reason: required(&input.reason, "reason")?,
        },
    })
}

/// A short summary of a tool call: the file for Read, Edit and Write, the head of the command
/// for Bash, the pattern for Grep and Glob, and nothing for other tools.
fn tool_summary(tool_name: &str, tool_input: &Value) -> Option<String> {
    let field = |name: &str| tool_input.get(name)?.as_str().filter(|s| !s.is_empty());
    match tool_name {
        "Read" | "Edit" | "Write" => field("file_path").map(str::to_owned),
        "Bash" => field("command").map(|command| truncate_chars(command, COMMAND_CHARS)),
        "Grep" | "Glob" => field("pattern").map(str::to_owned),
        _ => None,
    }
}

/// `s` cut to at most `max` characters, ending a cut string with `…`.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// Appends `line` to `logs_dir/hook.log`, first moving a full log to `hook.log.1`.
fn append_log(logs_dir: &Path, line: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join("hook.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() >= LOG_LIMIT) {
        std::fs::rename(&path, logs_dir.join("hook.log.1"))?;
    }
    let mut file = OpenOptions::new().append(true).create(true).open(&path)?;
    file.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kebab_and_snake_case() {
        assert_eq!(
            HookEvent::parse("pre-tool-use").unwrap(),
            HookEvent::PreToolUse
        );
        assert_eq!(
            HookEvent::parse("post_tool_use_failure").unwrap(),
            HookEvent::PostToolUseFailure
        );
        assert!(HookEvent::parse("PreToolUse").is_err());
        assert!(HookEvent::parse("").is_err());
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "ab…");
        assert_eq!(truncate_chars("éééé", 3), "éé…");
    }

    #[test]
    fn missing_required_field_is_an_error() {
        let err = payload(HookEvent::SessionEnd, &HookInput::default(), |_| None).unwrap_err();
        assert_eq!(err.to_string(), "hook input has no `reason`");
    }

    #[test]
    fn log_rotates_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hook.log");
        let full = vec![b'x'; usize::try_from(LOG_LIMIT).unwrap()];
        std::fs::write(&log, &full).unwrap();

        append_log(dir.path(), "next").unwrap();
        assert_eq!(std::fs::read(dir.path().join("hook.log.1")).unwrap(), full);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "next\n");

        append_log(dir.path(), "again").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "next\nagain\n");
    }
}
