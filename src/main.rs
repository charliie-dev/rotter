use rotter::config::{self, Config, Sources};
use rotter::json::Json;
use rotter::render::{self, Color, clean};
use rotter::{
    Git, Mode, Options, Report, extract_with, git_dir, hosts, install, integration, mcp, toplevel,
};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// The review skill ships inside the binary so it always matches this version of the CLI.
const SKILL: &str = include_str!("../skills/rotter-comment-review/SKILL.md");

const USAGE: &str = "usage: rotter --skill
       rotter integration (install | uninstall)
                          (claude | grok | codex | copilot | droid | pi | letta | opencode)
       rotter integration status
       rotter hook (claude | claude-stop | grok | grok-stop | codex | copilot | droid | pi | letta
                   | opencode) [--timeout <seconds>]
       rotter parser install [<name>...]
       rotter parser list
       rotter extract (--staged | --worktree | --base <rev> | --full)
                      [--include-untracked] [--lang <glob>=<language>]... [-C <dir>]
                      [-- <pathspec>...]
       rotter mcp [--root <dir>]...
Every command but --skill, hook and mcp takes [--pretty [--color=(auto | always | never)]].

mcp serves the Model Context Protocol (version 2026-07-28 only) on stdin/stdout: a read-only
`extract` tool, run like `rotter extract` with its stderr notes returned as text content, and
the review skill as the prompt `rotter-comment-review`. The tool reads only repositories whose
top level is inside a --root (default: the repository of the working directory, or the working
directory outside a repository). Once stdin closes, it answers the requests already read and
exits.

