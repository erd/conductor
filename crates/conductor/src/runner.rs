//! Running the external tools conductor drives (`wt`, `herdr`, `git`, `gh`) behind a trait, so
//! commands that orchestrate them can be tested against a [`Fake`].

use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;

use crate::context::env_var;
use crate::shell;

/// A command to run: program, arguments, working directory and extra environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The command as one shell-quoted line, for printing.
    pub fn display(&self) -> String {
        let env = self
            .env
            .iter()
            .map(|(key, value)| format!("{key}={} ", shell::quote(value)));
        let mut line: String = env.collect();
        line.push_str(&shell::quote(&self.program));
        for arg in &self.args {
            line.push(' ');
            line.push_str(&shell::quote(arg));
        }
        line
    }
}

/// What a finished command printed and how it exited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutput {
    /// The exit code; `None` if a signal killed it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs [`Cmd`]s and waits for them.
pub trait Runner {
    /// Runs `cmd` to completion, capturing its output.
    ///
    /// # Errors
    ///
    /// Only if the command can't be started; a non-zero exit is an `Ok` [`RunOutput`].
    fn run(&self, cmd: &Cmd) -> anyhow::Result<RunOutput>;
}

/// Runs commands for real.
pub struct System;

impl Runner for System {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<RunOutput> {
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args).envs(cmd.env.iter().cloned());
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        let output = command
            .output()
            .with_context(|| format!("cannot run `{}`", cmd.program))?;
        Ok(RunOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Where to find each tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub wt: String,
    pub herdr: String,
    pub git: String,
    pub gh: String,
}

impl Tools {
    /// `herdr` from `HERDR_BIN_PATH`, which herdr sets in its panes, else from `PATH`; the
    /// others from `PATH`.
    pub fn resolve() -> Self {
        Self {
            herdr: env_var("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_owned()),
            ..Self::default()
        }
    }
}

impl Default for Tools {
    /// Every tool by its bare name, found on `PATH`.
    fn default() -> Self {
        Self {
            wt: "wt".to_owned(),
            herdr: "herdr".to_owned(),
            git: "git".to_owned(),
            gh: "gh".to_owned(),
        }
    }
}

#[cfg(test)]
pub use fake::Fake;

#[cfg(test)]
mod fake {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::{Cmd, RunOutput, Runner};

    /// A [`Runner`] that answers from a script and records every call.
    ///
    /// Each expectation is a command prefix (program, then leading args) and the output to
    /// return. Calls must arrive in the scripted order; an unexpected call panics.
    #[derive(Default)]
    pub struct Fake {
        script: RefCell<VecDeque<(Vec<String>, RunOutput)>>,
        calls: RefCell<Vec<Cmd>>,
    }

    impl Fake {
        /// Expects a call starting with `prefix` and answers it with `output`.
        pub fn expect(self, prefix: &[&str], output: RunOutput) -> Self {
            let prefix = prefix.iter().map(|&s| s.to_owned()).collect();
            self.script.borrow_mut().push_back((prefix, output));
            self
        }

        /// Expects a call starting with `prefix` that exits 0 printing `stdout`.
        pub fn ok(self, prefix: &[&str], stdout: &str) -> Self {
            self.expect(prefix, ok(stdout))
        }

        /// Expects a call starting with `prefix` that exits `code` printing `stderr`.
        pub fn fail(self, prefix: &[&str], code: i32, stderr: &str) -> Self {
            let output = RunOutput {
                code: Some(code),
                stdout: String::new(),
                stderr: stderr.to_owned(),
            };
            self.expect(prefix, output)
        }

        /// Every call so far.
        pub fn calls(&self) -> Vec<Cmd> {
            self.calls.borrow().clone()
        }

        /// Each call's program and args.
        pub fn argvs(&self) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .map(|cmd| std::iter::once(cmd.program).chain(cmd.args).collect())
                .collect()
        }

        /// Whether every scripted call was made.
        pub fn done(&self) -> bool {
            self.script.borrow().is_empty()
        }
    }

    /// An output that exits 0 printing `stdout`.
    pub fn ok(stdout: &str) -> RunOutput {
        RunOutput {
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> anyhow::Result<RunOutput> {
            self.calls.borrow_mut().push(cmd.clone());
            let argv: Vec<&str> = std::iter::once(cmd.program.as_str())
                .chain(cmd.args.iter().map(String::as_str))
                .collect();
            let (prefix, output) = self
                .script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected call: {argv:?}"));
            assert!(
                argv.starts_with(&prefix.iter().map(String::as_str).collect::<Vec<_>>()),
                "expected a call starting with {prefix:?}, got {argv:?}"
            );
            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_env_program_and_args() {
        let mut cmd = Cmd::new("herdr").args(["pane", "run", "p 1"]);
        cmd.env.push(("A".into(), "x y".into()));
        assert_eq!(cmd.display(), "A='x y' herdr pane run 'p 1'");
    }

    #[test]
    fn system_runs_commands() {
        let mut cmd = Cmd::new("sh").args(["-c", "printf %s \"$X\"; echo err >&2; exit 3"]);
        cmd.env.push(("X".into(), "hi".into()));
        let output = System.run(&cmd).unwrap();
        assert_eq!(output.code, Some(3));
        assert!(!output.success());
        assert_eq!(output.stdout, "hi");
        assert_eq!(output.stderr, "err\n");
        assert!(System.run(&Cmd::new("no-such-program-xyz")).is_err());
    }
}
