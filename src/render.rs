//! `--pretty`: aligned, human-readable renderings of the documents rotter prints, built from the
//! same values as the JSON. Every interpolated value goes through [`clean`] (code through
//! [`code_lines`], which shares its rules); only rotter's own labels and SGR codes bypass it, so
//! repository or host-file text can never move the cursor, retitle the terminal, write the
//! clipboard or reorder what is shown.

use crate::install::{ParserEntry, ParserInstall};
use crate::integration::{IntegrationResult, Status};
use crate::json::Json;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write;

/// `--color=<auto|always|never>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Whether pretty output is coloured: `auto` only when stdout is a terminal, `NO_COLOR` is unset
/// or empty and `TERM` is not `dumb`.
pub fn color_on(
    choice: Color,
    terminal: bool,
    no_color: Option<&OsStr>,
    term: Option<&OsStr>,
) -> bool {
    match choice {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => {
            terminal && no_color.is_none_or(OsStr::is_empty) && term != Some(OsStr::new("dumb"))
        }
    }
}

/// Characters never shown raw: every Cc (C0, DEL and C1, which holds the 8-bit CSI U+009B and
/// OSC U+009D), the bidi controls (Trojan Source) and zero-width characters.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{061c}'
                | '\u{200b}'..='\u{200d}'
                | '\u{2060}'
                | '\u{feff}'
        )
}

fn push(out: &mut String, c: char) {
    if hidden(c) {
        let _ = write!(out, "{}", c.escape_unicode());
    } else {
        out.push(c);
    }
}

/// A value shown on one line (a path, name, detail, header or revision): hidden characters,
/// newline and tab included, become `\u{…}`.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push(&mut out, c);
    }
    out
}

/// Code shown as lines: split on `\n`, a CR ending a line or the text dropped (CRLF files; a
/// comment's text stops before its LF), tabs kept and every other hidden character, a lone CR
/// included, escaped as by [`clean`].
pub fn code_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut out = String::with_capacity(line.len());
            for c in line.chars() {
                if c == '\t' {
                    out.push(c);
                } else {
                    push(&mut out, c);
                }
            }
            out
        })
        .collect()
}

const BOLD: &str = "1";
const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";
const CYAN: &str = "36";

/// SGR styling, or none. Every span resets itself, so no style outlives its line.
#[derive(Clone, Copy)]
struct Style(bool);

impl Style {
    fn paint(self, sgr: &str, text: &str) -> String {
        if self.0 && !text.is_empty() {
            format!("\u{1b}[{sgr}m{text}\u{1b}[0m")
        } else {
            text.to_owned()
        }
    }
}

