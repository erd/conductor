use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a task is in its lifecycle. Changes only on purpose, through `conductor task phase`;
/// see the lifecycle diagram in `docs/design.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The goal is being captured and split into tasks.
    Intake,
    /// `brief.md` is written.
    Briefed,
    /// `conductor spawn` is creating the worktree and worker session.
    Spawning,
    /// The worker is running.
    Working,
    /// The worker opened a decision and is waiting for an answer.
    NeedsDecision,
    /// The worker reported done; the conductor is reviewing its draft PR or scout report.
    Review,
    /// The conductor marked the PR ready; waiting for a human to merge it on GitHub.
    AwaitingMerge,
    /// GitHub reports the PR merged.
    Merged,
    /// The worktree, worker session and herdr workspace are gone.
    TornDown,
    /// The task failed and needs a human.
    Failed,
    /// A human abandoned the task.
    Abandoned,
}

impl Phase {
    /// Every phase, in lifecycle order.
    pub const ALL: [Phase; 11] = [
        Phase::Intake,
        Phase::Briefed,
        Phase::Spawning,
        Phase::Working,
        Phase::NeedsDecision,
        Phase::Review,
        Phase::AwaitingMerge,
        Phase::Merged,
        Phase::TornDown,
        Phase::Failed,
        Phase::Abandoned,
    ];

    /// Whether a task may move from `from` to `to`.
    ///
    /// Allows the edges of the lifecycle diagram, plus: any phase except `torn_down` may move
    /// to `failed` or `abandoned`, `failed` may move to `abandoned`, and both may be torn down.
    /// Nothing leaves `torn_down`.
    #[must_use]
    pub fn can_transition(from: Phase, to: Phase) -> bool {
        use Phase::{
            Abandoned, AwaitingMerge, Briefed, Failed, Intake, Merged, NeedsDecision, Review,
            Spawning, TornDown, Working,
        };

        let live = !matches!(from, TornDown | Failed | Abandoned);
        if live && matches!(to, Failed | Abandoned) {
            return true;
        }
        match from {
            Intake => to == Briefed,
            Briefed => to == Spawning,
            Spawning | NeedsDecision => to == Working,
            Working => matches!(to, NeedsDecision | Review),
            Review => matches!(to, Working | AwaitingMerge | TornDown),
            AwaitingMerge => matches!(to, Working | Merged),
            Merged | Abandoned => to == TornDown,
            Failed => matches!(to, Abandoned | TornDown),
            TornDown => false,
        }
    }
}
