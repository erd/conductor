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
    /// `conductor watch` saw the task's PR change.
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
