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

fn hook(input: &str, state: &Path, rotter: &str) -> String {
    let mut child = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/hooks/claude-stop.sh"))
        .env("ROTTER_BIN", rotter)
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
    let rotter = env!("CARGO_BIN_EXE_rotter");
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

    assert_eq!(hook(&stop, &state, rotter), "", "no changes");
    fs::write(
        repo.join("a.go"),
        "package p\n\n// F returns one.\nfunc F() int { return 2 }\n",
    )
    .unwrap();
    let first = hook(&stop, &state, rotter);
    assert!(first.contains(r#""decision": "block""#), "{first}");
    assert!(first.contains("rotter-comment-review"), "{first}");
    assert_eq!(
        hook(&stop, &state, rotter),
        "",
        "same report is not reviewed twice"
    );
    assert_eq!(
        hook(&input(r#""stop_hook_active": true"#), &state, rotter),
        ""
    );
    assert_eq!(
        hook(&input(r#""stopHookActive": true"#), &state, rotter),
        ""
    );

    fs::write(
        repo.join("b.lua"),
        "-- G.\nlocal function g() return 1 end\n",
    )
    .unwrap();
    assert!(
        hook(&stop, &state, rotter).contains(r#""decision": "block""#),
        "untracked file"
    );

    let missing = hook(&stop, &state, "/nonexistent/rotter");
    assert!(missing.contains("systemMessage"), "{missing}");
    assert!(!missing.contains("decision"), "{missing}");

    fs::remove_dir_all(&repo).unwrap();
    fs::remove_dir_all(&state).unwrap();
}