--skill prints the comment review skill for coding agents (it matches this binary's version).
integration install claude registers `'<rotter>' hook claude-stop || true` as a Claude Code Stop
hook in $CLAUDE_CONFIG_DIR/settings.json (default ~/.claude), keeping a .rotter-bak copy that
uninstall removes. integration install grok writes `'<rotter>' hook grok-stop || true` to
$GROK_HOME/hooks/rotter.json (default ~/.grok; the home must exist), a file rotter owns. Their
timeout is max(60, parse_timeout_seconds + 30); re-run install after changing
parse_timeout_seconds. install codex merges `hook codex` into $CODEX_HOME/hooks.json (default
~/.codex; trust it in Codex's /hooks), install copilot writes $COPILOT_HOME/hooks/rotter.json
(default ~/.copilot) and install droid merges `hook droid` into ~/.factory/hooks.json. install
pi writes the extension $PI_CODING_AGENT_DIR/extensions/rotter-review.ts (default ~/.pi/agent)
and install letta the mod ~/.letta/mods/rotter-review.js (for pi and letta the timeout is capped
at 120 s, the host waits); install opencode writes the plugin $OPENCODE_CONFIG_DIR/plugins/rotter-review.js
(default $XDG_CONFIG_HOME/opencode, else ~/.config/opencode), which re-prompts the session with a
visible message. Each runs `hook <host> --timeout <n>` with only PATH and LANG. codex, droid,
pi, letta and opencode are experimental. A host directory inside a git work tree is refused.
status shows every host, including the unsupported ones and why. `hook claude` and `hook grok`
are the same hooks as `hook claude-stop` and `hook grok-stop`; every `rotter hook ...` exits 0.

parser install fetches each enabled external grammar at its pinned commit (or copies its local
path), compiles it with cc and caches it under $XDG_CACHE_HOME/rotter/parsers (default
~/.cache). Nothing else downloads or compiles. parser list shows enabled and available grammars.

Prints changed code units and their related comments as JSON. --full reports every commented
unit of the tracked working-tree files instead of a diff. Pathspecs limit any mode.
--lang parses files whose repository-relative path matches <glob> as <language> (go, lua, nix,
bash, sh, yaml, toml, rust, or an enabled external language), ahead of config overrides and
extension and shebang detection; `*` stays within one directory, `**` crosses directories. The
first matching --lang wins.
Config: $XDG_CONFIG_HOME/rotter/config.toml (default ~/.config) sets parse_timeout_seconds
(default 60), languages = [..] registry grammars to enable, [language.<name>] external grammars
and [overrides] <glob> = <language>.
Exit status: 0 complete, 1 printed but incomplete (unreadable, unparsed or unsupported files), 2 error.

Output is one indented JSON document on stdout (extract: rotter.extract.poc/0; integration
status: rotter.status/1; install and uninstall: rotter.integration/1; parser list:
rotter.parsers/1; parser install: rotter.parser_install/1). JSON strings escape only characters
below U+0020, `\"` and `\\`: DEL and C1 characters pass through raw, so render values safely.
--pretty prints an aligned, human-readable rendering instead, with control, bidi and zero-width
characters shown as \\u{..}; only the flag selects it. --color (auto by default) colours it when
auto finds stdout a terminal, NO_COLOR unset or empty and TERM not dumb; always and never force
it. For extract these flags go before --. Errors are `rotter: <message>` on stderr with nothing
on stdout, except that a failed parser install still prints its document.";

/// `--lang` values, resolved once the config says which external languages exist.
type LangArgs = Vec<(String, String)>;

/// The extract options that take the next argument as their value; that value is never a flag,
/// even when it looks like one (`-C --pretty` names a directory).
const VALUE_OPTIONS: [&str; 3] = ["--base", "--lang", "-C"];

const LARGE_REPORT: usize = 5 << 20;

/// `narrow` names how the caller limits a report: `-- <pathspec>` on the CLI, `paths` over MCP.
fn size_note(len: usize, narrow: &str) -> Option<String> {
    (len > LARGE_REPORT).then(|| {
        format!(
            "report is {} MiB; consider limiting it with {narrow}",
            len >> 20
        )
    })
}

/// Indexes of extract's arguments in flag position: after `extract`, before the first `--` that
/// is not an option's value, and not an option's value themselves.
///
/// One pass over `args` with no index arithmetic, so no change to it can loop forever.
fn flag_positions(args: &[String]) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut value_next = false;
    for (index, arg) in args.iter().enumerate().skip(1) {
        if value_next {
            value_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        positions.push(index);
        value_next = VALUE_OPTIONS.contains(&arg.as_str());
    }
    positions
}

fn parse_args(args: &[String]) -> Result<(PathBuf, Options, LangArgs), String> {
    let mut args = args.iter();
    if args.next().map(String::as_str) != Some("extract") {
        return Err("expected the extract command".into());
    }
    let mut mode = None;
    let mut include_untracked = false;
    let mut dir = PathBuf::from(".");
    let mut paths = Vec::new();
    let mut languages = Vec::new();
    while let Some(arg) = args.next() {
        if let Some(chosen) = mode_arg(arg, &mut args)? {
            if mode.replace(chosen).is_some() {
                return Err("choose exactly one of --staged, --worktree, --base, --full".into());
            }
            continue;
        }
        match arg.as_str() {
            "--" => paths.extend(args.by_ref().cloned()),
            "--include-untracked" => include_untracked = true,
            "--lang" => languages.push(lang_arg(args.next())?),
            "-C" => dir = args.next().ok_or("-C needs a directory")?.into(),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let mode = mode.ok_or("choose one of --staged, --worktree, --base, --full")?;
    if include_untracked && matches!(mode, Mode::Staged) {
        return Err("--include-untracked does not apply to --staged".into());
    }
    let mut options = Options::new(mode);
    options.include_untracked = include_untracked;
    options.paths = paths;
    Ok((dir, options, languages))
}

fn mode_arg(arg: &str, rest: &mut std::slice::Iter<'_, String>) -> Result<Option<Mode>, String> {
    Ok(Some(match arg {
        "--staged" => Mode::Staged,
        "--worktree" => Mode::Worktree,
        "--full" => Mode::Full,
        "--base" => Mode::Base(rest.next().ok_or("--base needs a revision")?.clone()),
        other => match other.strip_prefix("--base=") {
            Some(rev) => Mode::Base(rev.to_owned()),
            None => return Ok(None),
        },
    }))
}

fn lang_arg(value: Option<&String>) -> Result<(String, String), String> {
    let value = value.ok_or("--lang needs <glob>=<language>")?;
    let (pattern, name) = value
        .rsplit_once('=')
        .ok_or_else(|| format!("--lang needs <glob>=<language>: {value}"))?;
    Ok((pattern.to_owned(), name.to_owned()))
}

/// Takes `--pretty` and `--color=<when>` out of `args` for the commands that print a document:
/// only after the subcommand words and, for extract, before `--`; `hook`, `--skill` and anything
/// else keep their arguments. Returns the colour choice when `--pretty` was given.
fn output_flags(args: &mut Vec<String>) -> Result<Option<Color>, String> {
    let start = match args.first().map(String::as_str) {
        Some("extract") => 1,
        Some("integration" | "parser") => 2,
        _ => return Ok(None),
    };
    let (mut pretty, mut color) = (false, None);
    let positions = if start == 1 {
        flag_positions(args)
    } else {
        (start..args.len())
            .take_while(|&index| args[index] != "--")
            .collect()
    };
    let mut taken = Vec::new();
    for index in positions {
        match args[index].as_str() {
            "--pretty" => pretty = true,
            "--color" => {
                return Err("--color takes its value as --color=<auto|always|never>".into());
            }
            arg => match arg.strip_prefix("--color=") {
                Some(value) => {
                    color = Some(
                        Color::parse(value)
                            .ok_or_else(|| format!("unknown --color value: {value}"))?,
                    );
                }
                None => continue,
            },
        }
        taken.push(index);
    }
    for index in taken.into_iter().rev() {
        args.remove(index);
    }
    match (pretty, color) {
        (false, Some(_)) => Err("--color applies only with --pretty".into()),
        (false, None) => Ok(None),
        (true, color) => Ok(Some(color.unwrap_or(Color::Auto))),
    }
}

/// An untrusted config is reported through `note` and ignored.
fn load_config(repo: Option<&Path>, note: &mut dyn FnMut(String)) -> Result<Config, String> {
    let loaded = config::load(repo, &Sources::from_env())?;
    if let Some(text) = loaded.note {
        note(clean(&text).to_string());
    }
    Ok(loaded.config)
}

fn cli_config(repo: Option<&Path>) -> Result<Config, String> {
    load_config(repo, &mut |note| eprintln!("rotter: {note}"))
}

/// `--lang` globs come before `[overrides]`.
fn configure(
    repo: &Path,
    options: &mut Options,
    languages: LangArgs,
    note: &mut dyn FnMut(String),
) -> Result<(), String> {
    let config = load_config(Some(repo), note)?;
    options.grammars = config.languages();
    options.parse_timeout = config.parse_timeout();
    for (pattern, name) in languages {
        let (grammar, dialect) = options
            .grammars
            .by_name(&name)
            .ok_or_else(|| format!("unknown --lang language: {name}"))?;
        options.languages.push((pattern, grammar, dialect));
    }
    options
        .languages
        .extend(config.override_languages(&options.grammars));
    Ok(())
}

/// What `rotter extract` and the MCP `extract` tool share once the arguments are parsed. `allow`
/// checks the repository before rotter's config is loaded for it, and extraction refuses a top
/// level other than the one checked.
fn extract_parsed(
    (dir, mut options, languages): (PathBuf, Options, LangArgs),
    allow: &dyn Fn(&Git, &Path) -> Result<(), String>,
    note: &mut dyn FnMut(String),
) -> Result<Report, String> {
    let git = Git::cli(&dir)?;
    let top = toplevel(&git, &dir)?;
    allow(&git, &top)?;
    configure(&top, &mut options, languages, note)?;
    options.expect_top = Some(top);
    extract_with(&git, &dir, &options)
}

fn mcp_extract(
    roots: &[PathBuf],
    args: Vec<String>,
    cancel: Arc<AtomicBool>,
) -> Result<(String, Vec<String>), String> {
    let (dir, mut options, languages) = parse_args(&args)?;
    // Resolved once and checked before git runs there; every later step uses this path. Both
    // failures read the same and name the directory only as it was given (then the roots), so a
    // call cannot tell whether a path outside the roots exists or where a link leads.
    let refused = || {
        format!(
            "{} is not a directory inside the roots this server may read: {}",
            dir.display(),
            roots_text(roots)
        )
    };
    let dir = canonical(&dir).map_err(|_| refused())?;
    within(roots, &dir).map_err(|_| refused())?;
    options.cancel = Some(cancel);
    let allow = |git: &Git, top: &Path| check_repository(roots, git, top);
    let mut notes = Vec::new();
    let report = extract_parsed((dir, options, languages), &allow, &mut |note| {
        notes.push(note)
    })?;
    let text = report.json.to_string();
    notes.extend(size_note(text.len(), "paths"));
    Ok((text, notes))
}

/// The top level must be inside a root, and so must the git directory, unless the git directory
/// is the entry of a linked worktree that its repository registered for this very top level.
fn check_repository(roots: &[PathBuf], git: &Git, top: &Path) -> Result<(), String> {
    within(roots, top)?;
    let git_dir = git_dir(git, top)?;
    if within(roots, &git_dir).is_ok() || registered_worktree(&git_dir, top) {
        return Ok(());
    }
    Err(format!(
        "the git directory {} of {} is outside the roots this server may read",
        git_dir.display(),
        top.display()
    ))
}

/// Git writes a linked worktree's `.git` path into the worktree entry's `gitdir` file; a `.git`
/// file someone points at another repository has no such entry pointing back. Git writes that
/// `.git` as a regular file, so a symlink in its place is not what git registered.
fn registered_worktree(git_dir: &Path, top: &Path) -> bool {
    let is_file = std::fs::symlink_metadata(top.join(".git")).is_ok_and(|meta| meta.is_file());
    let Ok(back) = std::fs::read_to_string(git_dir.join("gitdir")) else {
        return false;
    };
    // A relative entry (`git worktree add --relative-paths`) is relative to the entry's directory.
    let back = git_dir.join(back.trim_end());
    let worktree = back.parent().map(canonical);
    is_file
        && back.file_name() == Some(std::ffi::OsStr::new(".git"))
        && worktree.is_some_and(|worktree| worktree.is_ok() && worktree == canonical(top))
}

fn within(roots: &[PathBuf], path: &Path) -> Result<(), String> {
    let path = canonical(path)?;
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }
    Err(format!(
        "{} is outside the roots this server may read: {}",
        path.display(),
        roots_text(roots)
    ))
}

fn roots_text(roots: &[PathBuf]) -> String {
    let roots: Vec<String> = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect();
    roots.join(", ")
}

fn canonical(dir: &Path) -> Result<PathBuf, String> {
    dir.canonicalize()
        .map_err(|error| format!("{}: {error}", dir.display()))
}

/// `--root <dir>`, repeated. Without one, the repository of the working directory, or the working
/// directory itself when git finds no repository there.
fn mcp_roots(rest: &[&str]) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for pair in rest.chunks(2) {
        match pair {
            ["--root", dir] => {
                let root = canonical(Path::new(dir))?;
                if !root.is_dir() {
                    return Err(format!("--root {dir} is not a directory"));
                }
                roots.push(root);
            }
            ["--root"] => return Err("--root needs a directory".into()),
            other => return Err(format!("unexpected mcp argument: {}", other[0])),
        }
    }
    if roots.is_empty() {
        let cwd = std::env::current_dir()
            .map_err(|error| format!("cannot read the working directory: {error}"))?;
        let found = Git::cli(&cwd).and_then(|git| {
            let top = toplevel(&git, &cwd)?;
            let git_dir = git_dir(&git, &top)?;
            Ok((top, git_dir))
        });
        match found {
            Ok((top, git_dir)) => {
                let top = canonical(&top)?;
                // A submodule or a `--separate-git-dir` repository keeps its git directory
                // outside its top level; the default root has to admit that repository too.
                let git_dir = canonical(&git_dir)?;
                if !git_dir.starts_with(&top) {
                    roots.push(git_dir);
                }
                roots.push(top);
            }
            Err(_) => roots.push(canonical(&cwd)?),
        }
    }
    Ok(roots)
}

/// A reader that closed the pipe early (`rotter extract | head`) is not an error, so the run keeps
/// the exit status it earned.
fn emit(text: &str, earned: ExitCode) -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => earned,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => earned,
        Err(error) => {
            eprintln!("rotter: cannot write output: {}", clean(&error.to_string()));
            ExitCode::from(2)
        }
    }
}

