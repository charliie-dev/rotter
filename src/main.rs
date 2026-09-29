use rotter::config::{self, Config, Sources};
use rotter::json::Json;
use rotter::render::{self, Color, clean};
use rotter::{Git, Mode, Options, extract_with, hosts, install, integration, toplevel};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
Every command but --skill and hook takes [--pretty [--color=(auto | always | never)]].

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
and install letta the mod ~/.letta/mods/rotter-review.js (timeout capped at 120 s, the host
waits); install opencode writes the plugin $OPENCODE_CONFIG_DIR/plugins/rotter-review.js
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
    let mut set = |value: Mode| match mode.replace(value) {
        Some(_) => Err("choose exactly one of --staged, --worktree, --base, --full".to_owned()),
        None => Ok(()),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--staged" => set(Mode::Staged)?,
            "--worktree" => set(Mode::Worktree)?,
            "--full" => set(Mode::Full)?,
            "--" => {
                paths.extend(args.by_ref().cloned());
            }
            "--base" => set(Mode::Base(
                args.next().ok_or("--base needs a revision")?.clone(),
            ))?,
            "--include-untracked" => include_untracked = true,
            "--lang" => {
                let value = args.next().ok_or("--lang needs <glob>=<language>")?;
                let (pattern, name) = value
                    .rsplit_once('=')
                    .ok_or_else(|| format!("--lang needs <glob>=<language>: {value}"))?;
                languages.push((pattern.to_owned(), name.to_owned()));
            }
            "-C" => dir = args.next().ok_or("-C needs a directory")?.into(),
            other => match other.strip_prefix("--base=") {
                Some(rev) => set(Mode::Base(rev.to_owned()))?,
                None => return Err(format!("unknown argument: {other}")),
            },
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
    let mut index = start;
    while index < args.len() && args[index] != "--" {
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
                None => {
                    index += 1;
                    continue;
                }
            },
        }
        args.remove(index);
    }
    match (pretty, color) {
        (false, Some(_)) => Err("--color applies only with --pretty".into()),
        (false, None) => Ok(None),
        (true, color) => Ok(Some(color.unwrap_or(Color::Auto))),
    }
}

/// Loads the config for a CLI command; an untrusted config is reported and ignored.
fn cli_config(repo: Option<&Path>) -> Result<Config, String> {
    let loaded = config::load(repo, &Sources::from_env())?;
    if let Some(note) = loaded.note {
        eprintln!("rotter: {}", clean(&note));
    }
    Ok(loaded.config)
}

/// Applies the config and resolves `--lang` names; `--lang` globs come before `[overrides]`.
fn configure(
    git: &Git,
    dir: &Path,
    options: &mut Options,
    languages: LangArgs,
) -> Result<(), String> {
    let config = cli_config(Some(&toplevel(git, dir)?))?;
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

/// Writes `text` to stdout. A reader that closed the pipe early (`rotter extract | head`) is not an
/// error, so the run keeps the exit status it earned; any other write failure is reported and
/// becomes exit 2.
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

fn main() -> ExitCode {
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let Ok(mut args) = args else {
        eprintln!("rotter: arguments must be UTF-8");
        // Even a malformed `rotter hook …` must not fail the host's turn.
        let hook = std::env::args_os().nth(1).is_some_and(|arg| arg == "hook");
        return if hook {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        };
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
    // The document, or with --pretty its rendering (Some(colour)).
    let show = |json: &Json, rendered: &dyn Fn(bool) -> String| match pretty {
        Some(color) => rendered(color),
        None => format!("{json}\n"),
    };
    let result: Option<Result<(String, Option<String>), String>> = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--skill"] => {
            return emit(SKILL, ExitCode::SUCCESS);
        }
        ["hook", rest @ ..] => {
            // A hook must never fail the host's turn: every `rotter hook …` exits 0, even on a
            // panic, and problems surface as a systemMessage or on stderr, per host.
            let _ = std::panic::catch_unwind(|| match rest {
                [name, rest @ ..]
                    if let Some(host) = hosts::by_hook(name)
                        && matches!(rest, [] | ["--timeout", _]) =>
                {
                    // A shim's own timeout; anything but a positive integer is ignored.
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
            return ExitCode::SUCCESS;
        }
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
    };
    if let Some(result) = result {
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
        return match error {
            Some(error) => {
                eprintln!("rotter: {}", clean(&error));
                ExitCode::from(2)
            }
            None => ExitCode::SUCCESS,
        };
    }
    // Arguments after `--` are pathspecs, never options.
    if args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "-h" || arg == "--help")
    {
        return emit(&format!("{USAGE}\n"), ExitCode::SUCCESS);
    }
    let (dir, mut options, languages) = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("rotter: {}\n{USAGE}", clean(&error));
            return ExitCode::from(2);
        }
    };
    let git = match Git::cli(&dir) {
        Ok(git) => git,
        Err(error) => {
            eprintln!("rotter: {}", clean(&error));
            return ExitCode::from(2);
        }
    };
    if let Err(error) = configure(&git, &dir, &mut options, languages) {
        eprintln!("rotter: {}", clean(&error));
        return ExitCode::from(2);
    }
    match extract_with(&git, &dir, &options) {
        Ok(report) => {
            let text = show(&report.json, &|color| render::extract(&report.json, color));
            if text.len() > 5 << 20 {
                eprintln!(
                    "rotter: report is {} MiB; consider limiting it with -- <pathspec>",
                    text.len() >> 20
                );
            }
            emit(&text, ExitCode::from(if report.complete { 0 } else { 1 }))
        }
        Err(error) => {
            eprintln!("rotter: {}", clean(&error));
            ExitCode::from(2)
        }
    }
}