/// Rows of `(already cleaned text, SGR)` cells, padded to aligned columns (by characters).
fn table(style: Style, rows: &[Vec<(String, &str)>]) -> Vec<String> {
    let mut widths = Vec::new();
    for row in rows {
        for (index, (text, _)) in row.iter().enumerate() {
            let width = text.chars().count();
            match widths.get_mut(index) {
                Some(known) if *known >= width => {}
                Some(known) => *known = width,
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (index, (text, sgr)) in row.iter().enumerate() {
                line.push_str(&style.paint(sgr, text));
                if index + 1 < row.len() {
                    let pad = widths[index] - text.chars().count() + 2;
                    line.push_str(&" ".repeat(pad));
                }
            }
            line
        })
        .collect()
}

fn finish(lines: Vec<String>) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn or_dash(value: Option<&str>) -> String {
    value.map_or_else(|| "-".to_owned(), clean)
}

/// `rotter integration status`: one aligned row per host with its detail and notes indented
/// under it, then Grok's Claude compatibility, git and this executable.
pub fn status(status: &Status, color: bool) -> String {
    let style = Style(color);
    let mut rows = vec![vec![
        ("host".to_owned(), BOLD),
        ("support".to_owned(), BOLD),
        ("state".to_owned(), BOLD),
        ("path".to_owned(), BOLD),
    ]];
    for host in &status.hosts {
        let state = match host.state {
            Some("installed") => GREEN,
            Some("not_installed") | None => DIM,
            Some("mismatch" | "other_binary") => YELLOW,
            Some(_) => RED,
        };
        rows.push(vec![
            (clean(host.id), ""),
            (
                clean(host.support),
                if host.support == "stable" { "" } else { DIM },
            ),
            (or_dash(host.state), state),
            (or_dash(host.path.as_deref()), ""),
        ]);
    }
    let aligned = table(style, &rows);
    let mut lines = vec![aligned[0].clone()];
    for (host, row) in status.hosts.iter().zip(&aligned[1..]) {
        lines.push(row.clone());
        if !matches!(host.state, Some("installed" | "not_installed"))
            && let Some(detail) = &host.detail
        {
            let sgr = if host.state.is_none() { DIM } else { YELLOW };
            lines.push(format!("  {}", style.paint(sgr, &clean(detail))));
        }
        for note in &host.notes {
            lines.push(format!("  {}", style.paint(DIM, &clean(note))));
        }
    }
    lines.push(String::new());
    let compat = match status.compat_state {
        "found" => YELLOW,
        "unknown" => RED,
        _ => DIM,
    };
    let exe = if status.exe_trusted { GREEN } else { RED };
    lines.extend(table(
        style,
        &[
            vec![
                ("grok compat".to_owned(), BOLD),
                (clean(status.compat_state), compat),
                (or_dash(status.compat_detail.as_deref()), ""),
            ],
            vec![
                ("git".to_owned(), BOLD),
                (or_dash(status.git_path.as_deref()), ""),
                (clean(&status.git_detail), ""),
            ],
            vec![
                ("executable".to_owned(), BOLD),
                (or_dash(status.exe_path.as_deref()), ""),
                (clean(&status.exe_detail), exe),
            ],
        ],
    ));
    finish(lines)
}

/// `rotter integration install|uninstall`: the result and path, then the notes.
pub fn integration(done: &IntegrationResult, color: bool) -> String {
    let style = Style(color);
    let result = match done.result {
        "installed" | "updated" | "removed" => GREEN,
        _ => DIM,
    };
    let mut lines = table(
        style,
        &[vec![
            (clean(done.host), BOLD),
            (clean(done.result), result),
            (clean(&done.path), ""),
        ]],
    );
    for note in &done.notes {
        lines.push(format!("  {}", style.paint(DIM, &clean(note))));
    }
    finish(lines)
}

/// `rotter parser list`: one row per grammar, its source and detail indented under it.
pub fn parsers(parsers: &[ParserEntry], color: bool) -> String {
    let style = Style(color);
    let mut rows = vec![vec![
        ("name".to_owned(), BOLD),
        ("state".to_owned(), BOLD),
        ("path".to_owned(), BOLD),
    ]];
    for parser in parsers {
        let (state, sgr) = match (parser.enabled, parser.installed) {
            (_, true) => ("installed", GREEN),
            (true, false) => ("not installed", YELLOW),
            (false, false) => ("available", DIM),
        };
        rows.push(vec![
            (clean(&parser.name), ""),
            (state.to_owned(), sgr),
            (or_dash(parser.path.as_deref()), ""),
        ]);
    }
    let aligned = table(style, &rows);
    let mut lines = vec![aligned[0].clone()];
    for (parser, row) in parsers.iter().zip(&aligned[1..]) {
        lines.push(row.clone());
        if let Some(source) = &parser.source {
            lines.push(format!("  source {}", clean(source)));
        }
        if let Some(detail) = &parser.detail {
            lines.push(format!("  {}", style.paint(DIM, &clean(detail))));
        }
    }
    finish(lines)
}

/// `rotter parser install`: one row per grammar, a failure's detail under it, then the notes.
pub fn parser_install(done: &ParserInstall, color: bool) -> String {
    let style = Style(color);
    let rows: Vec<Vec<(String, &str)>> = done
        .parsers
        .iter()
        .map(|parser| {
            let result = match parser.result {
                "installed" => GREEN,
                "failed" => RED,
                _ => DIM,
            };
            vec![
                (clean(&parser.name), BOLD),
                (clean(parser.result), result),
                (or_dash(parser.path.as_deref()), ""),
            ]
        })
        .collect();
    let mut lines = Vec::new();
    for (parser, row) in done.parsers.iter().zip(table(style, &rows)) {
        lines.push(row);
        if let Some(detail) = &parser.detail {
            lines.push(format!("  {}", style.paint(RED, &clean(detail))));
        }
    }
    for note in &done.notes {
        lines.push(String::new());
        lines.push(style.paint(YELLOW, &clean(note)));
    }
    finish(lines)
}

fn text(value: &Json) -> String {
    clean(value.as_str().unwrap_or_default())
}

fn status_color(status: &str) -> &'static str {
    match status {
        "ok" => GREEN,
        "not_in_scope" | "skipped_symlink" | "skipped_submodule" => DIM,
        "partial" => YELLOW,
        _ => RED,
    }
}

