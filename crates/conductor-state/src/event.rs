use jiff::Timestamp;
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::Phase;

/// One line of `events.jsonl`. The common fields sit next to the payload's fields, and the
/// payload's `kind` says which payload it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub ts: Timestamp,
    /// The task id.
    pub task: String,
    /// The Claude session id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    /// Cuts each free-text field down to at most `max` bytes, ending a cut field with `…`.
    /// Identifiers and paths are left alone.
    pub(crate) fn truncate_text(&mut self, max: usize) {
        let fields = match &mut self.payload {
            EventPayload::Prompt { text } | EventPayload::Attention { message: text, .. } => {
                [Some(text), None]
            }
            EventPayload::SessionEnd { reason } => [Some(reason), None],
            EventPayload::ToolStart {
                summary, command, ..
            } => [summary.as_mut(), command.as_mut()],
            EventPayload::ToolEnd { summary, .. } => [summary.as_mut(), None],
            EventPayload::TurnEnd { error } => [error.as_mut(), None],
            EventPayload::Status { text, .. } => [text.as_mut(), None],
            _ => [None, None],
        };
        for field in fields.into_iter().flatten() {
            truncate(field, max);
        }
    }
}

fn truncate(s: &mut String, max: usize) {
    const ELLIPSIS: &str = "…";
    if s.len() <= max {
        return;
    }
    let mut end = max.saturating_sub(ELLIPSIS.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str(ELLIPSIS);
}

/// The `kind`-specific part of an [`Event`].
///
/// Deserializing an unknown `kind` gives [`EventPayload::Unknown`]; a known `kind` with a
/// malformed payload is an error.
// `remote = "Self"` turns the derived impls into inherent fns, so the trait impls below can
// route unknown kinds to `Unknown` before using them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self", tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    /// `SessionStart` hook.
    SessionStart {
        /// Why the session started: `startup`, `resume`, `clear`, `compact`, …
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        herdr_pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        herdr_workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
    },
    /// `UserPromptSubmit` hook.
    Prompt {
        /// First line of the prompt, truncated.
        text: String,
    },
    /// `PreToolUse` hook.
    ToolStart {
        tool_name: String,
        /// Short summary of the input: a file path, a command's head, …
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Bash only, when `summary` cuts it: the whole command, for naming what a permission
        /// prompt is for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        /// Pairs this event with its `tool_end`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
    /// `PostToolUse` hook.
    ToolEnd {
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        ok: bool,
    },
    /// `Notification` hook.
    Attention {
        /// `permission_prompt`, `idle_prompt`, …
        #[serde(rename = "type")]
        notification_type: String,
        message: String,
    },
    /// `Stop` or `StopFailure` hook.
    TurnEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// `SessionEnd` hook.
    SessionEnd { reason: String },
    /// `conductor report <verb>`, from the worker.
    Status {
        verb: StatusVerb,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr_url: Option<String>,
    },
    /// `conductor decision open`.
    DecisionOpen { key: String },
    /// A decision was answered, by the deck or the conductor.
    DecisionAnswered { key: String },
    /// `conductor task phase`, from the conductor.
    Phase { from: Phase, to: Phase },
    /// The task's PR as `gh pr view` reported it: when `conductor watch` saw it change, when
    /// `conductor pr ready` marked it ready, or when `conductor teardown` checked it.
    Pr {
        state: PrState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checks: Option<CheckState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review: Option<ReviewDecision>,
    },
    /// An event kind this version doesn't know, kept as-is so it round-trips. Not part of the
    /// JSON Schema, which describes only what this version writes.
    #[serde(skip)]
    Unknown {
        kind: String,
        /// Every field except the common ones and `kind`.
        fields: Map<String, Value>,
    },
}

impl EventPayload {
    /// Every `kind` with its own variant. A kind missing here would read back as `Unknown`,
    /// which the round-trip tests catch.
    const KNOWN_KINDS: [&str; 12] = [
        "session_start",
        "prompt",
        "tool_start",
        "tool_end",
        "attention",
        "turn_end",
        "session_end",
        "status",
        "decision_open",
        "decision_answered",
        "phase",
        "pr",
    ];
}

impl Serialize for EventPayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown { kind, fields } => {
                let mut map = serializer.serialize_map(Some(fields.len() + 1))?;
                map.serialize_entry("kind", kind)?;
                for (key, value) in fields {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            known => EventPayload::serialize(known, serializer),
        }
    }
}

impl<'de> Deserialize<'de> for EventPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let kind = match fields.get("kind") {
            Some(Value::String(kind)) => kind.clone(),
            Some(_) => return Err(D::Error::custom("event `kind` must be a string")),
            None => return Err(D::Error::missing_field("kind")),
        };
        if Self::KNOWN_KINDS.contains(&kind.as_str()) {
            EventPayload::deserialize(Value::Object(fields)).map_err(D::Error::custom)
        } else {
            fields.remove("kind");
            Ok(Self::Unknown { kind, fields })
        }
    }
}

/// The verb of a `conductor report` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusVerb {
    Working,
    Blocked,
    Done,
    Failed,
}

/// A PR's state on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Draft,
    Open,
    Merged,
    Closed,
}

/// The rollup of a PR's checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pass,
    Fail,
    Pending,
}

/// GitHub's `reviewDecision` for a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Event {
        Event {
            ts: "2026-10-14T12:00:00Z".parse().unwrap(),
            task: "t".into(),
            session_id: None,
            payload: EventPayload::Prompt { text: text.into() },
        }
    }

    fn text(event: &Event) -> &str {
        match &event.payload {
            EventPayload::Prompt { text } => text,
            _ => unreachable!(),
        }
    }

    #[test]
    fn short_text_is_untouched() {
        let mut event = prompt("hello");
        event.truncate_text(5);
        assert_eq!(text(&event), "hello");
    }

    #[test]
    fn long_text_is_cut_with_ellipsis() {
        let mut event = prompt("abcdefghij");
        event.truncate_text(8);
        assert_eq!(text(&event), "abcde…");
        assert!(text(&event).len() <= 8);
    }

    #[test]
    fn tool_start_cuts_summary_and_command() {
        let mut event = Event {
            payload: EventPayload::ToolStart {
                tool_name: "Bash".into(),
                summary: Some("abcdefghij".into()),
                command: Some("abcdefghij --stage prod".into()),
                tool_use_id: None,
            },
            ..prompt("")
        };
        event.truncate_text(8);
        let EventPayload::ToolStart {
            summary, command, ..
        } = &event.payload
        else {
            unreachable!()
        };
        assert_eq!(summary.as_deref(), Some("abcde…"));
        assert_eq!(command.as_deref(), Some("abcde…"));
    }

    #[test]
    fn cut_lands_on_char_boundary() {
        // Each 'é' is two bytes, so a cut at byte 5 must back up to 4.
        let mut event = prompt("éééééé");
        event.truncate_text(8);
        assert_eq!(text(&event), "éé…");
    }
}
