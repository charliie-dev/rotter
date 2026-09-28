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
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(["hook", "claude-stop"])
        .env("ROTTER_STATE_DIR", state)
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
    let output = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .arg("integration")
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config)
        .output()
        .unwrap();
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

    assert_eq!(integration(&config, &["uninstall", "claude"]).0, 0);
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
    assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["timeout"], 60);
    fs::remove_dir_all(&config).unwrap();
}
