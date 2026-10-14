use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SchemaV1;

/// An open question and, once answered, its answer: `decisions/<key>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Decision {
    pub schema: SchemaV1,
    /// Unique within the task; also the file name.
    pub key: String,
    pub title: String,
    /// Context for whoever answers.
    pub about: String,
    #[serde(default)]
    pub options: Vec<DecisionOption>,
    /// The `value` of the option the asker recommends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommend: Option<String>,
    /// Whether an answer may be free text instead of one of `options`.
    #[serde(default)]
    pub allow_freeform: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Answer>,
}

/// One choice offered by a [`Decision`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DecisionOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The answer to a [`Decision`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Answer {
    /// An option's `value`, or free text when the decision allows it.
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub answered_by: AnsweredBy,
    pub ts: Timestamp,
}

/// Who answered a [`Decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredBy {
    /// The human, through the deck or by telling the conductor.
    User,
    /// The conductor, on its own authority.
    Conductor,
}