/// Renders a document: the JSON, or with --pretty the rendering for the given colour choice.
type Show<'a> = &'a dyn Fn(&Json, &dyn Fn(bool) -> String) -> String;

fn main() -> ExitCode {
    let mut args = match utf8_args() {
        Ok(args) => args,
        Err(code) => return code,
    };
    let pretty = match output_flags(&mut args) {
        Ok(choice) => choice.map(|choice| {
            render::color_on(
                choice,
                std::io::stdout().is_terminal(),
                std::env::var_os("NO_COLOR").as_deref(),
                std::env::var_os("TERM").as_deref(),
            )
        }),
        Err(error) => {
            eprintln!("rotter: {}\n{USAGE}", clean(&error));
            return ExitCode::from(2);
        }
    };
    let show = |json: &Json, rendered: &dyn Fn(bool) -> String| match pretty {
        Some(color) => rendered(color),
        None => format!("{json}\n"),
    };
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["--skill"] => emit(SKILL, ExitCode::SUCCESS),
        ["hook", rest @ ..] => hook(rest),
        ["mcp", rest @ ..] => mcp_command(rest),
        words => match document(words, &show) {
            Some(result) => finish(result),
            None => extract_command(&args, &show),
        },
    }
}

fn utf8_args() -> Result<Vec<String>, ExitCode> {
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    args.map_err(|_| {
        eprintln!("rotter: arguments must be UTF-8");
        // Even a malformed `rotter hook …` must not fail the host's turn.
        let hook = std::env::args_os().nth(1).is_some_and(|arg| arg == "hook");
        if hook {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        }
    })
}

