//! Quoting for command lines that a shell reads: `conductor start --dry-run` prints one, and
//! `conductor spawn` types one into a herdr pane, whose shell might be bash, zsh or fish.

use anyhow::bail;

/// `s` as one shell word that bash, zsh and fish all read back as `s`.
///
/// A word made only of characters no shell treats specially stays bare. Anything else is
/// single-quoted, with each `'` and `\` written outside the quotes as `\'` and `\\`. Inside single
/// quotes POSIX shells take a backslash literally but fish reads `\\` and `\'` as escapes, so
/// keeping backslashes out of the quotes is what makes the result portable.
pub fn quote(s: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c);
    if !s.is_empty() && s.chars().all(plain) {
        return s.to_owned();
    }
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\'' => out.push_str(r"'\''"),
            '\\' => out.push_str(r"'\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The words of a command line, each quoted with [`quote`] and joined with spaces.
pub fn join<S: AsRef<str>>(words: &[S]) -> String {
    let words: Vec<_> = words.iter().map(|w| quote(w.as_ref())).collect();
    words.join(" ")
}

/// Fails if `s` holds a control character. Text typed into a pane can't carry one: a newline
/// would submit the line early.
pub fn check_typeable(s: &str) -> anyhow::Result<()> {
    if s.chars().any(char::is_control) {
        bail!("{s:?} contains a control character, which can't be typed into a shell");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote("/conductor:conductor"), "/conductor:conductor");
        assert_eq!(quote("/Users/me/my repo"), "'/Users/me/my repo'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(r"a\b"), r"'a'\\'b'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn rejects_control_characters() {
        assert!(check_typeable("plain text, $HOME and 'quotes'").is_ok());
        assert!(check_typeable("two\nlines").is_err());
        assert!(check_typeable("tab\there").is_err());
    }

    /// Each shell prints the words of a quoted command line one per line; they must match the
    /// originals. `sh` always runs; `bash`, `zsh` and `fish` run when installed.
    #[test]
    fn round_trips_through_shells() {
        let words = [
            "plain",
            "with space",
            "it's",
            r"back\slash",
            r"trailing\\",
            r"\'",
            "$HOME `id` $(id) *?[a] ~ ! # ; & | < > ( ) { }",
            "",
            "Read your brief at /Users/me/.config/conductor/brief.md and follow it.",
        ];
        let script = format!("printf '%s\\n' {}", join(&words));
        let expected = words.join("\n") + "\n";
        for shell in ["sh", "bash", "zsh", "fish"] {
            let Ok(output) = Command::new(shell).args(["-c", &script]).output() else {
                assert_ne!(shell, "sh", "sh must be installed");
                continue;
            };
            assert!(output.status.success(), "{shell}: {output:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{shell}");
        }
    }
}
