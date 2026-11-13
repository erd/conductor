//! herdr-core's `hc`, which runs each worker in a terminal of its own, and the Herdr app's
//! `herdr-app`, which shows those terminals as tabs.
//!
//! A worker started with `hc run --label herdr.space=<space>` gets its own tab in that space
//! once the app adopts it, typically within a few hundred milliseconds. The app doesn't label the
//! terminal with its tab, so the tab is found in `herdr-app ls`. When the worker's program ends,
//! by exiting or by `hc kill`, its pane closes, and so does its tab, since that was the tab's only
//! pane.

use std::collections::BTreeMap;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde::Deserialize;

use crate::runner::{Cmd, RunOutput, Runner, Tools, check, parse_json};

/// hc's exit code when the terminal doesn't exist; its stderr says `"kind":"not_found"`.
pub const NOT_FOUND: i32 = 3;
/// hc's exit code when `send` refuses a terminal that is waiting on a person: a permission
/// prompt or a question.
pub const BLOCKED: i32 = 9;
/// hc's exit code when the terminal's program has already ended.
pub const FINISHED: i32 = 11;

/// The label that puts an hc terminal in a Herdr space.
pub const SPACE_LABEL: &str = "herdr.space";

/// How long spawn and resume wait for the app to show a new worker before giving up on naming
/// its tab.
pub const TAB_TIMEOUT: Duration = Duration::from_secs(5);
/// How often to look for the new worker's tab while waiting.
const TAB_POLL: Duration = Duration::from_millis(100);

/// The fields of `hc get --json` that conductor uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Terminal {
    /// `live` or `finished`.
    pub state: String,
    /// The program it was started with.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Terminal {
    pub fn live(&self) -> bool {
        self.state == "live"
    }
}

/// Whether an hc command failed with exit code `code`.
pub fn failed_with(output: &RunOutput, code: i32) -> bool {
    output.code == Some(code)
}

/// `terminal` as `hc get` reports it, or `None` if hc has no such terminal.
pub fn get(runner: &dyn Runner, tools: &Tools, terminal: &str) -> anyhow::Result<Option<Terminal>> {
    let cmd = Cmd::new(&tools.hc).args(["get", terminal, "--json"]);
    let output = runner.run(&cmd)?;
    if failed_with(&output, NOT_FOUND) {
        return Ok(None);
    }
    let output = check("hc get", output)?;
    parse_json(&output.stdout)
        .context("unexpected `hc get` output")
        .map(Some)
}

/// `hc run` for a worker: `argv` in the background, in `cwd`, shown in `space` if given. hc
/// doesn't type notes about it into the conductor's session; the worker's hooks already report
/// what it does.
pub fn run(tools: &Tools, cwd: &Path, space: Option<&str>, argv: &[String]) -> Cmd {
    let mut cmd = Cmd::new(&tools.hc)
        .args(["run", "--bg", "--json", "--cwd"])
        .arg(cwd.to_string_lossy());
    if let Some(space) = space {
        cmd = cmd.arg("--label").arg(format!("{SPACE_LABEL}={space}"));
    }
    cmd.args(["--no-notify", "--"]).args(argv.iter().cloned())
}

/// The address `hc run --json` printed, e.g. `t7`.
pub fn parse_run(stdout: &str) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct Run {
        address: String,
    }
    let run: Run = parse_json(stdout).context("unexpected `hc run` output")?;
    Ok(run.address)
}

/// Where the app shows a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub tab: String,
    /// The app's pane id (`p…`), which differs from the terminal's address.
    pub pane: String,
}

/// Where the app shows `terminal`, from `herdr-app ls`, or `None` if it isn't shown.
pub fn locate(runner: &dyn Runner, tools: &Tools, terminal: &str) -> anyhow::Result<Option<Place>> {
    #[derive(Deserialize)]
    struct Ls {
        spaces: Vec<Space>,
    }
    #[derive(Deserialize)]
    struct Space {
        tabs: Vec<Tab>,
    }
    #[derive(Deserialize)]
    struct Tab {
        id: String,
        terminals: Vec<Shown>,
    }
    #[derive(Deserialize)]
    struct Shown {
        terminal: String,
        pane: String,
    }

    let cmd = Cmd::new(&tools.herdr_app).arg("ls");
    let output = check("herdr-app ls", runner.run(&cmd)?)?;
    let ls: Ls = parse_json(&output.stdout).context("unexpected `herdr-app ls` output")?;
    Ok(ls
        .spaces
        .into_iter()
        .flat_map(|space| space.tabs)
        .find_map(|tab| {
            let shown = tab.terminals.into_iter().find(|t| t.terminal == terminal)?;
            Some(Place {
                tab: tab.id,
                pane: shown.pane,
            })
        }))
}

