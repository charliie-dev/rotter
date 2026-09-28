mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

fn temp(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rotter-hook-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) {
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
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn hook(input: &str, state: &Path) -> String {
    hook_with(input, state, &[])
}

/// Runs the hook with config and Claude settings pinned under `state` unless `env` sets them.
fn hook_with(input: &str, state: &Path, env: &[(&str, &str)]) -> String {
    hook_in(input, state, state, env)
}

/// [`hook_with`] with the hook process running in `cwd`.
fn hook_in(input: &str, state: &Path, cwd: &Path, env: &[(&str, &str)]) -> String {
    let output = run_hook("claude-stop", input, state, cwd, env);
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// `rotter hook grok-stop` with everything pinned under `state`: (stdout, stderr).
fn grok(input: &str, state: &Path, env: &[(&str, &str)]) -> (String, String) {
    let output = run_hook("grok-stop", input, state, state, env);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// `rotter hook <name>` in `cwd` with HOME, XDG dirs, Claude settings, Grok's home and the state
/// directory pinned under `state` unless `env` sets them.
fn run_hook(name: &str, input: &str, state: &Path, cwd: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
    command
        .args(["hook", name])
        .current_dir(cwd)
        .env("HOME", state.join("home"))
        .env("ROTTER_STATE_DIR", state)
        .env("XDG_CONFIG_HOME", state.join("xdg-config"))
        .env("XDG_CACHE_HOME", state.join("xdg-cache"))
        .env("XDG_STATE_HOME", state.join("xdg-state"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("GROK_HOME", state.join("grok"))
        .env("CODEX_HOME", state.join("codex"))
        .env("COPILOT_HOME", state.join("copilot"));
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    wait(child, "rotter hook")
}

/// Waits for `child`; one that does not finish promptly fails the test instead of hanging it.
fn wait(mut child: Child, what: &str) -> Output {
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            panic!("{what} hangs");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn stop_hook_blocks_once_per_new_report() {
    let repo = temp("repo");
    let state = temp("state");
    fs::write(
        repo.join("a.go"),
        "package p\n\n// F returns one.\nfunc F() int { return 1 }\n",
    )
    .unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    let input = |active: &str| {
        format!(
            r#"{{"session_id": "s/1", "cwd": "{}", "hook_event_name": "Stop", {active}}}"#,
            repo.display()
        )
    };
    let stop = input(r#""stop_hook_active": false"#);

    assert_eq!(hook(&stop, &state), "", "no changes");
    fs::write(
        repo.join("a.go"),
        "package p\n\n// F returns one.\nfunc F() int { return 2 }\n",
    )
    .unwrap();
    let first = hook(&stop, &state);
    assert!(first.contains(r#""decision":"block""#), "{first}");
    assert!(first.contains("--skill"), "{first}");
    assert_eq!(hook(&stop, &state), "", "same report is not reviewed twice");
    assert_eq!(hook(&input(r#""stop_hook_active": true"#), &state), "");
    assert_eq!(hook(&input(r#""stopHookActive": true"#), &state), "");

    fs::write(
        repo.join("b.lua"),
        "-- G.\nlocal function g() return 1 end\n",
    )
    .unwrap();
    assert!(
        hook(&stop, &state).contains(r#""decision":"block""#),
        "untracked file"
    );

    let outside = temp("outside");
    let elsewhere = format!(r#"{{"session_id": "s", "cwd": "{}"}}"#, outside.display());
    assert_eq!(hook(&elsewhere, &state), "", "not a git repository");
    fs::remove_dir_all(&outside).unwrap();

    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}

fn integration(config: &Path, args: &[&str]) -> (i32, String) {
    integration_env(config, args, &[])
}

/// [`integration`] with `env` applied after the pinned directories (None removes a variable).
fn integration_env(config: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> (i32, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
    command.arg("integration").args(args);
    finish_env(config, command, env)
}

/// `rotter integration <args>` under `umask`.
fn integration_umask(config: &Path, umask: &str, args: &[&str]) -> (i32, String) {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!("umask {umask} && exec \"$0\" integration \"$@\""))
        .arg(env!("CARGO_BIN_EXE_rotter"))
        .args(args);
    finish(config, command)
}

fn finish(config: &Path, command: Command) -> (i32, String) {
    finish_env(config, command, &[])
}

/// Runs an integration command in `config` with Claude settings in `config`, HOME, XDG dirs and
/// Grok's home pinned under it, then `env`; a command that does not finish promptly fails the
/// test instead of hanging it.
fn finish_env(config: &Path, mut command: Command, env: &[(&str, Option<&str>)]) -> (i32, String) {
    command
        .current_dir(config)
        .env("HOME", config.join("home"))
        .env("CLAUDE_CONFIG_DIR", config)
        .env("GROK_HOME", config.join("grok"))
        .env("CODEX_HOME", config.join("codex"))
        .env("COPILOT_HOME", config.join("copilot"))
        .env("XDG_CONFIG_HOME", config.join("xdg-config"))
        .env("XDG_CACHE_HOME", config.join("xdg-cache"))
        .env("XDG_STATE_HOME", config.join("xdg-state"));
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait(child, "rotter integration");
    let text =
        String::from_utf8(output.stdout).unwrap() + &String::from_utf8(output.stderr).unwrap();
    (output.status.code().unwrap(), text)
}

#[test]
fn integration_install_keeps_other_settings_and_is_idempotent() {
    let config = temp("claude");
    let settings = config.join("settings.json");
    let original = r#"{"model": "x", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other"}]}], "SessionStart": []}}"#;
    fs::write(&settings, original).unwrap();
    let read = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap()
    };
    let ours = |value: &serde_json::Value| -> Vec<String> {
        value["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap().iter())
            .filter_map(|entry| entry["command"].as_str())
            .filter(|command| command.ends_with("hook claude-stop || true"))
            .map(str::to_owned)
            .collect()
    };

    assert!(
        integration(&config, &["status"])
            .1
            .contains("not installed")
    );
    let (code, text) = integration(&config, &["install", "claude"]);
    assert_eq!(code, 0, "{text}");
    let installed = read();
    assert_eq!(ours(&installed).len(), 1);
    assert!(ours(&installed)[0].contains(env!("CARGO_BIN_EXE_rotter")));
    assert_eq!(installed["model"], "x");
    assert_eq!(
        installed["hooks"]["Stop"][0]["hooks"][0]["command"],
        "other"
    );
    assert_eq!(
        fs::read_to_string(config.join("settings.json.rotter-bak")).unwrap(),
        original
    );
    assert!(
        integration(&config, &["status"])
            .1
            .contains("installed (current)")
    );

    assert!(
        integration(&config, &["install", "claude"])
            .1
            .contains("already installed")
    );
    assert_eq!(ours(&read()).len(), 1);

    let (code, text) = integration(&config, &["uninstall", "claude"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("settings.json.rotter-bak"), "{text}");
    assert!(
        !config.join("settings.json.rotter-bak").exists(),
        "backup removed"
    );
    let removed = read();
    assert!(ours(&removed).is_empty());
    assert_eq!(removed["hooks"]["Stop"][0]["hooks"][0]["command"], "other");
    assert_eq!(removed["hooks"]["SessionStart"], serde_json::json!([]));

    assert_eq!(integration(&config, &["install", "codex"]).0, 2);
    fs::write(&settings, "[1]").unwrap();
    assert_eq!(integration(&config, &["install", "claude"]).0, 2);
    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        "[1]",
        "invalid settings are not rewritten"
    );
    fs::remove_dir_all(&config).unwrap();
}

#[test]
fn integration_install_creates_missing_settings() {
    let config = temp("fresh");
    assert_eq!(integration(&config, &["uninstall", "claude"]).0, 0);
    assert!(
        !config.join("settings.json").exists(),
        "uninstall without settings writes nothing"
    );
    assert_eq!(integration(&config, &["install", "claude"]).0, 0);
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(config.join("settings.json")).unwrap()).unwrap();
    assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["timeout"], 90);
    fs::remove_dir_all(&config).unwrap();
}

fn command() -> String {
    format!(
        "'{}' hook claude-stop || true",
        env!("CARGO_BIN_EXE_rotter")
    )
}

fn seed_settings(dir: &Path, timeout: serde_json::Value) {
    fs::create_dir_all(dir).unwrap();
    let settings = serde_json::json!({ "hooks": { "Stop": [{ "hooks": [
        { "type": "command", "command": command(), "timeout": timeout }
    ] }] } });
    fs::write(dir.join("settings.json"), settings.to_string()).unwrap();
}

fn write_config(xdg: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(xdg.join("rotter")).unwrap();
    let path = xdg.join("rotter/config.toml");
    fs::write(&path, text).unwrap();
    path
}

fn changed_repo(name: &str) -> PathBuf {
    let repo = temp(name);
    fs::write(
        repo.join("a.go"),
        "package p\n\n// F returns one.\nfunc F() int { return 1 }\n",
    )
    .unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    fs::write(
        repo.join("a.go"),
        "package p\n\n// F returns one.\nfunc F() int { return 2 }\n",
    )
    .unwrap();
    repo
}

#[test]
fn short_installed_timeout_reports_timeouts_once() {
    let repo = changed_repo("timeout-repo");
    let state = temp("timeout-state");
    // min(90, 10) - 15 saturates to zero: the run deadline is the hook's start.
    seed_settings(&state.join("claude"), serde_json::json!(10));
    let stop = format!(r#"{{"session_id": "t", "cwd": "{}"}}"#, repo.display());
    let first = hook(&stop, &state);
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert!(value.get("decision").is_none(), "{first}");
    assert!(
        value["systemMessage"]
            .as_str()
            .unwrap()
            .contains("could not be analysed"),
        "{first}"
    );
    assert!(
        state.join("claude-stop-errors/t").is_file(),
        "dedupe state written"
    );
    assert_eq!(
        hook(&stop, &state),
        "",
        "the same incomplete report is announced once"
    );
    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn invalid_config_warns_once_and_still_reviews_builtins() {
    let repo = changed_repo("badconfig-repo");
    let state = temp("badconfig-state");
    write_config(&state.join("xdg-config"), "parse_timeout_seconds = 0\n");
    let stop = format!(r#"{{"session_id": "b", "cwd": "{}"}}"#, repo.display());
    let output = hook(&stop, &state);
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["decision"], "block", "{output}");
    let message = value["systemMessage"].as_str().unwrap();
    assert!(message.starts_with("rotter: config error: "), "{message}");
    assert!(
        message.ends_with("; external languages disabled"),
        "{message}"
    );
    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn relative_state_dir_is_ignored() {
    let repo = changed_repo("relstate-repo");
    let state = temp("relstate-state");
    let xdg_state = temp("relstate-xdg");
    let stop = format!(r#"{{"session_id": "r", "cwd": "{}"}}"#, repo.display());
    let xdg = xdg_state.display().to_string();
    let env = [
        ("ROTTER_STATE_DIR", "rel-state"),
        ("XDG_STATE_HOME", xdg.as_str()),
    ];
    assert!(hook_with(&stop, &state, &env).contains(r#""decision":"block""#));
    assert!(xdg_state.join("rotter/claude-stop/r").is_file());
    assert!(!state.join("rel-state").exists() && !repo.join("rel-state").exists());
    assert_eq!(
        hook_with(&stop, &state, &env),
        "",
        "deduped through XDG_STATE_HOME"
    );
    for dir in [repo, state, xdg_state] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn install_sizes_the_timeout_from_the_config() {
    let config = temp("sizing");
    seed_settings(&config, serde_json::json!(60));
    let xdg = config.join("xdg-config");
    let settings = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(config.join("settings.json")).unwrap()).unwrap()
    };
    let ours = |value: &serde_json::Value| -> Vec<serde_json::Value> {
        value["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap().clone())
            .collect()
    };
    let expect = |timeout: u64| {
        let entries = ours(&settings());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["command"].as_str().unwrap(), command());
        assert_eq!(entries[0]["timeout"], timeout);
    };

    let (_, text) = integration(&config, &["status"]);
    assert!(
        text.contains("installed (timeout 60, expected 90)"),
        "{text}"
    );
    assert_eq!(integration(&config, &["install", "claude"]).0, 0);
    expect(90);
    assert!(
        integration(&config, &["status"])
            .1
            .contains("installed (current)")
    );

    let path = write_config(&xdg, "parse_timeout_seconds = 300\n");
    assert!(
        integration(&config, &["status"])
            .1
            .contains("installed (timeout 90, expected 330)")
    );
    assert_eq!(integration(&config, &["install", "claude"]).0, 0);
    expect(330);
    write_config(&xdg, "parse_timeout_seconds = 120\n");
    assert_eq!(integration(&config, &["install", "claude"]).0, 0);
    expect(150);

    // A refused config is reported and the default applies.
    write_config(&xdg, "parse_timeout_seconds = 3600\n");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    let (_, text) = integration(&config, &["status"]);
    assert!(
        text.contains("expected 90") && text.contains("refused"),
        "{text}"
    );
    let (code, text) = integration(&config, &["install", "claude"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("refused"), "{text}");
    expect(90);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    write_config(&xdg, "parse_timeout_seconds = 0\n");
    assert_eq!(integration(&config, &["install", "claude"]).0, 2);
    assert_eq!(integration(&config, &["status"]).0, 2);
    expect(90);
    fs::remove_dir_all(&config).unwrap();
}

#[test]
fn install_without_home_or_absolute_claude_dir_writes_nothing() {
    let work = temp("nohome");
    let output = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(["integration", "install", "claude"])
        .current_dir(&work)
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env("CLAUDE_CONFIG_DIR", "rel-claude")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(fs::read_dir(&work).unwrap().count(), 0, "nothing written");
    fs::remove_dir_all(&work).unwrap();
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn install_keeps_private_modes_under_umask_022() {
    use std::os::unix::fs::PermissionsExt;
    let config = temp("modes");
    seed_settings(&config, serde_json::json!(60));
    let settings = config.join("settings.json");
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    let (code, text) = integration_umask(&config, "022", &["install", "claude"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(mode(&settings), 0o600);
    let backup = config.join("settings.json.rotter-bak");
    assert!(fs::symlink_metadata(&backup).unwrap().is_file());
    assert_eq!(mode(&backup), 0o600);
    // A world-readable original keeps its bits (only the permission bits are copied).
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&settings, "{}").unwrap();
    assert_eq!(
        integration_umask(&config, "077", &["install", "claude"]).0,
        0
    );
    assert_eq!(mode(&settings), 0o644);
    assert_eq!(mode(&backup), 0o600);
    assert!(!config.join("settings.json.rotter-tmp").exists());
    fs::remove_dir_all(&config).unwrap();
}

#[test]
fn symlinked_backup_or_temporary_is_never_followed() {
    use std::os::unix::fs::symlink;
    let config = temp("links");
    let canary = config.join("canary");
    fs::write(&canary, "canary").unwrap();
    let settings = config.join("settings.json");
    let backup = config.join("settings.json.rotter-bak");
    let temporary = config.join("settings.json.rotter-tmp");
    fs::write(&settings, r#"{"model": "x"}"#).unwrap();

    // install stops on a symlinked backup without touching it, its referent or settings.json.
    symlink(&canary, &backup).unwrap();
    let (code, text) = integration(&config, &["install", "claude"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("rotter-bak"), "{text}");
    assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");
    assert_eq!(fs::read_to_string(&settings).unwrap(), r#"{"model": "x"}"#);
    assert!(
        fs::symlink_metadata(&backup)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&backup).unwrap();

    // A pre-planted temporary symlink is refused, never written through.
    symlink(&canary, &temporary).unwrap();
    let (code, text) = integration(&config, &["install", "claude"]);
    assert_eq!(code, 2, "{text}");
    assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");
    assert_eq!(fs::read_to_string(&settings).unwrap(), r#"{"model": "x"}"#);
    fs::remove_file(&temporary).unwrap();
    // A stale regular temporary is replaced.
    fs::write(&temporary, "stale").unwrap();
    assert_eq!(integration(&config, &["install", "claude"]).0, 0);
    assert!(!temporary.exists());

    // Uninstall with a symlinked backup: the entry goes, the link and its referent stay.
    fs::remove_file(&backup).unwrap();
    symlink(&canary, &backup).unwrap();
    let (code, text) = integration(&config, &["uninstall", "claude"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("left") && text.contains("rotter-bak"),
        "{text}"
    );
    assert!(
        !fs::read_to_string(&settings)
            .unwrap()
            .contains("claude-stop")
    );
    assert!(
        fs::read_to_string(&settings)
            .unwrap()
            .contains(r#""model": "x""#)
    );
    assert!(
        fs::symlink_metadata(&backup)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");
    fs::remove_dir_all(&config).unwrap();
}

#[test]
fn uninstall_backup_cleanup_follows_a_successful_read() {
    let config = temp("cleanup");
    let settings = config.join("settings.json");
    let backup = config.join("settings.json.rotter-bak");
    assert_eq!(integration(&config, &["install", "claude"]).0, 0);
    fs::write(&backup, "old").unwrap();
    // Unparseable settings: exit 2 and the backup stays.
    fs::write(&settings, "{").unwrap();
    assert_eq!(integration(&config, &["uninstall", "claude"]).0, 2);
    assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
    // Nothing installed: the leftover backup is still removed and named.
    fs::write(&settings, "{}").unwrap();
    let (code, text) = integration(&config, &["uninstall", "claude"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("not installed"), "{text}");
    assert!(
        text.contains(&format!("removed {}", backup.display())),
        "{text}"
    );
    assert!(!backup.exists());
    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        "{}",
        "not rewritten"
    );
    fs::remove_dir_all(&config).unwrap();
}

#[test]
fn fifo_settings_fail_promptly() {
    let config = temp("fifo");
    let settings = config.join("settings.json");
    assert!(
        Command::new("mkfifo")
            .arg(&settings)
            .status()
            .unwrap()
            .success()
    );
    for args in [
        &["install", "claude"][..],
        &["uninstall", "claude"],
        &["status"],
    ] {
        let (code, text) = integration(&config, args);
        assert_eq!(code, 2, "{args:?}: {text}");
        assert!(text.contains("not a regular file"), "{args:?}: {text}");
    }
    // The hook falls back to the default timeout instead of blocking.
    let repo = changed_repo("fifo-repo");
    let state = temp("fifo-state");
    fs::create_dir_all(state.join("claude")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(state.join("claude/settings.json"))
            .status()
            .unwrap()
            .success()
    );
    let stop = format!(r#"{{"session_id": "f", "cwd": "{}"}}"#, repo.display());
    assert!(hook(&stop, &state).contains(r#""decision":"block""#));
    for dir in [config, repo, state] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn status_prints_a_non_integer_timeout_readably() {
    let config = temp("strtimeout");
    seed_settings(&config, serde_json::json!("60"));
    let (_, text) = integration(&config, &["status"]);
    assert!(
        text.contains(r#"installed (timeout "60" (not a number), expected 90)"#),
        "{text}"
    );
    fs::remove_dir_all(&config).unwrap();
}

#[test]
fn hook_commands_quote_the_repository_path() {
    let repo = changed_repo("foo;bar");
    let state = temp("quote-state");
    let stop = serde_json::json!({ "session_id": "q", "cwd": repo }).to_string();
    let output = hook(&stop, &state);
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    let reason = value["reason"].as_str().unwrap();
    let exe = format!("'{}'", env!("CARGO_BIN_EXE_rotter"));
    assert!(
        reason.contains(&format!(
            "`{exe} extract --worktree --include-untracked -C '{}'`",
            repo.display()
        )),
        "{reason}"
    );
    assert!(reason.contains(&format!("`{exe} --skill`")), "{reason}");
    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn relative_path_git_is_never_run() {
    use std::os::unix::fs::PermissionsExt;
    let repo = changed_repo("relgit-repo");
    let state = temp("relgit-state");
    let marker = state.join("fake-git-ran");
    let fake = repo.join("git");
    fs::write(
        &fake,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(".:{}", std::env::var("PATH").unwrap());
    let stop = format!(r#"{{"session_id": "g", "cwd": "{}"}}"#, repo.display());
    let output = hook_in(&stop, &state, &repo, &[("PATH", path.as_str())]);
    assert!(output.contains(r#""decision":"block""#), "{output}");
    assert!(!marker.exists(), "the repository's git was executed");
    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn config_notes_are_announced_once_per_session() {
    let repo = changed_repo("notes-repo");
    let clean = temp("notes-clean");
    git(&clean, &["init", "-q", "-b", "main"]);
    let state = temp("notes-state");
    write_config(&state.join("xdg-config"), "parse_timeout_seconds = 0\n");
    let input = |cwd: &Path, session: &str| {
        format!(
            r#"{{"session_id": "{session}", "cwd": "{}"}}"#,
            cwd.display()
        )
    };
    // Nothing to review: the note alone, once.
    let first = hook(&input(&clean, "n"), &state);
    assert!(first.contains("config error"), "{first}");
    assert_eq!(hook(&input(&clean, "n"), &state), "", "note repeated");
    // A review request in the same session no longer carries the note.
    let review = hook(&input(&repo, "n"), &state);
    assert!(review.contains(r#""decision":"block""#), "{review}");
    assert!(!review.contains("systemMessage"), "{review}");
    // A new session hears it again; a different note is announced too.
    assert!(hook(&input(&clean, "m"), &state).contains("config error"));
    write_config(&state.join("xdg-config"), "parse_timeout_seconds = 4000\n");
    assert!(hook(&input(&clean, "n"), &state).contains("4000"));
    for dir in [repo, clean, state] {
        fs::remove_dir_all(dir).unwrap();
    }
}

fn grok_command() -> String {
    format!("'{}' hook grok-stop || true", env!("CARGO_BIN_EXE_rotter"))
}

/// A rotter-generated Grok document for `command` and `timeout`.
fn grok_document(command: &str, timeout: u64) -> serde_json::Value {
    serde_json::json!({ "hooks": { "Stop": [{ "hooks": [
        { "type": "command", "command": command, "timeout": timeout }
    ] }] } })
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A canonical temporary directory, so printed paths compare exactly.
fn canonical_temp(name: &str) -> PathBuf {
    fs::canonicalize(temp(name)).unwrap()
}

#[test]
fn grok_install_writes_one_private_file_and_uninstall_removes_it() {
    let root = canonical_temp("grok-install");
    let hooks = root.join("grok/hooks");
    let file = hooks.join("rotter.json");
    fs::create_dir(root.join("grok")).unwrap();
    let (code, text) = integration_umask(&root, "000", &["install", "grok"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("grok: installed"), "{text}");
    assert_eq!(mode(&hooks), 0o700, "missing hooks/ is created private");
    assert_eq!(mode(&file), 0o600, "0600 even under umask 000");
    assert_eq!(read_json(&file), grok_document(&grok_command(), 90));
    let (code, text) = integration(&root, &["install", "grok"]);
    assert_eq!(
        (code, text.contains("already installed")),
        (0, true),
        "{text}"
    );

    // Replacing a 0644 file, sized from the config: exactly 0600 again, nothing copied.
    set_mode(&file, 0o644);
    let xdg = root.join("xdg-config");
    write_config(&xdg, "parse_timeout_seconds = 300\n");
    let (code, text) = integration_umask(&root, "000", &["install", "grok"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("updated"), "{text}");
    assert_eq!(read_json(&file), grok_document(&grok_command(), 330));
    assert_eq!(mode(&file), 0o600);

    // A stale regular temporary is replaced; a symlinked one is refused and never followed.
    let temporary = hooks.join("rotter.json.rotter-tmp");
    fs::write(&temporary, "stale").unwrap();
    write_config(&xdg, "");
    assert_eq!(integration(&root, &["install", "grok"]).0, 0);
    assert_eq!(
        read_json(&file)["hooks"]["Stop"][0]["hooks"][0]["timeout"],
        90
    );
    let canary = root.join("canary");
    fs::write(&canary, "canary").unwrap();
    std::os::unix::fs::symlink(&canary, &temporary).unwrap();
    write_config(&xdg, "parse_timeout_seconds = 300\n");
    assert_eq!(integration(&root, &["install", "grok"]).0, 2);
    assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");
    assert_eq!(
        read_json(&file)["hooks"]["Stop"][0]["hooks"][0]["timeout"],
        90
    );
    fs::remove_file(&temporary).unwrap();
    assert_eq!(
        listing(&hooks),
        ["rotter.json"],
        "only rotter.json ends in .json"
    );

    let (code, text) = integration(&root, &["uninstall", "grok"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("removed"), "{text}");
    assert!(listing(&hooks).is_empty(), "{:?}", listing(&hooks));
    let (code, text) = integration(&root, &["uninstall", "grok"]);
    assert_eq!((code, text.contains("not installed")), (0, true), "{text}");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn grok_target_must_exist_and_be_private() {
    let root = canonical_temp("grok-target");
    let grok = root.join("grok");
    let (code, text) = integration(&root, &["install", "grok"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("does not exist"), "{text}");
    assert!(!grok.exists(), "a missing Grok home is never created");

    fs::create_dir(&grok).unwrap();
    set_mode(&grok, 0o775);
    let (code, text) = integration(&root, &["install", "grok"]);
    assert_eq!(code, 2, "{text}");
    assert!(!grok.join("hooks").exists(), "{text}");
    set_mode(&grok, 0o700);
    fs::create_dir(grok.join("hooks")).unwrap();
    set_mode(&grok.join("hooks"), 0o770);
    let (code, text) = integration(&root, &["install", "grok"]);
    assert_eq!(code, 2, "{text}");
    assert!(listing(&grok.join("hooks")).is_empty());
    fs::remove_dir(grok.join("hooks")).unwrap();
    std::os::unix::fs::symlink(&root, grok.join("hooks")).unwrap();
    let (code, text) = integration(&root, &["install", "grok"]);
    assert_eq!(code, 2, "symlinked hooks/: {text}");
    assert!(!root.join("rotter.json").exists());
    fs::remove_file(grok.join("hooks")).unwrap();

    // A relative GROK_HOME is ignored: $HOME/.grok is used.
    let dot = root.join("home/.grok");
    fs::create_dir_all(&dot).unwrap();
    let relative = [("GROK_HOME", Some("rel-grok"))];
    let (code, text) = integration_env(&root, &["install", "grok"], &relative);
    assert_eq!(code, 0, "{text}");
    assert!(dot.join("hooks/rotter.json").is_file());
    assert!(!root.join("rel-grok").exists() && !grok.join("hooks").exists());
    // Neither an absolute GROK_HOME nor HOME.
    let neither = [("GROK_HOME", Some("rel-grok")), ("HOME", None)];
    for args in [&["install", "grok"][..], &["uninstall", "grok"]] {
        let (code, text) = integration_env(&root, args, &neither);
        assert_eq!(code, 2, "{args:?}: {text}");
        assert!(text.contains("GROK_HOME"), "{text}");
    }
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn foreign_or_unsafe_rotter_json_is_left_alone() {
    let root = canonical_temp("grok-foreign");
    let hooks = root.join("grok/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let file = hooks.join("rotter.json");
    let ours = grok_document(&grok_command(), 90);
    let mut env = ours.clone();
    env["hooks"]["Stop"][0]["hooks"][0]["env"] = serde_json::json!({ "X": "1" });
    let mut matcher = ours.clone();
    matcher["hooks"]["Stop"][0]["matcher"] = "".into();
    let mut event = ours.clone();
    event["hooks"]["SessionStart"] = ours["hooks"]["Stop"].clone();
    let foreign = serde_json::json!({ "hooks": { "Stop": [{ "hooks": [
        { "type": "command", "command": "other", "timeout": 90 }
    ] }] } });
    let canary = root.join("canary.json");
    for (name, content, file_mode) in [
        ("foreign", foreign.to_string(), 0o600),
        ("env", env.to_string(), 0o600),
        ("matcher", matcher.to_string(), 0o600),
        ("second event", event.to_string(), 0o600),
        ("not json", "{".to_owned(), 0o600),
        ("0666", ours.to_string(), 0o666),
        ("symlink", String::new(), 0o600),
    ] {
        if name == "symlink" {
            fs::write(&canary, ours.to_string()).unwrap();
            std::os::unix::fs::symlink(&canary, &file).unwrap();
        } else {
            fs::write(&file, &content).unwrap();
            set_mode(&file, file_mode);
        }
        for args in [&["install", "grok"][..], &["uninstall", "grok"]] {
            let (code, text) = integration(&root, args);
            assert_eq!(code, 2, "{name} {args:?}: {text}");
        }
        let (code, text) = integration(&root, &["status"]);
        assert_eq!(code, 0, "{name}: {text}");
        if name == "symlink" {
            assert!(fs::symlink_metadata(&file).unwrap().is_symlink());
            assert_eq!(fs::read_to_string(&canary).unwrap(), ours.to_string());
        } else {
            assert_eq!(fs::read_to_string(&file).unwrap(), content, "{name}");
            assert_eq!(mode(&file), file_mode, "{name}");
        }
        fs::remove_file(&file).unwrap();
        assert_eq!(listing(&hooks), Vec::<String>::new(), "{name}");
    }
    fs::remove_dir_all(&root).unwrap();
}

/// The `status` line starting with `prefix`.
fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {text}"))
}

#[test]
fn status_reports_each_host_with_its_path() {
    let root = canonical_temp("status");
    fs::create_dir(root.join("grok")).unwrap();
    let settings = root.join("settings.json");
    let file = root.join("grok/hooks/rotter.json");
    let status = || {
        let (code, text) = integration(&root, &["status"]);
        assert_eq!(code, 0, "{text}");
        text
    };
    let text = status();
    assert_eq!(
        line(&text, "claude:"),
        format!("claude: not installed ({})", settings.display())
    );
    assert_eq!(
        line(&text, "grok:"),
        format!("grok: not installed ({})", file.display())
    );
    assert!(!text.contains("compatibility"), "{text}");
    assert_eq!(
        line(&text, "executable:"),
        format!(
            "executable: {} (safe to register)",
            env!("CARGO_BIN_EXE_rotter")
        )
    );

    assert_eq!(integration(&root, &["install", "claude"]).0, 0);
    assert_eq!(integration(&root, &["install", "grok"]).0, 0);
    let text = status();
    assert_eq!(
        line(&text, "claude:"),
        format!("claude: installed (current) ({})", settings.display())
    );
    assert_eq!(
        line(&text, "grok:"),
        format!("grok: installed (current) ({})", file.display())
    );

    // Edited timeouts.
    let edit = |path: &Path, key: &str, value: serde_json::Value| {
        let mut json = read_json(path);
        json["hooks"]["Stop"][0]["hooks"][0][key] = value;
        fs::write(path, json.to_string()).unwrap();
    };
    edit(&settings, "timeout", 5.into());
    edit(&file, "timeout", 5.into());
    let text = status();
    assert_eq!(
        line(&text, "claude:"),
        format!(
            "claude: installed (timeout 5, expected 90); run `rotter integration install claude` ({})",
            settings.display()
        )
    );
    assert_eq!(
        line(&text, "grok:"),
        format!(
            "grok: installed (timeout 5, expected 90); run `rotter integration install grok` ({})",
            file.display()
        )
    );
    // Edited commands.
    edit(
        &settings,
        "command",
        "'/else/rotter' hook claude-stop || true".into(),
    );
    edit(
        &file,
        "command",
        "'/else/rotter' hook grok-stop || true".into(),
    );
    let text = status();
    assert!(
        line(&text, "claude:").starts_with("claude: installed for another binary: '/else/rotter'"),
        "{text}"
    );
    assert!(
        line(&text, "grok:").starts_with("grok: installed for another binary: '/else/rotter'"),
        "{text}"
    );
    assert!(line(&text, "grok:").ends_with(&format!("({})", file.display())));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn status_compat_check_is_read_only_and_tolerant() {
    let root = canonical_temp("compat");
    let dot_claude = root.join("home/.claude");
    fs::create_dir_all(&dot_claude).unwrap();
    let literal = dot_claude.join("settings.json");
    // (i) $HOME/.claude/settings.json is the Claude settings file and holds the rotter entry.
    let same = dot_claude.display().to_string();
    let same = [("CLAUDE_CONFIG_DIR", Some(same.as_str()))];
    assert_eq!(integration_env(&root, &["install", "claude"], &same).0, 0);
    let (code, text) = integration_env(&root, &["status"], &same);
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        line(&text, "grok: found"),
        format!(
            "grok: found a Claude entry that Grok's Claude compatibility can pick up (whether \
             compat is enabled was not checked) ({})",
            literal.display()
        )
    );
    let before = fs::read(&literal).unwrap();
    // (ii) The primary settings are a different, readable file; only the compat path is
    // unreadable or a FIFO: "unknown", exit 0, no blocking.
    fs::write(root.join("settings.json"), "{}").unwrap();
    set_mode(&literal, 0o000);
    let (code, text) = integration(&root, &["status"]);
    assert_eq!(code, 0, "{text}");
    assert!(line(&text, "grok: Claude compatibility entry unknown").contains("cannot read"));
    assert!(line(&text, "claude:").contains("not installed"), "{text}");
    set_mode(&literal, 0o600);
    assert_eq!(fs::read(&literal).unwrap(), before, "never written");
    fs::remove_file(&literal).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&literal)
            .status()
            .unwrap()
            .success()
    );
    let (code, text) = integration(&root, &["status"]);
    assert_eq!(code, 0, "{text}");
    assert!(line(&text, "grok: Claude compatibility entry unknown").contains("not a regular file"));
    // (iii) A FIFO primary keeps the exit-2 contract (see fifo_settings_fail_promptly).
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn untrusted_executable_is_never_registered() {
    let root = canonical_temp("exe");
    for (dir, dir_mode) in [(root.join("bin$HOME"), 0o700), (root.join("shared"), 0o770)] {
        let work = root.join("work");
        fs::create_dir_all(work.join("grok")).unwrap();
        fs::create_dir(&dir).unwrap();
        let exe = dir.join("rotter");
        fs::copy(env!("CARGO_BIN_EXE_rotter"), &exe).unwrap();
        set_mode(&dir, dir_mode);
        let run = |args: &[&str]| {
            let mut command = Command::new(&exe);
            command.arg("integration").args(args);
            finish(&work, command)
        };
        for name in ["claude", "grok"] {
            let (code, text) = run(&["install", name]);
            assert_eq!(code, 2, "{} {name}: {text}", dir.display());
            assert!(text.contains("refusing to register"), "{text}");
        }
        assert!(!work.join("settings.json").exists());
        assert!(listing(&work.join("grok")).is_empty(), "nothing written");
        let (code, text) = run(&["status"]);
        assert_eq!(code, 0, "{text}");
        assert!(
            line(&text, "executable:").contains("refusing to register"),
            "{text}"
        );
        set_mode(&dir, 0o700);
        fs::remove_dir_all(&work).unwrap();
    }
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn unknown_hooks_and_failing_binaries_never_block() {
    let root = canonical_temp("unknown");
    for args in [
        &["hook", "bogus"][..],
        &["hook"],
        &["hook", "claude-stop", "x"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rotter"))
            .args(args)
            .current_dir(&root)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unknown hook"),
            "{output:?}"
        );
    }
    // The installed command strings with a binary that exits 2 still exit 0, printing nothing.
    fs::create_dir(root.join("grok")).unwrap();
    assert_eq!(integration(&root, &["install", "grok"]).0, 0);
    assert_eq!(integration(&root, &["install", "claude"]).0, 0);
    let stub = root.join("stub");
    fs::write(&stub, "#!/bin/sh\nexit 2\n").unwrap();
    set_mode(&stub, 0o755);
    let installed = [
        read_json(&root.join("grok/hooks/rotter.json")),
        read_json(&root.join("settings.json")),
    ];
    for document in installed {
        let command = document["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .replace(env!("CARGO_BIN_EXE_rotter"), &stub.display().to_string());
        assert!(command.ends_with(" || true"), "{command}");
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{command}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn install_claude_migrates_the_old_command_form() {
    let config = temp("migrate");
    let old = format!("'{}' hook claude-stop", env!("CARGO_BIN_EXE_rotter"));
    let seed = || {
        let settings = serde_json::json!({ "hooks": { "Stop": [
            { "hooks": [{ "type": "command", "command": "other" }] },
            { "hooks": [{ "type": "command", "command": old, "timeout": 90 }] }
        ] } });
        fs::write(config.join("settings.json"), settings.to_string()).unwrap();
    };
    let commands = || -> Vec<String> {
        read_json(&config.join("settings.json"))["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap().clone())
            .map(|entry| entry["command"].as_str().unwrap().to_owned())
            .collect()
    };
    seed();
    assert!(
        line(&integration(&config, &["status"]).1, "claude:").contains("older command"),
        "status names the old form"
    );
    let (code, text) = integration(&config, &["install", "claude"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("updated"), "{text}");
    assert_eq!(commands(), ["other".to_owned(), command()]);
    assert!(
        integration(&config, &["status"])
            .1
            .contains("installed (current)")
    );
    seed();
    assert_eq!(integration(&config, &["uninstall", "claude"]).0, 0);
    assert_eq!(commands(), ["other"]);
    fs::remove_dir_all(&config).unwrap();
}

/// Grok Build's Stop input for `cwd` in `session`, with `extra` fields replacing defaults.
fn grok_input(session: &str, cwd: &Path, extra: serde_json::Value) -> String {
    let mut input = serde_json::json!({
        "hookEventName": "stop",
        "hook_event_name": "Stop",
        "sessionId": session,
        "cwd": cwd,
        "workspaceRoot": cwd,
        "permissionMode": "default",
        "promptId": "p1",
        "stopHookActive": false,
        "lastAssistantMessage": "SECRET-last-message",
        "reason": "end_turn",
    });
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() {
            input.as_object_mut().unwrap().remove(key);
        } else {
            input[key] = value.clone();
        }
    }
    input.to_string()
}

fn is_block(stdout: &str) -> bool {
    stdout.contains(r#""decision":"block""#)
}

#[test]
fn a_block_that_cannot_be_written_is_not_recorded() {
    let repo = changed_repo("grok-epipe");
    let state = temp("grok-epipe-state");
    let input = grok_input("e", &repo, serde_json::json!({}));
    // Nobody reads the block: the write fails with EPIPE, so the slot must stay empty.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(["hook", "grok-stop"])
        .current_dir(&state)
        .env("HOME", state.join("home"))
        .env("ROTTER_STATE_DIR", &state)
        .env("XDG_CONFIG_HOME", state.join("xdg-config"))
        .env("XDG_CACHE_HOME", state.join("xdg-cache"))
        .env("XDG_STATE_HOME", state.join("xdg-state"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("GROK_HOME", state.join("grok"))
        .env("CODEX_HOME", state.join("codex"))
        .env("COPILOT_HOME", state.join("copilot"))
        .stdin(Stdio::piped())
        .stdout(writer)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(!state.join("grok-stop/e").exists());
    let (again, _) = grok(&input, &state, &[]);
    assert!(again.contains("\"block\""), "{again}");
}

#[test]
fn grok_stop_blocks_once_per_new_report() {
    use serde_json::json;
    let repo = changed_repo("grok-repo");
    let state = temp("grok-state");
    let input = |session: &str, extra| grok_input(session, &repo, extra);
    let (first, stderr) = grok(&input("g", json!({})), &state, &[]);
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["decision"], "block", "{first}");
    assert_eq!(
        value.as_object().unwrap().len(),
        2,
        "only decision and reason: {first}"
    );
    assert!(
        value["reason"].as_str().unwrap().contains("--skill"),
        "{first}"
    );
    assert!(
        !(first.clone() + &stderr).contains("SECRET"),
        "never echoed"
    );
    assert_eq!(stderr, "");
    assert!(state.join("grok-stop/g").is_file());
    assert_eq!(
        grok(&input("g", json!({})), &state, &[]).0,
        "",
        "same report"
    );
    assert_eq!(
        grok(&input("h", json!({ "stopHookActive": true })), &state, &[]).0,
        ""
    );

    // The session-end Stop never runs git; the same wrapper does run on a turn end.
    let wrapper = state.join("wrapper");
    let log = state.join("git.log");
    let real = common::real_git();
    common::native_git(
        &wrapper,
        &[
            ("log", &log.display().to_string()),
            ("real", &real.display().to_string()),
        ],
    );
    let path = format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str())];
    for reason in ["shutdown", "channel_closed"] {
        let (stdout, stderr) = grok(&input("end", json!({ "reason": reason })), &state, &env);
        assert_eq!(stdout + &stderr, "", "{reason}");
        assert!(!log.exists(), "{reason}: git ran");
    }
    assert!(is_block(&grok(&input("end", json!({})), &state, &env).0));
    assert!(log.exists(), "positive control: the wrapper is used");

    // cwd must be absolute and free of control characters; workspaceRoot is the fallback.
    for cwd in [
        json!("relative/dir"),
        json!(format!("{}\n", repo.display())),
    ] {
        let extra = json!({ "cwd": cwd, "workspaceRoot": null });
        assert_eq!(
            grok(&input("c", extra), &state, &[]),
            (String::new(), String::new())
        );
    }
    let root_only = json!({ "cwd": null });
    assert!(is_block(&grok(&input("w", root_only), &state, &[]).0));

    // Notes go to stderr, never into the decision, and are not marked as announced.
    let clean = temp("grok-clean");
    git(&clean, &["init", "-q", "-b", "main"]);
    write_config(&state.join("xdg-config"), "parse_timeout_seconds = 0\n");
    for _ in 0..2 {
        let (stdout, stderr) = grok(&grok_input("n", &clean, json!({})), &state, &[]);
        assert_eq!(stdout, "");
        assert!(stderr.starts_with("rotter: config error"), "{stderr}");
    }
    let (stdout, stderr) = grok(&input("n", json!({})), &state, &[]);
    assert!(
        is_block(&stdout) && !stdout.contains("systemMessage"),
        "{stdout}"
    );
    assert!(stderr.contains("config error"), "{stderr}");
    for dir in [repo, state, clean] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn native_and_compat_registrations_ask_once() {
    use serde_json::json;
    let repo = changed_repo("double-repo");
    let state = temp("double-state");
    let claude = |input: &str| hook(input, &state);
    // Native then compat, and compat then native, with the same Grok input.
    let input = grok_input("d1", &repo, json!({}));
    assert!(is_block(&grok(&input, &state, &[]).0));
    assert_eq!(claude(&input), "");
    let input = grok_input("d2", &repo, json!({}));
    assert!(is_block(&claude(&input)));
    assert_eq!(grok(&input, &state, &[]).0, "");

    // The external review's sequence: the compat hook's short timeout gives a zero-unit
    // timed-out report, which must not make the native report ask again. Three user turns.
    seed_settings(&state.join("claude"), json!(10));
    let turn = grok_input("r", &repo, json!({}));
    let continuation = grok_input("r", &repo, json!({ "stopHookActive": true }));
    let mut blocks = 0;
    for round in 0..3 {
        let native = grok(&turn, &state, &[]).0;
        let compat = claude(&turn);
        blocks += usize::from(is_block(&native)) + usize::from(is_block(&compat));
        assert!(!is_block(&compat), "{compat}");
        if round == 0 {
            assert!(compat.contains("could not be analysed"), "{compat}");
        } else {
            assert_eq!((native.as_str(), compat.as_str()), ("", ""));
        }
        assert_eq!(grok(&continuation, &state, &[]).0, "");
        assert_eq!(claude(&continuation), "");
    }
    assert_eq!(blocks, 1);
    // A genuinely different report asks again.
    fs::write(
        repo.join("b.lua"),
        "-- G.\nlocal function g() return 1 end\n",
    )
    .unwrap();
    assert!(is_block(&grok(&turn, &state, &[]).0));
    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn grok_run_budget_comes_from_rotter_json() {
    let repo = changed_repo("grok-budget-repo");
    let state = temp("grok-budget-state");
    let hooks = state.join("grok/hooks");
    fs::create_dir_all(&hooks).unwrap();
    // min(90, 10) - 15 saturates to zero: the run deadline is the hook's start.
    let document = grok_document(&grok_command(), 10);
    fs::write(hooks.join("rotter.json"), document.to_string()).unwrap();
    let input = grok_input("b", &repo, serde_json::json!({}));
    let (stdout, stderr) = grok(&input, &state, &[]);
    assert_eq!(stdout, "");
    assert!(stderr.contains("could not be analysed"), "{stderr}");
    // A FIFO rotter.json does not block the hook: Grok's 600 applies.
    fs::remove_file(hooks.join("rotter.json")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(hooks.join("rotter.json"))
            .status()
            .unwrap()
            .success()
    );
    assert!(is_block(&grok(&input, &state, &[]).0));
    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}