fn mcp_command(rest: &[&str]) -> ExitCode {
    if rest.iter().any(|arg| *arg == "-h" || *arg == "--help") {
        return emit(&format!("{USAGE}\n"), ExitCode::SUCCESS);
    }
    let roots = match mcp_roots(rest) {
        Ok(roots) => roots,
        Err(error) => {
            eprintln!("rotter: {}\n{USAGE}", clean(&error));
            return ExitCode::from(2);
        }
    };
    let extract = |args, cancel| mcp_extract(&roots, args, cancel);
    let input = std::io::BufReader::new(std::io::stdin());
    match mcp::serve(input, std::io::stdout().lock(), &extract, SKILL) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rotter: mcp: {}", clean(&error.to_string()));
            ExitCode::from(2)
        }
    }
}

fn hook(rest: &[&str]) -> ExitCode {
    // A hook must never fail the host's turn: every `rotter hook …` exits 0, even on a
    // panic, and problems surface as a systemMessage or on stderr, per host.
    let _ = std::panic::catch_unwind(|| match rest {
        [name, rest @ ..]
            if let Some(host) = hosts::by_hook(name)
                && matches!(rest, [] | ["--timeout", _]) =>
        {
            // A shim's own timeout.
            let timeout = match rest {
                ["--timeout", value] => value.parse().ok().filter(|value| *value > 0),
                _ => None,
            };
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            integration::stop(host, &input, &Sources::for_host(host), timeout);
        }
        _ => eprintln!(
            "rotter: unknown hook {:?}; available: {}",
            rest.join(" "),
            hosts::names()
        ),
    });
    ExitCode::SUCCESS
}

