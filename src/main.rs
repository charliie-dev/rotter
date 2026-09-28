use rotter::{Language, Mode, Options, extract, integration};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

/// The review skill ships inside the binary so it always matches this version of the CLI.
const SKILL: &str = include_str!("../skills/rotter-comment-review/SKILL.md");

const USAGE: &str = "usage: rotter --skill
       rotter integration (install | uninstall) claude
       rotter integration status
       rotter hook claude-stop
       rotter extract (--staged | --worktree | --base <rev> | --full)
                      [--include-untracked] [--lang <glob>=<language>]... [-C <dir>]
                      [-- <pathspec>...]

--skill prints the comment review skill for coding agents (it matches this binary's version).
integration install claude registers `rotter hook claude-stop` as a Claude Code Stop hook in
$CLAUDE_CONFIG_DIR/settings.json (default ~/.claude), keeping a .rotter-bak copy.

Prints changed code units and their related comments as JSON. --full reports every commented
unit of the tracked working-tree files instead of a diff. Pathspecs limit any mode.
--lang parses files whose repository-relative path matches <glob> as <language> (go, lua, nix,
bash, sh, yaml, toml, rust), ahead of extension and shebang detection; `*` stays within one
directory, `**` crosses directories. The first matching --lang wins.
Exit status: 0 complete, 1 printed but incomplete (unreadable, unparsed or unsupported files), 2 error.";

fn parse_args(args: &[String]) -> Result<(PathBuf, Options), String> {
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
                let (language, dialect) = Language::from_name(name)
                    .ok_or_else(|| format!("unknown --lang language: {name}"))?;
                languages.push((pattern.to_owned(), language, dialect));
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
    Ok((
        dir,
        Options {
            mode,
            include_untracked,
            paths,
            languages,
        },
    ))
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
        ["hook", "claude-stop"] => {
            // A hook must never fail the host's turn; problems surface as systemMessage.
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            if let Some(output) = integration::claude_stop(&input) {
                println!("{output}");
            }
            return ExitCode::SUCCESS;
        }
        ["integration", "install", name] => Some(integration::install(name)),
        ["integration", "uninstall", name] => Some(integration::uninstall(name)),
        ["integration", "status"] => Some(integration::status()),
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
    let (dir, options) = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("rotter: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
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