/// Waits up to `timeout` for the app to show `terminal`, then names its tab `name`. An unnamed
/// tab is called by its position, which says nothing about the task.
pub fn name_tab(
    runner: &dyn Runner,
    tools: &Tools,
    terminal: &str,
    name: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let place = loop {
        if let Some(place) = locate(runner, tools, terminal)? {
            break place;
        }
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "Herdr didn't show terminal {terminal} within {}s",
                timeout.as_secs()
            );
        }
        thread::sleep(TAB_POLL.min(deadline - now));
    };
    let rename =
        Cmd::new(&tools.herdr_app).args(["do", "tab.rename", "--id", &place.tab, "--name", name]);
    check("herdr-app do tab.rename", runner.run(&rename)?)?;
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::runner::Fake;

    /// `herdr-app ls` output with the conductor's tab (`t1`) and, if given, a worker's tab
    /// `tb9` showing `worker` in pane `p9`.
    pub fn ls(worker: Option<&str>) -> String {
        let worker = worker.map_or_else(String::new, |t| {
            format!(r#",{{"id":"tb9","name":"2","terminals":[{{"terminal":"{t}","pane":"p9","sub_agents":[]}}]}}"#)
        });
        format!(
            r#"{{"spaces":[{{"id":"sp1","name":"my-repo","tabs":[{{"id":"tb1","name":"Claude","terminals":[{{"terminal":"t1","pane":"p1","sub_agents":[]}}]}}{worker}]}}]}}"#
        )
    }

    /// `hc get --json` output for a terminal in `state` running `argv`.
    pub fn get_json(state: &str, argv: &[&str]) -> String {
        serde_json::json!({
            "address": "t7",
            "argv": argv,
            "state": state,
            "labels": {"herdr.space": "sp1", "where": "my-repo / 2"},
        })
        .to_string()
    }

    #[test]
    fn get_reads_state_and_treats_not_found_as_none() {
        let fake = Fake::default()
            .ok(&["hc", "get", "t7", "--json"], &get_json("live", &["sh"]))
            .fail(
                &["hc", "get", "t8"],
                NOT_FOUND,
                r#"{"error":{"kind":"not_found","message":"no terminal \"t8\""}}"#,
            )
            .fail(&["hc", "get", "t9"], 1, "boom");
        let tools = Tools::default();
        let t7 = get(&fake, &tools, "t7").unwrap().unwrap();
        assert!(t7.live());
        assert_eq!(t7.argv, ["sh"]);
        assert_eq!(t7.labels[SPACE_LABEL], "sp1");
        assert!(get(&fake, &tools, "t8").unwrap().is_none());
        assert!(get(&fake, &tools, "t9").is_err());
    }

    #[test]
    fn run_labels_the_space_when_known() {
        let tools = Tools::default();
        let argv = ["env".to_owned(), "claude".to_owned()];
        let cmd = run(&tools, Path::new("/wt"), Some("sp1"), &argv);
        assert_eq!(
            cmd.display(),
            "hc run --bg --json --cwd /wt --label herdr.space=sp1 --no-notify -- env claude"
        );
        let cmd = run(&tools, Path::new("/wt"), None, &argv);
        assert_eq!(
            cmd.display(),
            "hc run --bg --json --cwd /wt --no-notify -- env claude"
        );
        let out =
            r#"{"address":"t18","agent":"sh","cwd":"/wt","parent":"t11","state":"live","turn":0}"#;
        assert_eq!(parse_run(out).unwrap(), "t18");
    }

    #[test]
    fn name_tab_renames_the_workers_tab() {
        let fake = Fake::default()
            .ok(&["herdr-app", "ls"], &ls(Some("t7")))
            .ok(&["herdr-app", "do", "tab.rename"], r#"{"ok":true}"#);
        name_tab(
            &fake,
            &Tools::default(),
            "t7",
            "fix-login-07",
            Duration::ZERO,
        )
        .unwrap();
        assert!(fake.done());
        assert_eq!(
            fake.argvs()[1],
            [
                "herdr-app",
                "do",
                "tab.rename",
                "--id",
                "tb9",
                "--name",
                "fix-login-07"
            ]
        );
    }

    #[test]
    fn name_tab_gives_up_when_not_shown() {
        let fake = Fake::default().ok(&["herdr-app", "ls"], &ls(None));
        let err = name_tab(&fake, &Tools::default(), "t7", "x", Duration::ZERO).unwrap_err();
        assert_eq!(err.to_string(), "Herdr didn't show terminal t7 within 0s");

        let fake = Fake::default().fail(&["herdr-app", "ls"], 1, "no window");
        let err = name_tab(&fake, &Tools::default(), "t7", "x", Duration::ZERO).unwrap_err();
        assert_eq!(err.to_string(), "`herdr-app ls` failed (exit 1): no window");
    }

    #[test]
    fn locate_finds_the_pane() {
        let fake = Fake::default().ok(&["herdr-app", "ls"], &ls(None));
        let place = locate(&fake, &Tools::default(), "t1").unwrap().unwrap();
        assert_eq!(
            place,
            Place {
                tab: "tb1".into(),
                pane: "p1".into()
            }
        );
    }
}