/// The commands that print one document: the text to print and an error to report after it,
/// or None when `words` names none of them.
fn document(words: &[&str], show: Show<'_>) -> Option<Result<(String, Option<String>), String>> {
    match words {
        ["integration", "install", name] => Some(
            cli_config(None)
                .and_then(|config| integration::install(name, config.parse_timeout_seconds))
                .map(|done| {
                    let text = show(&done.json(), &|color| render::integration(&done, color));
                    (text, None)
                }),
        ),
        ["parser", "install", names @ ..] => {
            Some(config::load(None, &Sources::from_env()).and_then(|loaded| {
                // Installing needs the user's own config; a refused one is an error here.
                if let Some(note) = loaded.note {
                    return Err(note);
                }
                let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
                let done = install::install(&loaded.config, &names)?;
                // A failure after the first grammar keeps the document of what was done.
                let text = show(&done.json(), &|color| render::parser_install(&done, color));
                Ok((text, done.error))
            }))
        }
        ["parser", "list"] => Some(cli_config(None).map(|config| {
            let parsers = install::list(&config);
            let text = show(&install::list_json(&parsers), &|color| {
                render::parsers(&parsers, color)
            });
            (text, None)
        })),
        ["integration", "uninstall", name] => Some(integration::uninstall(name).map(|done| {
            let text = show(&done.json(), &|color| render::integration(&done, color));
            (text, None)
        })),
        ["integration", "status"] => Some(
            cli_config(None)
                .and_then(|config| integration::status(config.parse_timeout_seconds))
                .map(|status| {
                    let text = show(&status.json(), &|color| render::status(&status, color));
                    (text, None)
                }),
        ),
        _ => None,
    }
}

