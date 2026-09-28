use rotter::{Mode, Options, extract};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: rotter extract (--staged | --worktree | --base <rev> | --full)
                      [--include-untracked] [-C <dir>] [-- <pathspec>...]

Prints changed code units and their related comments as JSON. --full reports every commented
unit of the tracked working-tree files instead of a diff. Pathspecs limit any mode.
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