/// `[a, b]` of a range's `lines`.
fn lines_of(range: &Json) -> (usize, usize) {
    let lines = range.get("lines").as_arr();
    let at = |index: usize| {
        lines
            .get(index)
            .and_then(Json::as_u64)
            .map_or(0, |line| line as usize)
    };
    (at(0), at(1))
}

/// One unit: its header and annotations, then its comments and code with a line-number gutter,
/// `~` on changed lines and comment lines highlighted.
fn unit(style: Style, unit: &Json, lines: &mut Vec<String>) {
    let (first, last) = lines_of(unit.get("range"));
    let mut header = format!("    {}", style.paint(BOLD, &text(unit.get("kind"))));
    if let Some(name) = unit.get("name").as_str() {
        header.push(' ');
        header.push_str(&style.paint(BOLD, &clean(name)));
    }
    let _ = write!(header, " L{first}–{last}");
    let changed: Vec<String> = unit
        .get("changed_lines")
        .as_arr()
        .iter()
        .filter_map(Json::as_u64)
        .map(|line| line.to_string())
        .collect();
    if !changed.is_empty() {
        header.push_str(&style.paint(YELLOW, &format!("  changed {}", changed.join(", "))));
    }
    if let Some(name) = unit.get("referenced_name").as_str() {
        let _ = write!(header, "  references {}", clean(name));
    }
    lines.push(header);
    for outer in unit.get("enclosing").as_arr() {
        let mut inside = format!("in {}", text(outer.get("kind")));
        if let Some(name) = outer.get("name").as_str() {
            let _ = write!(inside, " {}", clean(name));
        }
        let _ = write!(inside, ": {}", text(outer.get("header")));
        lines.push(format!("      {}", style.paint(DIM, &inside)));
    }
    let mut notes = Vec::new();
    if unit.get("text_truncated").as_bool() == Some(true) {
        notes.push("text truncated".to_owned());
    }
    match unit.get("omitted_reference_units").as_u64() {
        Some(0) | None => {}
        Some(count) => notes.push(format!("{count} more referencing units omitted")),
    }
    if unit.get("overlaps_syntax_error").as_bool() == Some(true) {
        notes.push("overlaps a syntax error".to_owned());
    }
    for gap in unit.get("gaps_between_lines").as_arr() {
        let (before, after) = (
            gap.as_arr().first().and_then(Json::as_u64).unwrap_or(0),
            gap.as_arr().get(1).and_then(Json::as_u64).unwrap_or(0),
        );
        notes.push(format!("changed between lines {before} and {after}"));
    }
    if !notes.is_empty() {
        lines.push(format!("      {}", style.paint(DIM, &notes.join(" · "))));
    }
    // line → (text, comment); the unit's own lines win over a comment's partial first line.
    let mut shown: BTreeMap<usize, (String, bool)> = BTreeMap::new();
    let mut comment_lines = BTreeSet::new();
    let mut changed: BTreeSet<usize> = unit
        .get("changed_lines")
        .as_arr()
        .iter()
        .filter_map(|line| line.as_u64().map(|line| line as usize))
        .collect();
    let comments = unit.get("comments").as_arr();
    for comment in comments {
        let (from, to) = lines_of(comment.get("range"));
        comment_lines.extend(from..=to);
        if comment.get("changed").as_bool() == Some(true) {
            changed.extend(from..=to);
        }
    }
    for (index, line) in code_lines(unit.get("text").as_str().unwrap_or_default())
        .into_iter()
        .enumerate()
    {
        let number = first + index;
        shown.insert(number, (line, comment_lines.contains(&number)));
    }
    for comment in comments {
        let (from, _) = lines_of(comment.get("range"));
        for (index, line) in code_lines(comment.get("text").as_str().unwrap_or_default())
            .into_iter()
            .enumerate()
        {
            shown.entry(from + index).or_insert((line, true));
        }
    }
    let width = shown
        .keys()
        .next_back()
        .map_or(1, |last| last.to_string().len());
    let mut previous = None;
    for (number, (code, comment)) in shown {
        if previous.is_some_and(|previous| number > previous + 1) {
            lines.push(format!("      {:>width$} {}", "", style.paint(DIM, "┆")));
        }
        previous = Some(number);
        let mark = if changed.contains(&number) {
            style.paint(YELLOW, "~")
        } else {
            " ".to_owned()
        };
        let code = if comment {
            style.paint(CYAN, &code)
        } else {
            code
        };
        lines.push(format!(
            "      {}{mark}{} {code}",
            style.paint(DIM, &format!("{number:>width$}")),
            style.paint(DIM, "│")
        ));
    }
}