fn finish(result: Result<(String, Option<String>), String>) -> ExitCode {
    let error = match result {
        Ok((text, error)) => {
            let written = emit(&text, ExitCode::SUCCESS);
            if written != ExitCode::SUCCESS {
                return written;
            }
            error
        }
        Err(error) => Some(error),
    };
    match error {
        Some(error) => {
            eprintln!("rotter: {}", clean(&error));
            ExitCode::from(2)
        }
        None => ExitCode::SUCCESS,
    }
}

/// `rotter extract …` (and help or a usage error for anything else).
fn extract_command(args: &[String], show: Show<'_>) -> ExitCode {
    // Arguments after `--` are pathspecs and an option's value is never a flag.
    let flags = if args.first().map(String::as_str) == Some("extract") {
        flag_positions(args)
    } else {
        (0..args.len())
            .take_while(|&index| args[index] != "--")
            .collect()
    };
    if flags
        .into_iter()
        .any(|index| args[index] == "-h" || args[index] == "--help")
    {
        return emit(&format!("{USAGE}\n"), ExitCode::SUCCESS);
    }
    let (dir, options, languages) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("rotter: {}\n{USAGE}", clean(&error));
            return ExitCode::from(2);
        }
    };
    let parsed = (dir, options, languages);
    match extract_parsed(parsed, &|_, _| Ok(()), &mut |note| {
        eprintln!("rotter: {note}")
    }) {
        Ok(report) => {
            let text = show(&report.json, &|color| render::extract(&report.json, color));
            if let Some(note) = size_note(text.len(), "-- <pathspec>") {
                eprintln!("rotter: {note}");
            }
            emit(&text, ExitCode::from(if report.complete { 0 } else { 1 }))
        }
        Err(error) => {
            eprintln!("rotter: {}", clean(&error));
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mcp_roots, size_note, within};
    use std::path::Path;

    #[test]
    fn only_a_report_over_five_mib_gets_a_size_note() {
        assert_eq!(size_note(0, "paths"), None);
        assert_eq!(size_note(5 << 20, "paths"), None);
        let note = size_note((5 << 20) + 1, "-- <pathspec>").unwrap();
        assert_eq!(
            note,
            "report is 5 MiB; consider limiting it with -- <pathspec>"
        );
        assert_eq!(
            size_note(7 << 20, "paths").unwrap(),
            "report is 7 MiB; consider limiting it with paths"
        );
    }

    #[test]
    fn within_accepts_only_a_top_level_inside_a_root() {
        let base = std::env::temp_dir().join(format!("rotter-within-{}", std::process::id()));
        let root = base.join("root");
        let inside = root.join("repo");
        let outside = base.join("other");
        for dir in [&inside, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let roots = [root.canonicalize().unwrap()];
        assert_eq!(within(&roots, &inside), Ok(()));
        assert_eq!(within(&roots, &root), Ok(()));
        let refused = within(&roots, &outside).unwrap_err();
        assert!(refused.contains("outside the roots"), "{refused}");
        assert!(refused.contains(roots[0].to_str().unwrap()), "{refused}");
        assert!(within(&roots, &base.join("missing")).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn mcp_roots_take_root_pairs_or_default_to_the_working_repository() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let text = temp.to_str().unwrap();
        assert_eq!(
            mcp_roots(&["--root", text, "--root", text]).unwrap(),
            [temp.clone(), temp]
        );
        assert!(
            mcp_roots(&["--root"])
                .unwrap_err()
                .contains("needs a directory")
        );
        assert!(mcp_roots(&["--pretty"]).unwrap_err().contains("unexpected"));
        assert!(mcp_roots(&["--root", "/no/such/rotter/root"]).is_err());
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let refused = mcp_roots(&["--root", file.to_str().unwrap()]).unwrap_err();
        assert!(refused.contains("is not a directory"), "{refused}");
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        // The last root is the top level; a checkout whose git directory lies outside it (a
        // linked worktree, a submodule) adds that directory first.
        assert_eq!(mcp_roots(&[]).unwrap().last(), Some(&repo));
    }
}
