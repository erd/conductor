//! Opening the conductor-deck herdr plugin pane beside the conductor's pane, which the
//! conductor's `SessionStart` hook does.

use anyhow::Context as _;
use serde::Deserialize;

use crate::runner::{Cmd, Runner, Tools, check, parse_json};

/// The deck's herdr plugin id.
const PLUGIN: &str = "conductor-deck";
/// The deck's pane entrypoint in its plugin manifest.
const ENTRYPOINT: &str = "deck";

/// Opens the deck as a split to the right of `pane`, unless the deck plugin isn't installed and
/// enabled, or a deck pane is already open in `workspace` (else anywhere).
///
/// herdr doesn't say which plugin owns a pane, but it labels a plugin pane with its manifest
/// title, so an open deck is a pane labelled with the deck entrypoint's title.
pub fn open(
    runner: &dyn Runner,
    tools: &Tools,
    pane: &str,
    workspace: Option<&str>,
) -> anyhow::Result<()> {
    let list = Cmd::new(&tools.herdr).args(["plugin", "list", "--plugin", PLUGIN, "--json"]);
    let output = check("herdr plugin list", runner.run(&list)?)?;
    let plugins: Response<PluginList> =
        parse_json(&output.stdout).context("unexpected `herdr plugin list` output")?;
    let Some(title) = plugins
        .result
        .plugins
        .iter()
        .filter(|plugin| plugin.id == PLUGIN && plugin.enabled)
        .flat_map(|plugin| &plugin.panes)
        .find(|entry| entry.id == ENTRYPOINT)
        .map(|entry| entry.title.as_deref().unwrap_or(ENTRYPOINT))
    else {
        return Ok(());
    };

    if list_panes(runner, tools, workspace)?
        .iter()
        .any(|p| p.label.as_deref() == Some(title))
    {
        return Ok(());
    }

    let open = Cmd::new(&tools.herdr).args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN,
        "--entrypoint",
        ENTRYPOINT,
        "--placement",
        "split",
        "--direction",
        "right",
        "--target-pane",
        pane,
    ]);
    check("herdr plugin pane open", runner.run(&open)?)?;
    Ok(())
}

/// herdr's open panes, in `workspace` if given, else everywhere.
pub fn list_panes(
    runner: &dyn Runner,
    tools: &Tools,
    workspace: Option<&str>,
) -> anyhow::Result<Vec<Pane>> {
    let mut list = Cmd::new(&tools.herdr).args(["pane", "list"]);
    if let Some(workspace) = workspace {
        list = list.args(["--workspace", workspace]);
    }
    let output = check("herdr pane list", runner.run(&list)?)?;
    let panes: Response<PaneList> =
        parse_json(&output.stdout).context("unexpected `herdr pane list` output")?;
    Ok(panes.result.panes)
}

/// The envelope herdr's CLI prints its JSON results in.
#[derive(Deserialize)]
struct Response<T> {
    result: T,
}

#[derive(Deserialize)]
struct PluginList {
    plugins: Vec<Plugin>,
}

#[derive(Deserialize)]
struct Plugin {
    #[serde(rename = "plugin_id")]
    id: String,
    enabled: bool,
    #[serde(default)]
    panes: Vec<PluginPane>,
}

/// A `[[panes]]` entry in a plugin's manifest.
#[derive(Deserialize)]
struct PluginPane {
    id: String,
    title: Option<String>,
}

#[derive(Deserialize)]
struct PaneList {
    panes: Vec<Pane>,
}

/// The fields of a `herdr pane list` entry that conductor uses.
#[derive(Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Fake;

    const PLUGIN_LIST: &str = r#"{"id":"cli:plugin","result":{"plugins":[{"plugin_id":"conductor-deck","enabled":true,"panes":[{"id":"deck","title":"conductor","placement":"split"}]}],"type":"plugin_list"}}"#;
    const NO_PLUGINS: &str = r#"{"id":"cli:plugin","result":{"plugins":[],"type":"plugin_list"}}"#;
    const PANES: &str = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w1:p1","agent_status":"working"},{"pane_id":"w1:p2","label":"notes"}]}}"#;
    const PANES_WITH_DECK: &str = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w1:p3","label":"conductor"}]}}"#;

    fn open_with(fake: &Fake, workspace: Option<&str>) -> anyhow::Result<()> {
        open(fake, &Tools::default(), "w1:p1", workspace)
    }

    #[test]
    fn opens_beside_the_pane() {
        let fake = Fake::default()
            .ok(&["herdr", "plugin", "list"], PLUGIN_LIST)
            .ok(&["herdr", "pane", "list"], PANES)
            .ok(&["herdr", "plugin", "pane", "open"], "{}");
        open_with(&fake, Some("w1")).unwrap();
        assert!(fake.done());
        let argvs = fake.argvs();
        assert_eq!(
            argvs[0],
            [
                "herdr",
                "plugin",
                "list",
                "--plugin",
                "conductor-deck",
                "--json"
            ]
        );
        assert_eq!(argvs[1], ["herdr", "pane", "list", "--workspace", "w1"]);
        assert_eq!(
            argvs[2],
            [
                "herdr",
                "plugin",
                "pane",
                "open",
                "--plugin",
                "conductor-deck",
                "--entrypoint",
                "deck",
                "--placement",
                "split",
                "--direction",
                "right",
                "--target-pane",
                "w1:p1"
            ]
        );
    }

    #[test]
    fn lists_every_pane_without_a_workspace() {
        let fake = Fake::default()
            .ok(&["herdr", "plugin", "list"], PLUGIN_LIST)
            .ok(&["herdr", "pane", "list"], PANES)
            .ok(&["herdr", "plugin", "pane", "open"], "{}");
        open_with(&fake, None).unwrap();
        assert_eq!(fake.argvs()[1], ["herdr", "pane", "list"]);
    }

    #[test]
    fn skips_when_not_installed() {
        let fake = Fake::default().ok(&["herdr", "plugin", "list"], NO_PLUGINS);
        open_with(&fake, Some("w1")).unwrap();
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn skips_when_disabled() {
        let disabled = PLUGIN_LIST.replace(r#""enabled":true"#, r#""enabled":false"#);
        let fake = Fake::default().ok(&["herdr", "plugin", "list"], &disabled);
        open_with(&fake, Some("w1")).unwrap();
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn skips_when_already_open() {
        let fake = Fake::default()
            .ok(&["herdr", "plugin", "list"], PLUGIN_LIST)
            .ok(&["herdr", "pane", "list"], PANES_WITH_DECK);
        open_with(&fake, Some("w1")).unwrap();
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn herdr_failure_is_an_error() {
        let fake = Fake::default()
            .ok(&["herdr", "plugin", "list"], PLUGIN_LIST)
            .ok(&["herdr", "pane", "list"], PANES)
            .fail(&["herdr", "plugin", "pane", "open"], 1, "no such pane");
        let err = open_with(&fake, Some("w1")).unwrap_err().to_string();
        assert_eq!(
            err,
            "`herdr plugin pane open` failed (exit 1): no such pane"
        );
    }
}
