//! A native stand-in for git in tests: a native executable that rotter's git selection accepts,
//! doing what `<its own path>.conf` says (the hook clears the environment, so nothing comes from
//! there). Each line is `<key> <value>`:
//! - `log <file>`: append the arguments joined by spaces;
//! - `env <file>`: append `exe=`, `cwd=`, `args=`, `tmpdir=<mode> <uid>` and every environment
//!   variable as `KEY=VALUE`, then `--`;
//! - `marker <file>`: create that file;
//! - `version <text>`: answer `git version` with `<text>`;
//! - `refuse <text>`: exit 3 when the joined arguments contain `<text>`;
//! - `real <git>`: run that git with the same arguments and environment; without it, exit 1.

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, exit};

fn append(path: &str, text: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("helper log");
    file.write_all(text.as_bytes()).expect("helper log");
}

fn main() {
    let exe = std::env::current_exe().expect("helper path");
    let conf = fs::read_to_string(format!("{}.conf", exe.display())).unwrap_or_default();
    let get = |key: &str| {
        conf.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::to_owned)
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let joined = args.join(" ");
    if let Some(log) = get("log") {
        append(&log, &format!("{joined}\n"));
    }
    if let Some(env) = get("env") {
        let cwd = std::env::current_dir().map(|dir| dir.display().to_string());
        let mut text = format!(
            "exe={}\ncwd={}\nargs={joined}\n",
            exe.display(),
            cwd.unwrap_or_default()
        );
        if let Some(tmp) = std::env::var_os("TMPDIR")
            && let Ok(meta) = fs::symlink_metadata(&tmp)
        {
            text += &format!("tmpdir={:o} {}\n", meta.mode() & 0o7777, meta.uid());
        }
        for (key, value) in std::env::vars_os() {
            text += &format!("{}={}\n", key.to_string_lossy(), value.to_string_lossy());
        }
        append(&env, &(text + "--\n"));
    }
    if let Some(marker) = get("marker") {
        fs::write(marker, "").expect("marker");
    }
    if let Some(version) = get("version")
        && args == ["version"]
    {
        println!("{version}");
        return;
    }
    if let Some(refuse) = get("refuse")
        && joined.contains(&refuse)
    {
        exit(3);
    }
    match get("real") {
        Some(real) => {
            let error = Command::new(real).args(&args).exec();
            eprintln!("git helper: {error}");
            exit(1);
        }
        None => exit(1),
    }
}
