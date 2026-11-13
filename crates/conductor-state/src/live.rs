use std::collections::BTreeMap;
use std::mem;

use jiff::Timestamp;

use crate::{CheckState, Event, EventPayload, PrState, ReviewDecision, StatusVerb};

/// A task's live status, folded from its events with [`fold`] or [`LiveStatus::apply`].
/// `conductor status` and the deck's feed (`conductor deck --json`) both build on it.
///
/// This is separate from the task's [`Phase`](crate::Phase), which lives in `task.json` and
/// changes only on purpose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveStatus {
    pub activity: Activity,
    /// When the current kind of activity began. Moving from one tool to the next doesn't
    /// reset it, so a card shows how long the whole turn has run. `None` before any event
    /// changes the activity.
    pub since: Option<Timestamp>,
    /// The worker's latest `conductor report`.
    pub status: Option<LastStatus>,
    /// The latest `pr_url` from a `status` event.
    pub pr_url: Option<String>,
    /// Decisions opened and not yet answered: each key, and when it was opened.
    pub open_decisions: BTreeMap<String, Timestamp>,
    /// The latest `pr` event.
    pub pr: Option<PrStatus>,
    /// From the latest `session_start`.
    pub session_id: Option<String>,
    /// The hc terminal of the latest `session_start`.
    pub terminal_id: Option<String>,
    /// When the latest `phase` event moved the task.
    pub phase_since: Option<Timestamp>,
}

/// What the worker's Claude session is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Activity {
    /// No session events yet.
    #[default]
    NotStarted,
    /// Mid-turn: running `tool`, or thinking between tools when it's `None`.
    Working { tool: Option<CurrentTool> },
    /// Waiting on a permission prompt in the worker's pane.
    NeedsPermission { message: String },
    /// The turn ended. `error` is set when it ended on an API error (`StopFailure`).
    Idle { error: Option<String> },
    /// The session ended.
    Exited { reason: String },
}

/// The tool a working session is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentTool {
    pub name: String,
    /// Short summary of the input, e.g. a file path.
    pub summary: Option<String>,
}

/// A `conductor report` from the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastStatus {
    pub verb: StatusVerb,
    pub text: Option<String>,
    /// The PR this report named, if any.
    pub pr_url: Option<String>,
    pub ts: Timestamp,
}

/// A PR's state as of the latest `pr` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrStatus {
    pub state: PrState,
    pub checks: Option<CheckState>,
    pub review: Option<ReviewDecision>,
    pub ts: Timestamp,
}

/// Folds `events`, oldest first, into a [`LiveStatus`].
#[must_use]
pub fn fold<'a>(events: impl IntoIterator<Item = &'a Event>) -> LiveStatus {
    let mut status = LiveStatus::default();
    for event in events {
        status.apply(event);
    }
    status
}

impl LiveStatus {
    /// Folds one more event in, for callers tailing `events.jsonl`.
    pub fn apply(&mut self, event: &Event) {
        let ts = event.ts;
        match &event.payload {
            EventPayload::SessionStart {
                source,
                terminal_id,
                ..
            } => {
                self.session_id.clone_from(&event.session_id);
                self.terminal_id.clone_from(terminal_id);
                // A new session is the only way out of `Exited`.
                if self.exited() {
                    self.activity = Activity::NotStarted;
                }
                // An auto-compact restarts the session mid-turn; the turn carries on.
                if source != "compact" || self.activity == Activity::NotStarted {
                    self.set_activity(Activity::Idle { error: None }, ts);
                }
            }
            EventPayload::Prompt { .. } => self.set_activity(Activity::Working { tool: None }, ts),
            EventPayload::ToolStart {
                tool_name, summary, ..
            } => self.set_activity(
                Activity::Working {
                    tool: Some(CurrentTool {
                        name: tool_name.clone(),
                        summary: summary.clone(),
                    }),
                },
                ts,
            ),
            // Also ends a granted permission prompt: `PreToolUse` fires before the prompt.
            EventPayload::ToolEnd { .. } => {
                self.set_activity(Activity::Working { tool: None }, ts);
            }
            EventPayload::Attention {
                notification_type,
                message,
            } => match notification_type.as_str() {
                "permission_prompt" => self.set_activity(
                    Activity::NeedsPermission {
                        message: message.clone(),
                    },
                    ts,
                ),
                "idle_prompt" => {
                    if matches!(
                        self.activity,
                        Activity::NotStarted | Activity::Working { .. }
                    ) {
                        self.set_activity(Activity::Idle { error: None }, ts);
                    }
                }
                _ => {}
            },
            EventPayload::TurnEnd { error } => {
                self.set_activity(
                    Activity::Idle {
                        error: error.clone(),
                    },
                    ts,
                );
            }
            EventPayload::SessionEnd { reason } => self.set_activity(
                Activity::Exited {
                    reason: reason.clone(),
                },
                ts,
            ),
            EventPayload::Status { verb, text, pr_url } => {
                self.status = Some(LastStatus {
                    verb: *verb,
                    text: text.clone(),
                    pr_url: pr_url.clone(),
                    ts,
                });
                if pr_url.is_some() {
                    self.pr_url.clone_from(pr_url);
                }
            }
            EventPayload::DecisionOpen { key } => {
                self.open_decisions.insert(key.clone(), ts);
            }
            EventPayload::DecisionAnswered { key } => {
                self.open_decisions.remove(key);
            }
            EventPayload::Pr {
                state,
                checks,
                review,
            } => {
                self.pr = Some(PrStatus {
                    state: *state,
                    checks: *checks,
                    review: *review,
                    ts,
                });
            }
            EventPayload::Phase { .. } => self.phase_since = Some(ts),
            EventPayload::Unknown { .. } => {}
        }
    }

    fn exited(&self) -> bool {
        matches!(self.activity, Activity::Exited { .. })
    }

    /// Moves to `activity`, resetting `since` if its kind changed. Does nothing once exited:
    /// stray hook events from a session that's gone don't revive it.
    fn set_activity(&mut self, activity: Activity, ts: Timestamp) {
        if self.exited() {
            return;
        }
        if self.since.is_none() || mem::discriminant(&self.activity) != mem::discriminant(&activity)
        {
            self.since = Some(ts);
        }
        self.activity = activity;
    }
}
