//! The host abstraction for claude and grok: host-id keying, the loop cap, merged-file
//! hardening, the install work-tree probe and the upgrade from S0-written files.

mod common;

use common::{
    changed_repo, claude_input, git, go, grok_input, is_block, native_git, real_git, rotter,
    set_mode, stdout, temp,
};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_rotter");

fn hook(root: &Path, name: &str, input: &str) -> Output {
    rotter(root, &["hook", name], root, input, &[])
}

fn integration(root: &Path, args: &[&str], env: &[(&str, Option<&OsStr>)]) -> (i32, String) {
    let mut all = vec!["integration"];
    all.extend(args);
    let output = rotter(root, &all, root, "", env);
    let text = stdout(&output) + &String::from_utf8_lossy(&output.stderr);
    (output.status.code().unwrap(), text)
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

type Input = fn(&str, &Path) -> String;

const HOSTS: [(&str, &str, Input); 2] = [
    ("claude", "claude-stop", claude_input),
    ("grok", "grok-stop", grok_input),
];

#[test]
fn hook_aliases_share_one_counter_and_one_dedupe_slot() {
    for (alias, canonical, input) in HOSTS {
        let root = temp("alias");
        let repo = root.join("repo");
        changed_repo(&repo);
        // A changing report every Stop, alternating the two names of one host.
        let mut blocks = Vec::new();
        for (turn, name) in [alias, canonical, alias, canonical].iter().enumerate() {
            fs::write(repo.join("a.go"), go(10 + turn)).unwrap();
            blocks.push(is_block(&hook(&root, name, &input("s", &repo))));
        }
        assert_eq!(blocks, [true, true, false, true], "{alias}");
        // The last report through the other name: one slot, so quiet.
        assert!(
            !is_block(&hook(&root, alias, &input("s", &repo))),
            "{alias}"
        );
        // One slot and one counter, under the canonical name only.
        assert_eq!(
            listing(&root.join("state")),
            [canonical.to_owned(), format!("{canonical}-count")],
            "{alias}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn the_loop_cap_fails_closed() {
    for (_, canonical, input) in HOSTS {
        let root = temp("cap");
        let repo = root.join("repo");
        changed_repo(&repo);
        let stop = input("s", &repo);
        // The continuation flag is absent from the input and the state dir cannot be written.
        let mut unflagged: Value = serde_json::from_str(&stop).unwrap();
        unflagged
            .as_object_mut()
            .unwrap()
            .retain(|key, _| !key.contains("top"));
        let unflagged = unflagged.to_string();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        set_mode(&locked, 0o500);
        let env = [("ROTTER_STATE_DIR", Some(locked.as_os_str()))];
        let output = rotter(&root, &["hook", canonical], &root, &unflagged, &env);
        assert!(!is_block(&output), "{canonical}: {output:?}");
        set_mode(&locked, 0o700);
        // No state location at all.
        let unset = [
            ("ROTTER_STATE_DIR", None),
            ("XDG_STATE_HOME", None),
            ("HOME", None),
        ];
        let output = rotter(&root, &["hook", canonical], &root, &stop, &unset);
        assert!(!is_block(&output), "{canonical}: {output:?}");
        // Session ids that cannot key a counter: silent without looking at anything.
        for session in ["..", ".", "", &"s".repeat(300)] {
            let output = hook(&root, canonical, &input(session, &repo));
            assert_eq!(stdout(&output), "", "{canonical} {session:?}");
        }
        let mut missing: Value = serde_json::from_str(&stop).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .retain(|key, _| !key.contains("ession"));
        assert_eq!(stdout(&hook(&root, canonical, &missing.to_string())), "");
        // Positive control: the same Stop with a usable state dir asks.
        assert!(is_block(&hook(&root, canonical, &unflagged)), "{canonical}");
        fs::remove_dir_all(root).unwrap();
    }
}

/// Two hook runs of `canonical` in one session started together on different repositories.
fn concurrent(root: &Path, canonical: &str, input: Input, repos: [&Path; 2]) -> usize {
    let children: Vec<_> = repos
        .iter()
        .map(|repo| {
            let mut child = Command::new(EXE)
                .args(["hook", canonical])
                .current_dir(root)
                .env("HOME", root.join("home"))
                .env("XDG_CONFIG_HOME", root.join("xdg-config"))
                .env("CLAUDE_CONFIG_DIR", root.join("claude"))
                .env("GROK_HOME", root.join("grok"))
                .env("ROTTER_STATE_DIR", root.join("state"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input("s", repo).as_bytes())
                .unwrap();
            child
        })
        .collect();
    children
        .into_iter()
        .map(|child| usize::from(is_block(&child.wait_with_output().unwrap())))
        .sum()
}

#[test]
fn concurrent_stops_respect_the_remaining_capacity() {
    for (_, canonical, input) in HOSTS {
        let root = temp("concurrent");
        let (one, two) = (root.join("one"), root.join("two"));
        changed_repo(&one);
        changed_repo(&two);
        fs::write(two.join("a.go"), go(3)).unwrap();
        let count = root.join(format!("state/{canonical}-count/s"));
        fs::create_dir_all(count.parent().unwrap()).unwrap();
        // One request left: at most one of two passes.
        fs::write(&count, "1\n").unwrap();
        assert_eq!(concurrent(&root, canonical, input, [&one, &two]), 1);
        // From 0 both pass, and the count is not lost.
        fs::write(one.join("a.go"), go(4)).unwrap();
        fs::write(two.join("a.go"), go(5)).unwrap();
        fs::write(&count, "").unwrap();
        assert_eq!(concurrent(&root, canonical, input, [&one, &two]), 2);
        assert_eq!(fs::read_to_string(&count).unwrap(), "2\n");
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn every_hook_name_exits_zero() {
    let root = temp("names");
    for args in [
        &["hook", "bogus"][..],
        &["hook"],
        &["hook", "claude", "x"],
        &["hook", "grok", "x"],
        &["hook", "Claude"],
    ] {
        let output = rotter(&root, args, &root, "{}", &[]);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unknown hook"),
            "{output:?}"
        );
    }
    {
        use std::os::unix::ffi::OsStrExt;
        let bad = OsStr::from_bytes(b"\xff");
        for args in [
            vec![OsStr::new("hook"), bad],
            vec!["hook".as_ref(), "claude".as_ref(), bad],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_rotter"))
                .args(&args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
            assert!(output.stdout.is_empty(), "{args:?}");
        }
    }
    for name in ["claude", "grok"] {
        let output = rotter(&root, &["hook", name], &root, "not json", &[]);
        assert_eq!(
            (output.status.code(), stdout(&output)),
            (Some(0), String::new())
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn claude_falls_back_to_home_when_its_variable_is_unset_or_relative() {
    let root = temp("fallback");
    let dot = root.join("home/.claude");
    fs::create_dir_all(&dot).unwrap();
    for value in [None, Some(OsStr::new("rel-claude"))] {
        let env = [("CLAUDE_CONFIG_DIR", value)];
        let (code, text) = integration(&root, &["install", "claude"], &env);
        assert_eq!(code, 0, "{value:?}: {text}");
        assert!(
            text.contains(&dot.join("settings.json").display().to_string()),
            "{text}"
        );
        assert!(
            commands(&dot.join("settings.json"))
                .iter()
                .any(|command| command.contains(EXE))
        );
        assert!(!root.join("rel-claude").exists());
        let (code, text) = integration(&root, &["uninstall", "claude"], &env);
        assert_eq!((code, text.contains("removed")), (0, true), "{text}");
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn claude_takes_its_directories_from_home() {
    // A non-injectable host: HOME (no XDG_CONFIG_HOME) locates the config, as documented.
    let root = temp("home");
    let repo = root.join("repo");
    changed_repo(&repo);
    let config = root.join("home/.config/rotter");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "parse_timeout_seconds = 0\n").unwrap();
    let env = [("XDG_CONFIG_HOME", None)];
    let output = rotter(
        &root,
        &["hook", "claude"],
        &root,
        &claude_input("h", &repo),
        &env,
    );
    assert!(stdout(&output).contains("config error"), "{output:?}");
    fs::remove_dir_all(root).unwrap();
}

/// Copies `tests/fixtures/s0/<name>` to `to`, with the S0 binary path set to this binary.
fn golden(name: &str, to: &Path) {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/s0")
        .join(name);
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    let text = fs::read_to_string(from).unwrap().replace("@ROTTER@", EXE);
    fs::write(to, text).unwrap();
    set_mode(to, 0o600);
}

#[test]
fn s0_installations_and_state_carry_over() {
    let root = temp("upgrade");
    let settings = root.join("claude/settings.json");
    let rotter_json = root.join("grok/hooks/rotter.json");
    golden("claude/settings.json", &settings);
    golden("grok/hooks/rotter.json", &rotter_json);
    set_mode(&root.join("grok/hooks"), 0o700);
    let before = (
        fs::read(&settings).unwrap(),
        fs::read(&rotter_json).unwrap(),
    );
    let (code, text) = integration(&root, &["status"], &[]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("claude: installed (current)"), "{text}");
    assert!(text.contains("grok: installed (current)"), "{text}");
    for host in ["claude", "grok"] {
        let (code, text) = integration(&root, &["install", host], &[]);
        assert_eq!(
            (code, text.contains("already installed")),
            (0, true),
            "{text}"
        );
    }
    let after = (
        fs::read(&settings).unwrap(),
        fs::read(&rotter_json).unwrap(),
    );
    assert!(after == before, "install rewrote an S0 file");
    for host in ["claude", "grok"] {
        let (code, text) = integration(&root, &["uninstall", host], &[]);
        assert_eq!((code, text.contains("removed")), (0, true), "{text}");
    }
    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        "{\n  \"hooks\": {\n    \"Stop\": []\n  }\n}\n"
    );
    assert_eq!(listing(&root.join("grok/hooks")), Vec::<String>::new());

    // An S0 merge next to another tool's entry, with its backup.
    let merged = root.join("merged");
    golden("claude-merged/settings.json", &merged.join("settings.json"));
    golden(
        "claude-merged/settings.json.rotter-bak",
        &merged.join("settings.json.rotter-bak"),
    );
    let env = [("CLAUDE_CONFIG_DIR", Some(merged.as_os_str()))];
    let before = fs::read(merged.join("settings.json")).unwrap();
    assert!(
        integration(&root, &["status"], &env)
            .1
            .contains("claude: installed (current)")
    );
    assert!(
        integration(&root, &["install", "claude"], &env)
            .1
            .contains("already installed")
    );
    assert_eq!(fs::read(merged.join("settings.json")).unwrap(), before);
    let (code, text) = integration(&root, &["uninstall", "claude"], &env);
    assert_eq!(code, 0, "{text}");
    let value: Value =
        serde_json::from_str(&fs::read_to_string(merged.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        value,
        json!({ "model": "x", "hooks": { "Stop": [{ "hooks": [
        { "type": "command", "command": "other" }
    ] }] } })
    );
    assert!(!merged.join("settings.json.rotter-bak").exists());

    // S0 state: a diagnostics store written by S0 is still honoured (the same note is not
    // repeated in its session), and the new request counter starts at 0.
    let state = root.join("state");
    golden(
        "state/claude-stop-notes/s0",
        &state.join("claude-stop-notes/s0"),
    );
    let clean = root.join("clean");
    fs::create_dir(&clean).unwrap();
    git(&clean, &["init", "-q", "-b", "main"]);
    let no_config = [("HOME", None), ("XDG_CONFIG_HOME", None)];
    let run = |session: &str, cwd: &Path| {
        rotter(
            &root,
            &["hook", "claude-stop"],
            &root,
            &claude_input(session, cwd),
            &no_config,
        )
    };
    assert_eq!(stdout(&run("s0", &clean)), "", "S0's announced note");
    assert!(
        stdout(&run("s1", &clean)).contains("no config location"),
        "positive control"
    );
    let repo = root.join("repo");
    changed_repo(&repo);
    let asks: Vec<bool> = (0..3)
        .map(|turn| {
            fs::write(repo.join("a.go"), go(20 + turn)).unwrap();
            is_block(&run("s0", &repo))
        })
        .collect();
    assert_eq!(asks, [true, true, false], "the counter started at 0");
    assert!(
        state.join("claude-stop/s0").is_file(),
        "the S0 slot name is kept"
    );
    fs::remove_dir_all(root).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn commands(path: &Path) -> Vec<String> {
    read_json(path)["hooks"]["Stop"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap().clone())
        .map(|entry| entry["command"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn merged_settings_keep_every_other_tools_entry() {
    let root = temp("merged");
    let dir = root.join("claude");
    fs::create_dir(&dir).unwrap();
    let settings = dir.join("settings.json");
    let ours = format!("'{EXE}' hook claude-stop || true");
    let foreign = [
        "'/x/rotter-proxy' hook codex",
        "'/opt/rotter-dev' hook claude-stop || true",
    ];
    let seed = json!({ "hooks": { "Stop": [
        { "hooks": [] },
        { "matcher": "x", "hooks": [] },
        { "hooks": [{ "type": "command", "command": foreign[0] }] },
        { "hooks": [{ "type": "command", "command": foreign[1] }] },
        { "hooks": [{ "type": "command", "command": "'/else/rotter' hook claude-stop || true" }] },
    ] } });
    fs::write(&settings, serde_json::to_string_pretty(&seed).unwrap()).unwrap();
    let (code, text) = integration(&root, &["install", "claude"], &[]);
    assert_eq!((code, text.contains("updated")), (0, true), "{text}");
    // Another rotter binary's entry is replaced by one entry of this one; the rest stays.
    assert_eq!(commands(&settings), [foreign[0], foreign[1], ours.as_str()]);
    let groups = read_json(&settings)["hooks"]["Stop"].clone();
    assert_eq!(groups[0], json!({ "hooks": [] }));
    assert_eq!(groups[1], json!({ "matcher": "x", "hooks": [] }));
    let (code, text) = integration(&root, &["uninstall", "claude"], &[]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(commands(&settings), foreign);
    let groups = read_json(&settings)["hooks"]["Stop"].clone();
    assert_eq!(
        groups.as_array().unwrap().len(),
        4,
        "only the group rotter emptied goes"
    );

    // Unsafe modes and duplicate keys: refused and untouched.
    for (mode, text, why) in [
        (0o664, "{}".to_owned(), "chmod go-w"),
        (0o646, "{}".to_owned(), "chmod go-w"),
        (
            0o600,
            r#"{"model": "a", "model": "b"}"#.to_owned(),
            "duplicate key",
        ),
    ] {
        fs::write(&settings, &text).unwrap();
        set_mode(&settings, mode);
        let (code, output) = integration(&root, &["install", "claude"], &[]);
        assert_eq!(code, 2, "{output}");
        assert!(output.contains(why), "{output}");
        assert_eq!(fs::read_to_string(&settings).unwrap(), text);
        assert_eq!(listing(&dir), ["settings.json"]);
    }
    // A missing host directory is never created.
    let gone = root.join("gone");
    let env = [("CLAUDE_CONFIG_DIR", Some(gone.as_os_str()))];
    let (code, text) = integration(&root, &["install", "claude"], &env);
    assert_eq!((code, text.contains("does not exist")), (2, true), "{text}");
    assert!(!gone.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn install_refuses_host_dirs_inside_a_work_tree() {
    let root = temp("probe");
    let work = root.join("dotfiles");
    fs::create_dir_all(work.join("claude")).unwrap();
    fs::create_dir_all(work.join("grok")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    // Variables in rotter's own environment that would hide the work tree from git.
    let ceiling = work.clone().into_os_string();
    let hiding = [
        (
            "CLAUDE_CONFIG_DIR",
            Some(work.join("claude").into_os_string()),
        ),
        ("GROK_HOME", Some(work.join("grok").into_os_string())),
        ("GIT_CEILING_DIRECTORIES", Some(ceiling)),
        ("GIT_DIR", Some("/nonexistent/rotter-git-dir".into())),
    ];
    let env: Vec<(&str, Option<&OsStr>)> = hiding
        .iter()
        .map(|(key, value)| (*key, value.as_deref()))
        .collect();
    for host in ["claude", "grok"] {
        let (code, text) = integration(&root, &["install", host], &env);
        assert_eq!(code, 2, "{host}: {text}");
        assert!(text.contains("inside the git work tree"), "{host}: {text}");
        assert!(text.contains(if host == "claude" {
            "CLAUDE_CONFIG_DIR"
        } else {
            "GROK_HOME"
        }));
    }
    assert!(listing(&work.join("claude")).is_empty() && listing(&work.join("grok")).is_empty());
    // Positive control: with those variables plain git does not see the work tree.
    let plain = Command::new(real_git())
        .args([
            "-C",
            work.join("claude").to_str().unwrap(),
            "rev-parse",
            "--show-toplevel",
        ])
        .env("GIT_CEILING_DIRECTORIES", &work)
        .env("GIT_DIR", "/nonexistent/rotter-git-dir")
        .output()
        .unwrap();
    assert!(!plain.status.success(), "{plain:?}");

    // No usable git on PATH: refused too (fails closed).
    let empty = root.join("empty");
    fs::create_dir(&empty).unwrap();
    let mut no_git = env.clone();
    no_git.push(("PATH", Some(empty.as_os_str())));
    for host in ["claude", "grok"] {
        let (code, text) = integration(&root, &["install", host], &no_git);
        assert_eq!(code, 2, "{host}: {text}");
        assert!(text.contains("cannot tell"), "{host}: {text}");
    }

    // No `.git` above the host dir: installed without any git call.
    let outside = root.join("outside");
    fs::create_dir_all(outside.join("grok")).unwrap();
    let wrapper = root.join("wrapper");
    let log = root.join("git.log");
    native_git(
        &wrapper,
        &[
            ("log", &log.display().to_string()),
            ("real", &real_git().display().to_string()),
        ],
    );
    let path = std::env::join_paths(
        std::iter::once(wrapper.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let clean = [
        ("CLAUDE_CONFIG_DIR", Some(outside.clone().into_os_string())),
        ("GROK_HOME", Some(outside.join("grok").into_os_string())),
        ("PATH", Some(path.clone())),
    ];
    let clean: Vec<(&str, Option<&OsStr>)> = clean
        .iter()
        .map(|(key, value)| (*key, value.as_deref()))
        .collect();
    for host in ["claude", "grok"] {
        let (code, text) = integration(&root, &["install", host], &clean);
        assert_eq!(code, 0, "{host}: {text}");
    }
    assert!(
        !log.exists(),
        "git ran: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    // Positive control: the wrapper is the git rotter uses once there is a `.git` above.
    let mut inside = hiding.to_vec();
    inside.push(("PATH", Some(path)));
    let inside: Vec<(&str, Option<&OsStr>)> = inside
        .iter()
        .map(|(key, value)| (*key, value.as_deref()))
        .collect();
    assert_eq!(integration(&root, &["install", "claude"], &inside).0, 2);
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .contains("rev-parse --show-toplevel")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn owned_file_status_names_other_binaries() {
    let root = temp("owned");
    let hooks = root.join("grok/hooks");
    fs::create_dir_all(&hooks).unwrap();
    set_mode(&hooks, 0o700);
    let document = json!({ "hooks": { "Stop": [{ "hooks": [
        { "type": "command", "command": "'/else/rotter' hook grok-stop || true", "timeout": 90 }
    ] }] } });
    let file: PathBuf = hooks.join("rotter.json");
    fs::write(&file, document.to_string()).unwrap();
    set_mode(&file, 0o600);
    let (code, text) = integration(&root, &["status"], &[]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("grok: installed for another binary"),
        "{text}"
    );
    let git = fs::canonicalize(real_git()).unwrap();
    assert!(
        text.contains(&format!("git: {} (git version ", git.display())),
        "{text}"
    );
    // A dispatcher alone on PATH (outside the status run's directory, whose tree is excluded):
    // status names it as skipped and still exits 0.
    let shims = temp("owned-shims");
    fs::write(shims.join("git"), "#!/bin/sh\nexit 0\n").unwrap();
    set_mode(&shims.join("git"), 0o755);
    let env = [("PATH", Some(shims.as_os_str()))];
    let (code, text) = integration(&root, &["status"], &env);
    assert_eq!(code, 0, "{text}");
    let line = text.lines().find(|line| line.starts_with("git: ")).unwrap();
    assert!(
        line.contains("skipped") && line.contains("not a native executable"),
        "{line}"
    );
    fs::remove_dir_all(shims).unwrap();
    let (code, text) = integration(&root, &["install", "grok"], &[]);
    assert_eq!((code, text.contains("updated")), (0, true), "{text}");
    fs::remove_dir_all(root).unwrap();
}
