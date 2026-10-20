use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use jiff::SignedDuration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

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
    /// Glyphs the deck draws with: `unicode`, or `nerd` for a Nerd Font terminal.
    pub deck_icons: DeckIcons,
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
            deck_icons: DeckIcons::Unicode,
            pr_poll_interval: SignedDuration::from_mins(2),
        }
    }
}

/// The glyphs the deck draws with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeckIcons {
    /// Box drawing and a few symbols, which any monospace font has.
    #[default]
    Unicode,
    /// Nerd Font icons, powerline pills and segments; needs a Nerd Font v3 terminal font.
    Nerd,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deck_icons: Option<DeckIcons>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "jiff::fmt::serde::duration::friendly::compact::optional"
    )]
    #[schemars(with = "Option<String>")]
    pub pr_poll_interval: Option<SignedDuration>,
}

impl Config {
    /// Reads `config.toml` at `path`. A missing file gives the defaults.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if it can't be read, and [`Error::Toml`] if it doesn't parse.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Error::io(path)(e)),
        };
        toml::from_str(&text).map_err(|source| Error::Toml {
            path: path.to_owned(),
            source,
        })
    }

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
            deck_icons: repo.deck_icons.unwrap_or(defaults.deck_icons),
            pr_poll_interval: repo.pr_poll_interval.unwrap_or(defaults.pr_poll_interval),
        }
    }
}
