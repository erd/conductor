use std::collections::BTreeMap;

use jiff::SignedDuration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `config.toml`: global defaults, plus optional per-repo overrides under
/// `[repos."<repo-id>"]`. Use [`Config::resolve`] to get the settings for one repo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    #[serde(flatten)]
    pub defaults: Settings,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, RepoOverrides>,
}

/// Settings that apply to a repo's conductor and workers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    /// Claude model for workers; unset uses Claude Code's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Claude effort level for workers; unset uses Claude Code's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Prefix for task branches.
    pub branch_prefix: String,
    /// Width of the deck, in columns.
    pub deck_width: u16,
    /// How often `conductor watch` polls an open PR, e.g. `"2m"`.
    #[serde(serialize_with = "jiff::fmt::serde::duration::friendly::compact::required")]
    #[schemars(with = "String")]
    pub pr_poll_interval: SignedDuration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: None,
            effort: None,
            branch_prefix: "conductor/".to_owned(),
            deck_width: 48,
            pr_poll_interval: SignedDuration::from_mins(2),
        }
    }
}

/// Per-repo overrides; each set field replaces the global default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RepoOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deck_width: Option<u16>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "jiff::fmt::serde::duration::friendly::compact::optional"
    )]
    #[schemars(with = "Option<String>")]
    pub pr_poll_interval: Option<SignedDuration>,
}

impl Config {
    /// The settings for `repo_id`: its overrides where set, the global defaults otherwise.
    #[must_use]
    pub fn resolve(&self, repo_id: &str) -> Settings {
        let defaults = &self.defaults;
        let Some(repo) = self.repos.get(repo_id) else {
            return defaults.clone();
        };
        Settings {
            model: repo.model.clone().or_else(|| defaults.model.clone()),
            effort: repo.effort.clone().or_else(|| defaults.effort.clone()),
            branch_prefix: repo
                .branch_prefix
                .clone()
                .unwrap_or_else(|| defaults.branch_prefix.clone()),
            deck_width: repo.deck_width.unwrap_or(defaults.deck_width),
            pr_poll_interval: repo.pr_poll_interval.unwrap_or(defaults.pr_poll_interval),
        }
    }
}
