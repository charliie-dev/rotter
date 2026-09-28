//! Support shared by the integration tests: the native git helper (a shell wrapper would be
//! skipped by rotter's git selection) and the git rotter itself would use.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const SOURCE: &str = include_str!("git_helper.rs");

/// The helper binary, compiled with `rustc` once and cached under `CARGO_TARGET_TMPDIR` by a hash
/// of its source.
pub fn helper() -> &'static Path {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    HELPER.get_or_init(|| {
        let hash = SOURCE
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let out = dir.join(format!("git-helper-{hash:016x}"));
        if !out.is_file() {
            let unique = format!("git-helper-{hash:016x}-{}", std::process::id());
            let source = dir.join(format!("{unique}.rs"));
            let built = dir.join(&unique);
            fs::write(&source, SOURCE).unwrap();
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let output = Command::new(rustc)
                .args(["--edition", "2024", "-o"])
                .arg(&built)
                .arg(&source)
                .output()
                .expect("rustc for the native git helper");
            assert!(
                output.status.success(),
                "cannot compile the native git helper: {output:?}"
            );
            fs::rename(&built, &out).unwrap();
            let _ = fs::remove_file(source);
        }
        out
    })
}

/// Installs the helper as `<dir>/<name>` (0755) with `settings` as its `key value` lines.
pub fn native_as(dir: &Path, name: &str, settings: &[(&str, &str)]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::copy(helper(), &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let conf: String = settings
        .iter()
        .map(|(key, value)| format!("{key} {value}\n"))
        .collect();
    fs::write(dir.join(format!("{name}.conf")), conf).unwrap();
    path
}

/// [`native_as`] named `git`.
pub fn native_git(dir: &Path, settings: &[(&str, &str)]) -> PathBuf {
    native_as(dir, "git", settings)
}

/// The git a plain `git` resolves to from this process's absolute PATH entries.
pub fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH")
}

/// Every call the `env` setting recorded: `(key, value)` lines per call.
pub fn calls(log: &Path) -> Vec<Vec<(String, String)>> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .split("--\n")
        .filter(|call| !call.is_empty())
        .map(|call| {
            call.lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        })
        .collect()
}

/// A fresh directory under the temp dir, as the temp dir spells it (on macOS `/tmp/...` or
/// `/var/folders/...`, both behind a symlink).
pub fn temp(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rotter-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// Plain git for fixtures, isolated from the user's configuration.
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// `a.go` with a documented function returning `value`.
pub fn go(value: usize) -> String {
    format!("package p\n\n// F returns one.\nfunc F() int {{ return {value} }}\n")
}

/// A repository at `repo` whose committed `a.go` is changed in the work tree.
pub fn changed_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    fs::write(repo.join("a.go"), go(1)).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "c"]);
    fs::write(repo.join("a.go"), go(2)).unwrap();
}

/// `rotter <args>` in `cwd` with HOME, XDG_*, CLAUDE_CONFIG_DIR, GROK_HOME, CODEX_HOME,
/// COPILOT_HOME and ROTTER_STATE_DIR pinned under `root` (Droid has no variable: `~/.factory`), then `env` (None removes a variable); `input` on stdin. A run that does
/// not finish promptly fails the test instead of hanging it.
pub fn rotter(
    root: &Path,
    args: &[&str],
    cwd: &Path,
    input: &str,
    env: &[(&str, Option<&std::ffi::OsStr>)],
) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
    command
        .args(args)
        .current_dir(cwd)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_CACHE_HOME", root.join("xdg-cache"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("GROK_HOME", root.join("grok"))
        .env("CODEX_HOME", root.join("codex"))
        .env("COPILOT_HOME", root.join("copilot"))
        .env("ROTTER_STATE_DIR", root.join("state"));
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            panic!("rotter {args:?} hangs");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// Claude Code's Stop input.
pub fn claude_input(session: &str, cwd: &Path) -> String {
    serde_json::json!({ "session_id": session, "cwd": cwd, "hook_event_name": "Stop",
        "stop_hook_active": false })
    .to_string()
}

/// Grok Build's Stop input.
pub fn grok_input(session: &str, cwd: &Path) -> String {
    serde_json::json!({ "hookEventName": "stop", "hook_event_name": "Stop", "sessionId": session,
        "cwd": cwd, "workspaceRoot": cwd, "stopHookActive": false, "reason": "end_turn" })
    .to_string()
}

/// Codex's Stop input (codex-rs/hooks/src/events/stop.rs).
pub fn codex_input(session: &str, cwd: &Path) -> String {
    serde_json::json!({ "session_id": session, "turn_id": "t1", "transcript_path": null,
        "cwd": cwd, "hook_event_name": "Stop", "model": "m", "permission_mode": "default",
        "stop_hook_active": false, "last_assistant_message": "done" })
    .to_string()
}

/// GitHub Copilot CLI's agentStop input (hooks reference, camelCase event).
pub fn copilot_input(session: &str, cwd: &Path) -> String {
    serde_json::json!({ "sessionId": session, "timestamp": 1_700_000_000_000_u64, "cwd": cwd,
        "transcriptPath": "/nonexistent/transcript.jsonl", "stopReason": "end_turn",
        "stop_hook_active": false })
    .to_string()
}

/// Factory Droid's Stop input (hooks reference: common fields plus `stop_hook_active`).
pub fn droid_input(session: &str, cwd: &Path) -> String {
    serde_json::json!({ "session_id": session, "transcript_path": "/nonexistent/t.jsonl",
        "cwd": cwd, "permission_mode": "default", "hook_event_name": "Stop",
        "stop_hook_active": false })
    .to_string()
}

pub fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn is_block(output: &std::process::Output) -> bool {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    stdout(output).contains(r#""decision":"block""#)
}

pub fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
