use rotter::config::{self, Config};
use rotter::{Mode, Options, extract, install, integration, toplevel};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The review skill ships inside the binary so it always matches this version of the CLI.
const SKILL: &str = include_str!("../skills/rotter-comment-review/SKILL.md");

const USAGE: &str = "usage: rotter --skill
       rotter integration (install | uninstall) (claude | grok)
       rotter integration status
       rotter hook (claude-stop | grok-stop)
       rotter parser install [<name>...]
       rotter parser list
       rotter extract (--staged | --worktree | --base <rev> | --full)
                      [--include-untracked] [--lang <glob>=<language>]... [-C <dir>]
                      [-- <pathspec>...]

--skill prints the comment review skill for coding agents (it matches this binary's version).
integration install claude registers `'<rotter>' hook claude-stop || true` as a Claude Code Stop
hook in $CLAUDE_CONFIG_DIR/settings.json (default ~/.claude), keeping a .rotter-bak copy that
uninstall removes. integration install grok writes `'<rotter>' hook grok-stop || true` to
$GROK_HOME/hooks/rotter.json (default ~/.grok; the home must exist), a file rotter owns. Their
timeout is max(60, parse_timeout_seconds + 30); re-run install after changing
parse_timeout_seconds. status shows both hosts.

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
Exit status: 0 complete, 1 printed but incomplete (unreadable, unparsed or unsupported files), 2 error.";

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

/// Loads the config for a CLI command; an untrusted config is reported and ignored.
fn cli_config(repo: Option<&Path>) -> Result<Config, String> {
    let loaded = config::load(repo)?;
    if let Some(note) = loaded.note {
        eprintln!("rotter: {note}");
    }
    Ok(loaded.config)
}

/// Applies the config and resolves `--lang` names; `--lang` globs come before `[overrides]`.
fn configure(dir: &Path, options: &mut Options, languages: LangArgs) -> Result<(), String> {
    let config = cli_config(Some(&toplevel(dir)?))?;
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

fn main() -> ExitCode {
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let Ok(args) = args else {
        eprintln!("rotter: arguments must be UTF-8");
        return ExitCode::from(2);
    };
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--skill"] => {
            print!("{SKILL}");
            return ExitCode::SUCCESS;
        }
        ["hook", rest @ ..] => {
            // A hook must never fail the host's turn: every `rotter hook …` exits 0, and problems
            // surface as a systemMessage (Claude) or on stderr (Grok, unknown hooks).
            match integration::Host::from_hook(rest) {
                Some(host) => {
                    let mut input = String::new();
                    let _ = std::io::stdin().read_to_string(&mut input);
                    integration::stop(host, &input);
                }
                None => eprintln!(
                    "rotter: unknown hook {:?}; available: claude-stop, grok-stop",
                    rest.join(" ")
                ),
            }
            return ExitCode::SUCCESS;
        }
        ["integration", "install", name] => Some(
            cli_config(None)
                .and_then(|config| integration::install(name, config.parse_timeout_seconds)),
        ),
        ["parser", "install", names @ ..] => Some(config::load(None).and_then(|loaded| {
            // Installing needs the user's own config; a refused one is an error here.
            if let Some(note) = loaded.note {
                return Err(note);
            }
            let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            install::install(&loaded.config, &names)
        })),
        ["parser", "list"] => Some(cli_config(None).map(|config| install::list(&config))),
        ["integration", "uninstall", name] => Some(integration::uninstall(name)),
        ["integration", "status"] => Some(
            cli_config(None).and_then(|config| integration::status(config.parse_timeout_seconds)),
        ),
        _ => None,
    };
    if let Some(result) = result {
        return match result {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("rotter: {error}");
                ExitCode::from(2)
            }
        };
    }
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let (dir, mut options, languages) = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("rotter: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = configure(&dir, &mut options, languages) {
        eprintln!("rotter: {error}");
        return ExitCode::from(2);
    }
    match extract(&dir, &options) {
        Ok(report) => {
            let text = report.json.to_string();
            if text.len() > 5 << 20 {
                eprintln!(
                    "rotter: report is {} MiB; consider limiting it with -- <pathspec>",
                    text.len() >> 20
                );
            }
            println!("{text}");
            ExitCode::from(if report.complete { 0 } else { 1 })
        }
        Err(error) => {
            eprintln!("rotter: {error}");
            ExitCode::from(2)
        }
    }
}
