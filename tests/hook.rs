use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
    command
        .args(["hook", "claude-stop"])
        .current_dir(cwd)
        .env("ROTTER_STATE_DIR", state)
        .env("XDG_CONFIG_HOME", state.join("xdg-config"))
        .env("XDG_CACHE_HOME", state.join("xdg-cache"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"));
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
    command.arg("integration").args(args);
    finish(config, command)
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

/// Runs an integration command with Claude settings and XDG dirs pinned under `config`; a
/// command that does not finish promptly fails the test instead of hanging it.
fn finish(config: &Path, mut command: Command) -> (i32, String) {
    let mut child = command
        .env("CLAUDE_CONFIG_DIR", config)
        .env("XDG_CONFIG_HOME", config.join("xdg-config"))
        .env("XDG_CACHE_HOME", config.join("xdg-cache"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            panic!("rotter integration hangs");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
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
            .filter(|command| command.ends_with("hook claude-stop"))
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
    format!("'{}' hook claude-stop", env!("CARGO_BIN_EXE_rotter"))
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
        state.join("claude-stop/t").is_file(),
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
