//! The host abstraction: host-id keying, the loop cap, merged-file hardening, the install
//! work-tree probe, the upgrade from S0-written files, and per-host install and hook cases.

mod common;

use common::{
    changed_repo, claude_input, codex_input, copilot_input, droid_input, git, go, grok_input,
    is_block, native_git, real_git, rotter, set_mode, stdout, temp,
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

const HOSTS: [(&str, &str, Input); 5] = [
    ("claude", "claude-stop", claude_input),
    ("grok", "grok-stop", grok_input),
    ("codex", "codex", codex_input),
    ("copilot", "copilot", copilot_input),
    ("droid", "droid", droid_input),
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
                .env("CODEX_HOME", root.join("codex"))
                .env("COPILOT_HOME", root.join("copilot"))
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

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
        &["hook", "cursor"],
        &["hook", "mastracode"],
        &["hook", "codex", "x"],
        &["hook", "pi", "--timeout"],
        &["hook", "letta", "x", "1"],
        &["hook", "opencode", "--timeout", "1", "x"],
        &["hook", "kilo"],
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
    // Input without a session returns before any directory is looked at, so the shim hosts
    // (whose HOME comes from the password database) can run here too.
    for args in [
        &["hook", "claude"][..],
        &["hook", "grok"],
        &["hook", "codex"],
        &["hook", "copilot"],
        &["hook", "droid"],
        &["hook", "pi", "--timeout", "5"],
        &["hook", "letta", "--timeout", "x"],
        &["hook", "opencode", "--timeout", "7"],
        &["hook", "claude", "--timeout", "0"],
    ] {
        let output = rotter(&root, args, &root, "not json", &[]);
        assert_eq!(
            (output.status.code(), stdout(&output), stderr_text(&output)),
            (Some(0), String::new(), String::new()),
            "{args:?}"
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

/// A host added in S2: where its hook lives and what rotter writes there.
struct New {
    id: &'static str,
    /// The directory variable, if the host has one.
    var: Option<&'static str>,
    /// Under `root/home` when the variable is unset or relative.
    fallback: &'static str,
    /// The hook file below the host directory.
    file: &'static str,
    /// The file rotter writes into an empty host directory, for command C and timeout 90.
    document: fn(&str) -> Value,
}

const NEW: [New; 3] = [
    New {
        id: "codex",
        var: Some("CODEX_HOME"),
        fallback: ".codex",
        file: "hooks.json",
        document: |command| {
            json!({ "hooks": { "Stop": [{ "hooks": [
                { "type": "command", "command": command, "timeout": 90 }
            ] }] } })
        },
    },
    New {
        id: "copilot",
        var: Some("COPILOT_HOME"),
        fallback: ".copilot",
        file: "hooks/rotter.json",
        document: |command| {
            json!({ "version": 1, "hooks": { "agentStop": [
                { "type": "command", "bash": command, "timeoutSec": 90 }
            ] } })
        },
    },
    New {
        id: "droid",
        var: None,
        fallback: ".factory",
        file: "hooks.json",
        document: |command| {
            json!({ "Stop": [{ "hooks": [
                { "type": "command", "command": command, "timeout": 90 }
            ] }] })
        },
    },
];

fn env<'a>(pairs: &'a [(&'a str, Option<PathBuf>)]) -> Vec<(&'a str, Option<&'a OsStr>)> {
    pairs
        .iter()
        .map(|(key, value)| (*key, value.as_deref().map(Path::as_os_str)))
        .collect()
}

#[test]
fn new_hosts_install_exactly_their_entry_where_their_variable_points() {
    for host in NEW {
        let root = temp("new-install");
        let command = format!("'{EXE}' hook {} || true", host.id);
        // The variable when absolute, else `~/<fallback>`: unset and relative both fall back.
        let fallback = root.join("home").join(host.fallback);
        let cases = match host.var {
            Some(var) => vec![
                (Some((var, Some(root.join("by-var")))), root.join("by-var")),
                (
                    Some((var, Some(PathBuf::from(format!("rel-{}", host.id))))),
                    fallback.clone(),
                ),
                (Some((var, None)), fallback),
            ],
            None => vec![(None, fallback)],
        };
        for (setting, dir) in cases {
            let pairs: Vec<_> = setting.into_iter().collect();
            let env = env(&pairs);
            // A missing host directory is never created.
            let (code, text) = integration(&root, &["install", host.id], &env);
            assert_eq!((code, text.contains("does not exist")), (2, true), "{text}");
            assert!(!dir.exists(), "{}", dir.display());
            fs::create_dir_all(&dir).unwrap();
            set_mode(&dir, 0o700);
            let (code, text) = integration(&root, &["install", host.id], &env);
            let file = dir.join(host.file);
            assert_eq!(code, 0, "{}: {text}", host.id);
            assert!(text.contains(&file.display().to_string()), "{text}");
            assert_eq!(read_json(&file), (host.document)(&command), "{}", host.id);
            assert!(!listing(&root).contains(&format!("rel-{}", host.id)));
            let (code, text) = integration(&root, &["install", host.id], &env);
            assert_eq!(
                (code, text.contains("already installed")),
                (0, true),
                "{text}"
            );
            let (_, text) = integration(&root, &["status"], &env);
            let line = text
                .lines()
                .find(|line| line.starts_with(&format!("{}: ", host.id)))
                .unwrap();
            assert!(line.contains("installed (current)"), "{line}");
            assert_eq!(
                line.ends_with("[experimental]"),
                host.id != "copilot",
                "{line}"
            );
            // Every installed command exits 0 even when the binary it names exits 2.
            let stub = root.join("stub");
            fs::write(&stub, "#!/bin/sh\nexit 2\n").unwrap();
            set_mode(&stub, 0o755);
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(command.replace(EXE, &stub.display().to_string()))
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(0));
            let (code, text) = integration(&root, &["uninstall", host.id], &env);
            assert_eq!((code, text.contains("removed")), (0, true), "{text}");
            if host.id == "copilot" {
                assert!(!file.exists());
                assert_eq!(mode(&dir.join("hooks")), 0o700);
            } else {
                let empty = if host.id == "codex" {
                    json!({ "hooks": { "Stop": [] } })
                } else {
                    json!({ "Stop": [] })
                };
                assert_eq!(read_json(&file), empty);
            }
            let (code, text) = integration(&root, &["uninstall", host.id], &env);
            assert_eq!((code, text.contains("not installed")), (0, true), "{text}");
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every install kind, with the host file rotter would write: (host, dir, file, owned).
fn kinds(root: &Path) -> [(&'static str, PathBuf, PathBuf, bool); 5] {
    [
        (
            "claude",
            root.join("claude"),
            root.join("claude/settings.json"),
            false,
        ),
        (
            "codex",
            root.join("codex"),
            root.join("codex/hooks.json"),
            false,
        ),
        (
            "droid",
            root.join("home/.factory"),
            root.join("home/.factory/hooks.json"),
            false,
        ),
        (
            "grok",
            root.join("grok"),
            root.join("grok/hooks/rotter.json"),
            true,
        ),
        (
            "copilot",
            root.join("copilot"),
            root.join("copilot/hooks/rotter.json"),
            true,
        ),
    ]
}

#[test]
fn every_install_kind_refuses_unsafe_or_foreign_targets_and_leaves_them() {
    let root = temp("unsafe");
    for (host, dir, file, owned) in kinds(&root) {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        set_mode(&dir, 0o700);
        set_mode(file.parent().unwrap(), 0o700);
        // A symlink at the target: never followed, never replaced.
        let elsewhere = root.join(format!("{host}-elsewhere.json"));
        fs::write(&elsewhere, "{}\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
        for action in ["install", "uninstall"] {
            let (code, text) = integration(&root, &[action, host], &[]);
            assert_eq!(code, 2, "{host} {action}: {text}");
            assert!(fs::symlink_metadata(&file).unwrap().is_symlink());
            assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "{}\n");
        }
        fs::remove_file(&file).unwrap();
        // A file others can write: refused and untouched.
        fs::write(&file, "{}\n").unwrap();
        set_mode(&file, 0o666);
        let (code, text) = integration(&root, &["install", host], &[]);
        assert_eq!(code, 2, "{host}: {text}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "{}\n");
        assert_eq!(mode(&file), 0o666);
        set_mode(&file, 0o600);
        // Another program's entry, shaped like rotter's except for the binary's name.
        let foreign = format!("'/x/rotter-proxy' hook {host} || true");
        let text = if owned {
            let key = if host == "copilot" { "bash" } else { "command" };
            let handler = json!({ "type": "command", key: foreign, "timeout": 90 });
            json!({ "hooks": { "Stop": [{ "hooks": [handler] }] } }).to_string()
        } else {
            let entry = json!({ "hooks": [{ "type": "command", "command": foreign }] });
            let event = if host == "droid" {
                json!({ "Stop": [entry] })
            } else {
                json!({ "hooks": { "Stop": [entry] } })
            };
            event.to_string()
        };
        fs::write(&file, &text).unwrap();
        set_mode(&file, 0o600);
        let (code, output) = integration(&root, &["install", host], &[]);
        if owned {
            assert_eq!(code, 2, "{host}: {output}");
            assert!(output.contains("not managed by rotter"), "{output}");
            assert_eq!(fs::read_to_string(&file).unwrap(), text);
            let (code, _) = integration(&root, &["uninstall", host], &[]);
            assert_eq!(code, 2);
            assert_eq!(fs::read_to_string(&file).unwrap(), text);
        } else {
            assert_eq!(code, 0, "{host}: {output}");
            assert!(commands_of(host, &file).contains(&foreign), "{host}");
            let (code, _) = integration(&root, &["uninstall", host], &[]);
            assert_eq!(code, 0);
            assert_eq!(commands_of(host, &file), [foreign], "{host}");
        }
    }
    fs::remove_dir_all(root).unwrap();
}

/// The commands of a merged file's Stop groups.
fn commands_of(host: &str, path: &Path) -> Vec<String> {
    let document = read_json(path);
    let groups = if host == "droid" {
        &document["Stop"]
    } else {
        &document["hooks"]["Stop"]
    };
    groups
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap().clone())
        .map(|entry| entry["command"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn codex_entries_keep_their_positions_and_status_names_the_trust_step() {
    let root = temp("codex");
    let dir = root.join("codex");
    fs::create_dir(&dir).unwrap();
    let file = dir.join("hooks.json");
    // Codex keys a hook's trust on its group and handler index.
    let seed = json!({ "hooks": { "Stop": [
        { "hooks": [{ "type": "command", "command": "/a.sh" }] },
        { "hooks": [{ "type": "command", "command": "'/else/rotter' hook codex || true",
            "timeout": 5 }] },
        { "hooks": [{ "type": "command", "command": "'/x/rotter-proxy' hook codex" }] },
    ] }, "PreToolUse": [] });
    fs::write(&file, serde_json::to_string_pretty(&seed).unwrap()).unwrap();
    let (_, text) = integration(&root, &["status"], &[]);
    assert!(
        text.contains("codex: installed for another binary"),
        "{text}"
    );
    assert!(
        text.contains("codex: hooks feature on (default); Codex runs a new or changed hook only after you trust it in /hooks"),
        "{text}"
    );
    let (code, text) = integration(&root, &["install", "codex"], &[]);
    assert_eq!((code, text.contains("updated")), (0, true), "{text}");
    let ours = format!("'{EXE}' hook codex || true");
    assert_eq!(
        commands_of("codex", &file),
        ["/a.sh", ours.as_str(), "'/x/rotter-proxy' hook codex"]
    );
    assert_eq!(read_json(&file)["PreToolUse"], json!([]));
    assert!(dir.join("hooks.json.rotter-bak").is_file());
    let (code, _) = integration(&root, &["uninstall", "codex"], &[]);
    assert_eq!(code, 0);
    assert_eq!(
        commands_of("codex", &file),
        ["/a.sh", "'/x/rotter-proxy' hook codex"]
    );
    assert!(!dir.join("hooks.json.rotter-bak").exists());
    for (config, feature) in [
        ("[features]\nhooks = false\n", "hooks feature off in"),
        ("[features]\ncodex_hooks = false\n", "hooks feature off in"),
        ("[features]\nhooks = true\n", "hooks feature on;"),
        ("not toml [", "hooks feature unknown"),
    ] {
        fs::write(dir.join("config.toml"), config).unwrap();
        let (code, text) = integration(&root, &["status"], &[]);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(&format!("codex: {feature}")),
            "{config}: {text}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn droid_never_hides_hooks_declared_in_its_settings() {
    let root = temp("droid");
    let dir = root.join("home/.factory");
    fs::create_dir_all(&dir).unwrap();
    for name in ["settings.json", "settings.local.json"] {
        let declared = json!({ "model": "x", "hooks": { "Stop": [{ "hooks": [
            { "type": "command", "command": "/their.sh" }
        ] }] } });
        fs::write(dir.join(name), declared.to_string()).unwrap();
        let (code, text) = integration(&root, &["install", "droid"], &[]);
        assert_eq!(code, 2, "{text}");
        assert!(text.contains("declares hooks"), "{text}");
        assert!(!dir.join("hooks.json").exists());
        fs::write(dir.join(name), "{\"model\": \"x\", \"hooks\": {}}").unwrap();
    }
    // Unreadable as JSON: whether it declares hooks is unknown, so nothing is created.
    fs::write(dir.join("settings.json"), "// comment\n{}").unwrap();
    let (code, text) = integration(&root, &["install", "droid"], &[]);
    assert_eq!((code, text.contains("cannot tell")), (2, true), "{text}");
    fs::write(dir.join("settings.json"), "{}").unwrap();
    let (code, text) = integration(&root, &["install", "droid"], &[]);
    assert_eq!(code, 0, "{text}");
    // Once hooks.json exists Droid no longer reads the settings' hooks.
    fs::write(
        dir.join("settings.json"),
        "{\"hooks\": {\"Stop\": [{\"hooks\": []}]}}",
    )
    .unwrap();
    let (code, text) = integration(&root, &["install", "droid"], &[]);
    assert_eq!(
        (code, text.contains("already installed")),
        (0, true),
        "{text}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn new_hosts_answer_in_their_own_protocol() {
    for (host, input) in [
        ("codex", codex_input as Input),
        ("copilot", copilot_input),
        ("droid", droid_input),
    ] {
        let root = temp("protocol");
        let repo = root.join("repo");
        changed_repo(&repo);
        let stop = input("s", &repo);
        let output = hook(&root, host, &stop);
        let reply: Value = serde_json::from_str(&stdout(&output)).unwrap();
        let keys: Vec<&str> = reply
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["decision", "reason"], "{host}");
        assert_eq!(reply["decision"], "block");
        assert!(reply["reason"].as_str().unwrap().contains("' --skill`"));
        // The same report again: silent.
        assert_eq!(stdout(&hook(&root, host, &stop)), "", "{host}");
        // A continuation the host started, or (Copilot) a stop that ends no turn: silent.
        fs::write(repo.join("a.go"), go(7)).unwrap();
        let mut continuation: Value = serde_json::from_str(&stop).unwrap();
        continuation["stop_hook_active"] = true.into();
        assert_eq!(stdout(&hook(&root, host, &continuation.to_string())), "");
        if host == "copilot" {
            let mut other: Value = serde_json::from_str(&stop).unwrap();
            other["stopReason"] = "error".into();
            assert_eq!(stdout(&hook(&root, host, &other.to_string())), "");
        }
        // Notes: a systemMessage for Codex, stderr for the others; the decision is unchanged.
        let config = root.join("xdg-config/rotter");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("config.toml"), "parse_timeout_seconds = 0\n").unwrap();
        let output = hook(&root, host, &input("n", &repo));
        let reply: Value = serde_json::from_str(&stdout(&output)).unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if host == "codex" {
            assert!(
                reply["systemMessage"]
                    .as_str()
                    .unwrap()
                    .contains("config error"),
                "{reply}"
            );
        } else {
            assert!(reply.get("systemMessage").is_none(), "{reply}");
            assert!(stderr.contains("config error"), "{host}: {stderr}");
        }
        assert_eq!(reply["decision"], "block");
        fs::remove_dir_all(root).unwrap();
    }
}

/// Codex, Copilot and Droid: a continuation the host itself started, and (Copilot only, since
/// it is the one of the three with a documented end-reason field) a stop that ends no turn, both
/// run no git at all — reusing the native recording wrapper from `tests/common`.
#[test]
fn codex_copilot_and_droid_run_no_git_on_a_continuation_or_a_non_end_turn_stop() {
    for (host, input) in [
        ("codex", codex_input as Input),
        ("copilot", copilot_input),
        ("droid", droid_input),
    ] {
        let root = temp("no-git");
        let repo = root.join("repo");
        changed_repo(&repo);
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
        let env = [("PATH", Some(path.as_os_str()))];

        // A continuation the host itself started because of a block: silent, no git.
        let mut continuation: Value = serde_json::from_str(&input("c", &repo)).unwrap();
        continuation["stop_hook_active"] = true.into();
        let output = rotter(
            &root,
            &["hook", host],
            &root,
            &continuation.to_string(),
            &env,
        );
        assert_eq!(stdout(&output), "", "{host}");
        assert!(!log.exists(), "{host}: git ran on a continuation");

        if host == "copilot" {
            let mut other: Value = serde_json::from_str(&input("o", &repo)).unwrap();
            other["stopReason"] = "error".into();
            let output = rotter(&root, &["hook", host], &root, &other.to_string(), &env);
            assert_eq!(stdout(&output), "", "{host}");
            assert!(!log.exists(), "{host}: git ran on a non-end_turn stop");
        }

        // Positive control: an ordinary Stop does use the wrapper.
        let output = rotter(&root, &["hook", host], &root, &input("p", &repo), &env);
        assert!(is_block(&output), "{host}: {output:?}");
        assert!(log.exists(), "{host}: positive control, wrapper unused");
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn unsupported_hosts_are_named_and_refused() {
    let root = temp("unsupported");
    let (_, status) = integration(&root, &["status"], &[]);
    for host in [
        "omp",
        "kilo",
        "hermes",
        "mastracode",
        "devin",
        "cursor",
        "antigravity-cli",
    ] {
        let (code, text) = integration(&root, &["install", host], &[]);
        assert_eq!(code, 2, "{text}");
        assert!(
            text.contains(&format!("{host} is not supported: ")),
            "{text}"
        );
        assert!(
            status.contains(&format!("\n{host}: unsupported: ")),
            "{status}"
        );
    }
    for host in ["omp", "kilo", "hermes"] {
        assert!(
            status.contains(&format!("{host}: unsupported: not supported yet (TODO)")),
            "{status}"
        );
    }
    assert!(listing(&root).is_empty(), "{:?}", listing(&root));
    fs::remove_dir_all(root).unwrap();
}

/// Every in-scope host (S4): the 15 named in the plan's Host table, each named exactly once in
/// `status`, whether shipped, experimental or unsupported. China-based agents (kimi, qwen,
/// qodercli) are deliberately excluded and must never appear.
#[test]
fn status_names_every_in_scope_host_exactly_once() {
    const IN_SCOPE: [&str; 15] = [
        "claude",
        "grok",
        "pi",
        "omp",
        "codex",
        "copilot",
        "devin",
        "droid",
        "opencode",
        "kilo",
        "hermes",
        "cursor",
        "mastracode",
        "antigravity-cli",
        "letta",
    ];
    let root = temp("all-hosts");
    let (code, status) = integration(&root, &["status"], &[]);
    assert_eq!(code, 0, "{status}");
    for host in IN_SCOPE {
        // Codex also prints a second, `hooks feature` line about its own trust step.
        let occurrences = status
            .lines()
            .filter(|line| {
                line.starts_with(&format!("{host}: ")) && !line.contains("hooks feature")
            })
            .count();
        assert_eq!(occurrences, 1, "{host}: {status}");
    }
    for excluded in ["kimi", "qwen", "qodercli"] {
        assert!(
            !status.contains(excluded),
            "excluded host leaked into status: {excluded}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
