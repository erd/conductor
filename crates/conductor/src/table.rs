//! Plain aligned tables for human-readable command output.

use std::fmt::Write as _;

/// `header` and `rows` as left-aligned columns two spaces apart, one line each, with trailing
/// spaces trimmed.
pub fn render<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> String {
    let header = header.map(String::from);
    let mut widths = [0; N];
    for line in std::iter::once(&header).chain(rows) {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in std::iter::once(&header).chain(rows) {
        let mut text = String::new();
        for (cell, width) in line.iter().zip(widths) {
            let _ = write!(text, "{cell:width$}  ");
        }
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// `s` cut to `max` characters, ending in `…` if anything was dropped.
pub fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_columns() {
        let rows = [
            ["a".into(), "long cell".into()],
            ["bbb".into(), String::new()],
        ];
        assert_eq!(render(["X", "Y"], &rows), "X    Y\na    long cell\nbbb\n");
    }

    #[test]
    fn cuts_long_cells() {
        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("exactly 10", 10), "exactly 10");
        assert_eq!(cut("much too long", 10), "much too …");
    }
}