/// `rotter extract`: a header, one section per file with its units and their code, untracked
/// files not covered, and a summary footer.
pub fn extract(report: &Json, color: bool) -> String {
    let style = Style(color);
    let mut header = vec![style.paint(BOLD, &text(report.get("mode")))];
    let before = report.get("before");
    if before != &Json::Null {
        let commit = before.get("commit").as_str().map_or_else(
            || "empty".to_owned(),
            |commit| clean(&commit.chars().take(12).collect::<String>()),
        );
        header.push(format!(
            "{} ({commit}) → {}",
            text(before.get("rev")),
            text(report.get("after"))
        ));
    } else {
        header.push(text(report.get("after")));
    }
    header.push(text(report.get("repository")));
    let mut lines = vec![header.join("  ")];
    let files = report.get("files").as_arr();
    let mut units = 0;
    for file in files {
        lines.push(String::new());
        let path = match (file.get("old_path").as_str(), file.get("new_path").as_str()) {
            (Some(old), Some(new)) if old != new => format!("{} → {}", clean(old), clean(new)),
            (_, Some(path)) | (Some(path), None) => clean(path),
            (None, None) => String::new(),
        };
        let mut change = text(file.get("change"));
        if let Some(similarity) = file.get("similarity").as_u64() {
            let _ = write!(change, " {similarity}%");
        }
        let sides: Vec<(&str, &Json)> =
            [("before", file.get("before")), ("after", file.get("after"))]
                .into_iter()
                .filter(|(_, side)| *side != &Json::Null)
                .collect();
        let language = sides
            .iter()
            .rev()
            .find_map(|(_, side)| side.get("language").as_str())
            .map(clean);
        let statuses: Vec<&str> = sides
            .iter()
            .map(|(_, side)| side.get("status").as_str().unwrap_or_default())
            .collect();
        let status = if statuses.windows(2).all(|pair| pair[0] == pair[1]) {
            statuses
                .first()
                .map(|status| style.paint(status_color(status), &clean(status)))
                .unwrap_or_default()
        } else {
            sides
                .iter()
                .zip(&statuses)
                .map(|((name, _), status)| {
                    format!(
                        "{name} {}",
                        style.paint(status_color(status), &clean(status))
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let complete = file.get("complete").as_bool() == Some(true);
        let mut title = format!(
            "{} {}  {change}",
            style.paint(if complete { GREEN } else { YELLOW }, "■"),
            style.paint(BOLD, &path)
        );
        if let Some(language) = language {
            let _ = write!(title, "  {language}");
        }
        let _ = write!(title, "  {status}");
        lines.push(title);
        for ((name, side), status) in sides.iter().zip(&statuses) {
            if let Some(detail) = side.get("detail").as_str() {
                lines.push(format!(
                    "  {}",
                    style.paint(
                        status_color(status),
                        &format!("{name} {}: {}", clean(status), clean(detail))
                    )
                ));
            }
        }
        for (name, side) in &sides {
            let found = side.get("units").as_arr();
            if found.is_empty() {
                continue;
            }
            units += found.len();
            lines.push(format!("  {}", style.paint(DIM, name)));
            for item in found {
                unit(style, item, &mut lines);
            }
        }
    }
    let not_covered = report.get("untracked").get("not_covered").as_arr();
    if !not_covered.is_empty() {
        lines.push(String::new());
        lines.push(style.paint(YELLOW, "untracked, not covered:"));
        for path in not_covered {
            lines.push(format!("  {}", text(path)));
        }
    }
    lines.push(String::new());
    let complete = report.get("complete").as_bool() == Some(true);
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    lines.push(format!(
        "{} file{} · {units} unit{} · {}",
        files.len(),
        plural(files.len()),
        plural(units),
        if complete {
            style.paint(GREEN, "complete")
        } else {
            style.paint(YELLOW, "incomplete")
        }
    ));
    finish(lines)
}

#[cfg(test)]
mod tests {
    use super::{
        Color, clean, code_lines, color_on, extract, integration, parser_install, parsers, status,
    };
    use crate::install::{ParserEntry, ParserInstall, ParserResult};
    use crate::integration::{HostStatus, IntegrationResult, Status};
    use crate::json::Json;
    use std::ffi::OsStr;

    /// ESC CSI, OSC 52 ended by BEL, CR, BS, DEL, C1 CSI and OSC, bidi overrides and a newline.
    const EVIL: &str =
        "\u{1b}[2J\u{1b}]52;c;aGk=\u{7}\r\u{8}\u{7f}\u{9b}\u{9d}\u{202e}\u{2066}\u{200b}\u{feff}\n";

    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    /// A `Json` from a `serde_json` value, keys in order.
    fn json(value: serde_json::Value) -> Json {
        use serde_json::Value;
        match value {
            Value::Null => Json::Null,
            Value::Bool(value) => Json::Bool(value),
            Value::Number(value) => Json::Num(value.as_u64().unwrap()),
            Value::String(value) => Json::Str(value),
            Value::Array(items) => Json::Arr(items.into_iter().map(json).collect()),
            Value::Object(fields) => Json::Obj(
                fields
                    .into_iter()
                    .map(|(key, value)| (leak(key), json(value)))
                    .collect(),
            ),
        }
    }

    /// `text` without SGR runs (`ESC [ <digits and ;> m`).
    fn strip_sgr(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("\u{1b}[") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find(|c: char| !(c.is_ascii_digit() || c == ';'))
                .unwrap_or(after.len());
            if after[end..].starts_with('m') {
                rest = &after[end + 1..];
            } else {
                out.push_str("\u{1b}[");
                rest = after;
            }
        }
        out.push_str(rest);
        out
    }

    /// No C1, bidi or zero-width character and no ESC, BEL, BS, CR or DEL except in rotter's
    /// own SGR runs (none at all without colour), every injected newline escaped.
    fn assert_inert(text: &str, color: bool) {
        if !color {
            assert!(!text.contains('\u{1b}'), "{text:?}");
        }
        for c in strip_sgr(text).chars() {
            assert!(
                !matches!(c, '\u{80}'..='\u{9f}' | '\u{1b}' | '\u{7}' | '\u{8}' | '\r' | '\u{7f}'
                    | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}'
                    | '\u{61c}' | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'),
                "raw {c:?} in {text:?}"
            );
        }
        assert!(text.contains(r"\u{1b}[2J\u{1b}]52;c;aGk=\u{7}\u{d}\u{8}\u{7f}\u{9b}\u{9d}"));
        assert!(
            text.contains(r"\u{202e}\u{2066}\u{200b}\u{feff}\u{a}"),
            "{text}"
        );
    }

    /// `render` of the evil values against the same with plain ones: inert in both colour
    /// modes, and no injected newline adds a line.
    fn check_injection(render: impl Fn(&str, bool) -> String) {
        for color in [false, true] {
            let evil = render(EVIL, color);
            assert_inert(&evil, color);
            assert_eq!(
                evil.lines().count(),
                render("x", color).lines().count(),
                "{evil}"
            );
        }
    }

    #[test]
    fn colour_only_on_a_terminal_without_no_color_or_a_dumb_term() {
        let dumb = Some(OsStr::new("dumb"));
        let xterm = Some(OsStr::new("xterm-256color"));
        let set = Some(OsStr::new("1"));
        let empty = Some(OsStr::new(""));
        for (choice, terminal, no_color, term, on) in [
            (Color::Auto, true, None, xterm, true),
            (Color::Auto, true, None, None, true),
            (Color::Auto, true, empty, xterm, true),
            (Color::Auto, false, None, xterm, false),
            (Color::Auto, true, set, xterm, false),
            (Color::Auto, true, None, dumb, false),
            (Color::Always, false, set, dumb, true),
            (Color::Never, true, None, xterm, false),
        ] {
            assert_eq!(
                color_on(choice, terminal, no_color, term),
                on,
                "{choice:?} {terminal} {no_color:?} {term:?}"
            );
        }
        assert_eq!(Color::parse("always"), Some(Color::Always));
        assert_eq!(Color::parse("Always"), None);
    }

    #[test]
    fn values_are_cleaned_per_line_and_code_keeps_tabs() {
        assert_eq!(
            clean("a\tb\nc\u{1b}\u{9b}\u{202e}é"),
            r"a\u{9}b\u{a}c\u{1b}\u{9b}\u{202e}é"
        );
        assert_eq!(
            code_lines("f() {\r\n\treturn\r\n}\r"),
            ["f() {", "\treturn", "}"]
        );
        assert_eq!(
            code_lines("a\rb\u{7f}\n\u{2067}"),
            [r"a\u{d}b\u{7f}", r"\u{2067}"]
        );
        assert_eq!(code_lines("a\r\r\nb"), [r"a\u{d}", "b"]);
    }

    fn sample_status(value: &str) -> Status {
        let text = |base: &str| format!("{base}{value}");
        Status {
            hosts: vec![
                HostStatus {
                    id: leak(text("claude")),
                    support: leak(text("stable")),
                    state: Some(leak(text("installed"))),
                    path: Some(text("/h/.claude/settings.json")),
                    detail: Some(text("installed (current)")),
                    notes: vec![text("a note")],
                },
                HostStatus {
                    id: "grok",
                    support: "stable",
                    state: Some("mismatch"),
                    path: Some("/h/.grok/hooks/rotter.json".to_owned()),
                    detail: Some(text("installed (timeout 5, expected 90)")),
                    notes: Vec::new(),
                },
                HostStatus {
                    id: "codex",
                    support: "experimental",
                    state: Some("error"),
                    path: None,
                    detail: Some(text("/h/.codex/hooks.json: expected value")),
                    notes: vec![text("hooks feature on")],
                },
                HostStatus {
                    id: "omp",
                    support: "unsupported",
                    state: None,
                    path: None,
                    detail: Some(text("not supported yet (TODO)")),
                    notes: Vec::new(),
                },
            ],
            compat_state: leak(text("found")),
            compat_detail: Some(text("found a Claude entry")),
            git_path: Some(text("/usr/bin/git")),
            git_detail: text("git version 2.50.1"),
            exe_path: Some(text("/bin/rotter")),
            exe_trusted: false,
            exe_detail: text("refusing to register /bin/rotter"),
        }
    }

    const STATUS: &str = "\
host    support       state      path
claude  stable        installed  /h/.claude/settings.json
  a note
grok    stable        mismatch   /h/.grok/hooks/rotter.json
  installed (timeout 5, expected 90)
codex   experimental  error      -
  /h/.codex/hooks.json: expected value
  hooks feature on
omp     unsupported   -          -
  not supported yet (TODO)

grok compat  found         found a Claude entry
git          /usr/bin/git  git version 2.50.1
executable   /bin/rotter   refusing to register /bin/rotter
";

    #[test]
    fn status_renders_aligned_and_inert() {
        let text = status(&sample_status(""), false);
        assert_eq!(text, STATUS);
        let colored = status(&sample_status(""), true);
        assert!(
            colored.contains("\u{1b}[32minstalled\u{1b}[0m"),
            "{colored}"
        );
        assert_eq!(strip_sgr(&colored), text);
        check_injection(|value, color| status(&sample_status(value), color));
    }

    fn sample_integration(value: &str) -> IntegrationResult {
        IntegrationResult {
            host: leak(format!("claude{value}")),
            action: "uninstall",
            result: leak(format!("removed{value}")),
            path: format!("/h/.claude/settings.json{value}"),
            notes: vec![format!(
                "removed /h/.claude/settings.json.rotter-bak{value}"
            )],
        }
    }

    const INTEGRATION: &str = "\
claude  removed  /h/.claude/settings.json
  removed /h/.claude/settings.json.rotter-bak
";

    #[test]
    fn integration_renders_its_result_and_notes() {
        let text = integration(&sample_integration(""), false);
        assert_eq!(text, INTEGRATION);
        check_injection(|value, color| integration(&sample_integration(value), color));
    }

    fn sample_parsers(value: &str) -> Vec<ParserEntry> {
        let text = |base: &str| Some(format!("{base}{value}"));
        vec![
            ParserEntry {
                name: format!("lua2{value}"),
                enabled: true,
                installed: true,
                path: text("/c/rotter/parsers/lua2-1.dylib"),
                source: text("/src/lua"),
                detail: None,
            },
            ParserEntry {
                name: "python".to_owned(),
                enabled: true,
                installed: false,
                path: None,
                source: text("https://example.invalid/python 0123"),
                detail: text("not installed; run `rotter parser install python`"),
            },
            ParserEntry {
                name: "hcl".to_owned(),
                enabled: false,
                installed: false,
                path: None,
                source: None,
                detail: text("add \"hcl\" to languages in config.toml"),
            },
        ]
    }

    const PARSERS: &str = "\
name    state          path
lua2    installed      /c/rotter/parsers/lua2-1.dylib
  source /src/lua
python  not installed  -
  source https://example.invalid/python 0123
  not installed; run `rotter parser install python`
hcl     available      -
  add \"hcl\" to languages in config.toml
";

    #[test]
    fn parser_list_renders_rows_with_sources_and_details() {
        let text = parsers(&sample_parsers(""), false);
        assert_eq!(text, PARSERS);
        check_injection(|value, color| parsers(&sample_parsers(value), color));
    }

    fn sample_install(value: &str) -> ParserInstall {
        let text = |base: &str| format!("{base}{value}");
        ParserInstall {
            parsers: vec![
                ParserResult {
                    name: text("lua2"),
                    result: "installed",
                    path: Some(text("/c/rotter/parsers/lua2-1.dylib")),
                    detail: None,
                },
                ParserResult {
                    name: text("lua3"),
                    result: leak(text("failed")),
                    path: None,
                    detail: Some(text("cc parser.c failed: error: expected ';'")),
                },
                ParserResult {
                    name: "hcl".to_owned(),
                    result: "skipped",
                    path: None,
                    detail: None,
                },
            ],
            notes: vec![text("Note: an installed grammar is C code.")],
            error: Some(text("lua3: cc failed")),
        }
    }

    const INSTALL: &str = "\
lua2  installed  /c/rotter/parsers/lua2-1.dylib
lua3  failed     -
  cc parser.c failed: error: expected ';'
hcl   skipped    -

Note: an installed grammar is C code.
";

    #[test]
    fn parser_install_renders_results_and_the_notice() {
        let text = parser_install(&sample_install(""), false);
        assert_eq!(text, INSTALL);
        check_injection(|value, color| parser_install(&sample_install(value), color));
    }

    /// A report with `value` in every string field and `code` as the unit's text.
    fn report(value: &str, code: &str) -> Json {
        let text = |base: &str| format!("{base}{value}");
        let range = |first: u64, last: u64| {
            serde_json::json!({ "lines": [first, last],
            "bytes": [0, 1] })
        };
        let unit = serde_json::json!({
            "kind": text("function_declaration"), "name": text("F"), "range": range(3, 5),
            "selected_by": "reference", "referenced_name": text("G"),
            "omitted_reference_units": 2, "changed_lines": [4], "gaps_between_lines": [[4, 5]],
            "enclosing": [{ "kind": text("impl_item"), "name": text("T"), "range": range(1, 9),
                "header": text("impl T {") }],
            "overlaps_syntax_error": true, "text": code, "text_truncated": true,
            "comments": [{ "relation": "leading", "style": text("line"),
                "directive": text("go_directive"), "changed": true, "range": range(2, 2),
                "text": text("// F") }]
        });
        let side = |path: &str, status: String, detail: &str| {
            serde_json::json!({
            "path": text(path), "language": text("go"), "dialect": text("go"),
            "blob": text("b"), "status": status, "detail": text(detail) })
        };
        let mut after = side("new.go", "partial".to_owned(), "syntax errors near lines 5");
        after["units"] = serde_json::json!([unit]);
        let file = serde_json::json!({
            "change": text("renamed"), "similarity": 90, "old_path": text("old.go"),
            "new_path": text("new.go"), "complete": false, "hunks": [],
            "before": side("old.go", text("read_error"), "cannot read"), "after": after });
        json(serde_json::json!({
            "tool": "rotter", "schema": "rotter.extract.poc/0", "stage": "extraction",
            "semantic_verification": "not_performed", "mode": text("base"),
            "repository": text("/repo"),
            "before": { "rev": text("main"), "commit": text("0123456789abcdef"),
                "empty_initial": false },
            "pathspec": [text("src")], "after": text("worktree"),
            "untracked": { "included": false, "not_covered": [text("new.txt")] },
            "complete": false, "files": [file]
        }))
    }

    const EXTRACT: &str = "\
base  main (0123456789ab) → worktree  /repo

■ old.go → new.go  renamed 90%  go  before read_error · after partial
  before read_error: cannot read
  after partial: syntax errors near lines 5
  after
    function_declaration F L3–5  changed 4  references G
      in impl_item T: impl T {
      text truncated · 2 more referencing units omitted · overlaps a syntax error · changed between lines 4 and 5
      2~│ // F
      3 │ func F() {
      4~│ \tx := 1 // c
      5 │ }

untracked, not covered:
  new.txt

1 file · 1 unit · incomplete
";

    #[test]
    fn extract_renders_every_field_inert() {
        let code = "func F() {\r\n\tx := 1 // c\r\n}";
        let text = extract(&report("", code), false);
        assert_eq!(text, EXTRACT);
        check_injection(|value, color| extract(&report(value, code), color));
        // The code of a CRLF file shows no CR; a lone CR is escaped.
        assert!(!text.contains(r"\u{d}"), "{text}");
        let lone = extract(&report("", "func F() {\r\tx\r\n}"), false);
        assert!(lone.contains(r"func F() {\u{d}	x"), "{lone}");
    }

    const TIMEOUTS: &str = "\
full  worktree  /repo

■ a.lua2  full  lua2  parser_not_installed
  after parser_not_installed: no library for lua2 in /c/rotter/parsers; run `rotter parser install lua2`

■ big.lua2  full  lua2  parse_timeout
  after parse_timeout: stopped after 60 s; the limit is parse_timeout_seconds in $XDG_CONFIG_HOME/rotter/config.toml

2 files · 0 units · incomplete
";

    /// Timing- and path-bearing statuses are rendered only from constructed reports.
    #[test]
    fn parse_timeout_and_parser_not_installed_render_their_detail() {
        let side = |status: &str, detail: &str| {
            serde_json::json!({ "path": "a.lua2",
            "language": "lua2", "dialect": "lua2", "blob": "b", "status": status,
            "detail": detail })
        };
        let report = json(serde_json::json!({
            "mode": "full", "repository": "/repo", "before": null, "after": "worktree",
            "untracked": { "included": false, "not_covered": [] }, "complete": false,
            "files": [
                { "change": "full", "similarity": null, "old_path": null,
                    "new_path": "a.lua2", "complete": false, "before": null,
                    "after": side("parser_not_installed",
                        "no library for lua2 in /c/rotter/parsers; run `rotter parser install lua2`") },
                { "change": "full", "similarity": null, "old_path": null,
                    "new_path": "big.lua2", "complete": false, "before": null,
                    "after": side("parse_timeout", "stopped after 60 s; the limit is \
                        parse_timeout_seconds in $XDG_CONFIG_HOME/rotter/config.toml") }
            ]
        }));
        let text = extract(&report, false);
        assert_eq!(text, TIMEOUTS);
        let colored = extract(&report, true);
        assert!(
            colored.contains("\u{1b}[31mparse_timeout\u{1b}[0m"),
            "{colored}"
        );
        assert_eq!(strip_sgr(&colored), text);
    }
}
