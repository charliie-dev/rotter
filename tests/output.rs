//! Output formats: `rotter extract`'s default JSON stays byte-identical to the goldens captured
//! from the binary before `--pretty` existed (9f4f132); `--pretty` extract goldens; the output
//! flags; and every other command's JSON document.
//!
//! Goldens are deterministic: fixture commits use a pinned identity and dates, and exactly one
//! substitution is applied to stdout, the JSON-escaped canonical repository path to `@REPO@`.
//! With `ROTTER_BLESS_DIR=<dir>` the tests write what they would compare into `<dir>` instead.

mod common;

use common::rotter;
use rotter::json::Json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Every string field of the injection fixture carries these (ESC, OSC 52, BEL, CR, BS, DEL, C1
/// CSI and OSC, bidi overrides).
const EVIL: &str = "\u{1b}[2J\u{1b}]52;c;aGk=\u{7}\r\u{8}\u{7f}\u{9b}\u{9d}\u{202e}\u{2066}";

/// Git isolated from the user's configuration, with a pinned identity and dates so commit ids
/// are stable.
fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "rotter")
        .env("GIT_AUTHOR_EMAIL", "rotter@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_NAME", "rotter")
        .env("GIT_COMMITTER_EMAIL", "rotter@example.invalid")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+0000")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// A repository under `root` built from `commits`, each a list of `(path, Some(content))` writes
/// and `(path, None)` removals committed in order.
fn repo(root: &Path, commits: &[&[(&str, Option<&str>)]]) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for changes in commits {
        for (path, content) in *changes {
            match content {
                Some(content) => fs::write(repo.join(path), content).unwrap(),
                None => fs::remove_file(repo.join(path)).unwrap(),
            }
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "c"]);
    }
    fs::canonicalize(repo).unwrap()
}

/// A fixture: its repository and the extract arguments it is reported with.
struct Case {
    name: &'static str,
    args: &'static [&'static str],
    code: i32,
    build: fn(&Path) -> PathBuf,
}

const OLD: &str = "package p\n\n// Old is renamed.\nfunc Old() int {\n\treturn 1\n}\n";

/// Modified, renamed, deleted and an out-of-scope file; everything parses (exit 0).
fn complete(root: &Path) -> PathBuf {
    repo(
        root,
        &[
            &[
                (
                    "a.go",
                    Some("package p\n\n// F returns one.\nfunc F() int {\n\treturn 1\n}\n"),
                ),
                ("old.go", Some(OLD)),
                (
                    "gone.go",
                    Some("package p\n\n// Gone goes.\nfunc Gone() {}\n"),
                ),
                ("notes.txt", Some("one\n")),
            ],
            &[
                (
                    "a.go",
                    Some("package p\n\n// F returns two.\nfunc F() int {\n\treturn 2\n}\n"),
                ),
                ("old.go", None),
                ("new.go", Some(&OLD.replace("return 1", "return 3"))),
                ("gone.go", None),
                ("notes.txt", Some("two\n")),
            ],
        ],
    )
}

/// A committed syntax error: the side is `partial` (exit 1).
fn partial(root: &Path) -> PathBuf {
    repo(
        root,
        &[
            &[(
                "b.go",
                Some("package p\n\n// G is fine.\nfunc G() int { return 1 }\n"),
            )],
            &[(
                "b.go",
                Some("package p\n\n// G is fine.\nfunc G() int { return 2 }\n\nfunc H( {\n"),
            )],
        ],
    )
}

/// Control, bidi and newline characters in a path, a comment and an untracked name, a lone CR
/// in a comment and a CRLF file.
fn injection(root: &Path) -> PathBuf {
    let path = format!("e{EVIL}\n.go");
    let before = "package p\n\n// F says hi.\nfunc F() int { return 1 }\n";
    let after = format!("package p\n\n// F says {EVIL} hi.\nfunc F() int {{ return 2 }}\n");
    let repo = repo(
        root,
        &[
            &[
                (&path, Some(before)),
                (
                    "crlf.go",
                    Some("package p\r\n\r\n// G is crlf.\r\nfunc G() int { return 1 }\r\n"),
                ),
            ],
            &[
                (&path, Some(&after)),
                (
                    "crlf.go",
                    Some("package p\r\n\r\n// G is crlf.\r\nfunc G() int { return 2 }\r\n"),
                ),
            ],
        ],
    );
    fs::write(repo.join(format!("u{EVIL}\n.txt")), "untracked\n").unwrap();
    repo
}

/// A commented function longer than `--full`'s 80-line text limit.
fn truncated(root: &Path) -> PathBuf {
    let body: String = (0..90).map(|line| format!("\tx += {line}\n")).collect();
    let source =
        format!("package p\n\n// Long adds.\nfunc Long() (x int) {{\n{body}\treturn\n}}\n");
    repo(root, &[&[("long.go", Some(&source))]])
}

const HEAD_CASES: [Case; 3] = [
    Case {
        name: "complete",
        args: &["extract", "--base", "HEAD~1"],
        code: 0,
        build: complete,
    },
    Case {
        name: "partial",
        args: &["extract", "--base", "HEAD~1"],
        code: 1,
        build: partial,
    },
    Case {
        name: "injection",
        args: &["extract", "--base", "HEAD~1"],
        code: 0,
        build: injection,
    },
];

/// `--pretty --color=never` goldens, captured from this binary and reviewed.
const PRETTY_CASES: [Case; 4] = [
    Case {
        name: "complete",
        args: &["extract", "--base", "HEAD~1", "--pretty", "--color=never"],
        code: 0,
        build: complete,
    },
    Case {
        name: "partial",
        args: &["extract", "--pretty", "--base", "HEAD~1", "--color=never"],
        code: 1,
        build: partial,
    },
    Case {
        name: "truncated",
        args: &["extract", "--full", "--pretty", "--color=never"],
        code: 0,
        build: truncated,
    },
    Case {
        name: "injection",
        args: &["extract", "--base", "HEAD~1", "--pretty", "--color=never"],
        code: 0,
        build: injection,
    },
];

/// `stdout` with every occurrence of the JSON-escaped `repo` replaced by `@REPO@`, the only
/// normalisation.
fn substitute(stdout: &[u8], repo: &Path) -> Vec<u8> {
    let escaped = Json::from(repo.to_str().unwrap()).to_string();
    let escaped = &escaped.as_bytes()[1..escaped.len() - 1];
    let mut out = Vec::new();
    let mut rest = stdout;
    while !rest.is_empty() {
        if rest.starts_with(escaped) {
            out.extend_from_slice(b"@REPO@");
            rest = &rest[escaped.len()..];
        } else {
            out.push(rest[0]);
            rest = &rest[1..];
        }
    }
    out
}

fn run(root: &Path, repo: &Path, args: &[&str]) -> Output {
    rotter(root, args, repo, "", &[])
}

/// Compares `actual` with the golden `name` byte for byte (or blesses it), after checking that
/// no part of the temp path survived the substitution.
fn check_golden(name: &str, actual: &[u8], root: &Path) {
    let text = String::from_utf8_lossy(actual);
    let temp = std::env::temp_dir();
    let mut leaks = vec![
        "/private".to_owned(),
        "/var/folders".to_owned(),
        temp.display().to_string(),
    ];
    leaks.extend(
        root.components()
            .filter_map(|part| part.as_os_str().to_str())
            .filter(|part| part.starts_with("rotter-"))
            .map(str::to_owned),
    );
    for leak in &leaks {
        assert!(!text.contains(leak.as_str()), "{name} contains {leak:?}");
    }
    if let Some(dir) = std::env::var_os("ROTTER_BLESS_DIR") {
        fs::write(Path::new(&dir).join(name), actual).unwrap();
        return;
    }
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/output")
        .join(name);
    let expected =
        fs::read(&golden).unwrap_or_else(|error| panic!("{}: {error}", golden.display()));
    assert!(
        expected == actual,
        "{name} differs from its golden\n--- golden\n{}\n--- actual\n{text}",
        String::from_utf8_lossy(&expected)
    );
}

/// The default JSON of each case, in two differently named temp dirs, equals the HEAD golden.
#[test]
fn default_extract_output_is_byte_identical_to_head() {
    for case in &HEAD_CASES {
        for label in ["golden-a", "golden-longer-name"] {
            let root = common::temp(label);
            let repo = (case.build)(&root);
            let output = run(&root, &repo, case.args);
            assert_eq!(
                output.status.code(),
                Some(case.code),
                "{}: {output:?}",
                case.name
            );
            check_golden(
                &format!("{}.json", case.name),
                &substitute(&output.stdout, &repo),
                &root,
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
}

/// The same fixtures rendered with `--pretty --color=never`, byte for byte.
#[test]
fn pretty_extract_matches_its_goldens() {
    for case in &PRETTY_CASES {
        for label in ["pretty-a", "pretty-longer-name"] {
            let root = common::temp(label);
            let repo = (case.build)(&root);
            let output = run(&root, &repo, case.args);
            assert_eq!(
                output.status.code(),
                Some(case.code),
                "{}: {output:?}",
                case.name
            );
            assert!(
                !output.stdout.contains(&0x1b),
                "{}: ESC with --color=never",
                case.name
            );
            check_golden(
                &format!("{}.pretty.txt", case.name),
                &substitute(&output.stdout, &repo),
                &root,
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
}

/// `text` without rotter's own SGR runs (`ESC [ <digits and ;> m`).
fn strip_sgr(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("\u{1b}[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(after.len());
        if after[end..].starts_with('m') {
            rest = &after[end + 1..];
        } else {
            // Not an SGR run: keep the ESC so the caller's check sees it.
            out.push_str("\u{1b}[");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// No C1, bidi or zero-width character and no ESC, BEL, BS, CR or DEL outside SGR runs.
fn assert_inert(text: &str, what: &str) {
    for c in strip_sgr(text).chars() {
        assert!(
            !matches!(c, '\u{80}'..='\u{9f}' | '\u{1b}' | '\u{7}' | '\u{8}' | '\r' | '\u{7f}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}'
                | '\u{61c}' | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'),
            "{what}: raw {c:?} in {text:?}"
        );
    }
}

#[test]
fn colour_is_forced_by_always_and_off_through_a_pipe() {
    let root = common::temp("colour");
    let repo = injection(&root);
    let always = run(
        &root,
        &repo,
        &["extract", "--base", "HEAD~1", "--pretty", "--color=always"],
    );
    assert_eq!(always.status.code(), Some(0), "{always:?}");
    let text = String::from_utf8(always.stdout).unwrap();
    assert!(
        text.contains("\u{1b}[1m") && text.contains("\u{1b}[0m"),
        "{text}"
    );
    // Every style opened on a line is reset on that line.
    for line in text.lines().filter(|line| line.contains('\u{1b}')) {
        let resets = line.matches("\u{1b}[0m").count();
        assert_eq!(line.matches('\u{1b}').count(), 2 * resets, "{line:?}");
        let tail = &line[line.rfind("\u{1b}[0m").unwrap()..];
        assert_eq!(tail.matches('\u{1b}').count(), 1, "{line:?}");
    }
    assert_inert(&text, "--color=always");
    assert!(text.contains("\\u{1b}[2J") && text.contains("\\u{9b}") && text.contains("\\u{a}"));
    // The CRLF file shows no CR marker; the lone CR in the comment is escaped.
    assert!(text.contains("// G is crlf.\u{1b}[0m"), "{text}");
    assert!(!text.contains("crlf.\\u{d}"), "{text}");
    assert!(text.contains("\\u{d}\\u{8}"), "{text}");
    // Auto through a pipe (the test's stdout is not a terminal), even with TERM set: no colour.
    for env in [
        &[][..],
        &[("TERM", Some(std::ffi::OsStr::new("xterm-256color")))],
    ] {
        let auto = rotter(
            &root,
            &["extract", "--base", "HEAD~1", "--pretty"],
            &repo,
            "",
            env,
        );
        assert_eq!(auto.status.code(), Some(0), "{auto:?}");
        assert!(!auto.stdout.contains(&0x1b), "{auto:?}");
        assert_inert(&String::from_utf8(auto.stdout).unwrap(), "auto");
    }
    fs::remove_dir_all(root).unwrap();
}

/// Usage errors: exit 2, the message on stderr, nothing on stdout.
fn usage_error(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(message) && stderr.contains("usage:"),
        "{stderr}"
    );
}

#[test]
fn output_flags_are_validated_and_stop_at_the_pathspec() {
    let root = common::temp("flags");
    let repo = complete(&root);
    let base = ["extract", "--base", "HEAD~1"];
    let with = |extra: &[&str]| {
        let mut args = base.to_vec();
        args.extend(extra);
        run(&root, &repo, &args)
    };
    usage_error(
        &with(&["--color=never"]),
        "--color applies only with --pretty",
    );
    usage_error(
        &with(&["--pretty", "--color=sometimes"]),
        "unknown --color value: sometimes",
    );
    usage_error(
        &with(&["--pretty", "--color", "always"]),
        "--color=<auto|always|never>",
    );
    usage_error(
        &run(&root, &repo, &["integration", "status", "--color=always"]),
        "--color applies only with --pretty",
    );
    // After `--` they are pathspecs, as is --help.
    for spec in ["--help", "--pretty", "--color=always"] {
        let output = with(&["--", spec]);
        assert_eq!(output.status.code(), Some(0), "{spec}: {output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["pathspec"], serde_json::json!([spec]), "{spec}");
        assert_eq!(report["files"], serde_json::json!([]), "{spec}");
    }
    let output = run(&root, &repo, &["extract", "--worktree", "--", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["schema"],
        "rotter.extract.poc/0"
    );
    // Before `--`, --help still prints the usage.
    let output = run(&root, &repo, &["extract", "--worktree", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("usage: rotter"));
    // The hook branch never takes the flags: exactly today's unknown-hook behaviour.
    for args in [
        &["hook", "claude", "--pretty"][..],
        &["hook", "claude", "--color=never"],
    ] {
        let output = rotter(&root, args, &root, "{}", &[]);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unknown hook"),
            "{output:?}"
        );
    }
    // --skill keeps its exact form.
    let output = run(&root, &repo, &["--skill", "--pretty"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    fs::remove_dir_all(root).unwrap();
}

/// The one JSON document on stdout, checked to be rotter's with `schema`.
fn document(output: &Output, schema: &str) -> serde_json::Value {
    let text = std::str::from_utf8(&output.stdout).unwrap();
    assert!(text.ends_with("}\n"), "{text}");
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        (&value["tool"], &value["schema"]),
        (&"rotter".into(), &schema.into())
    );
    value
}

fn keys(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

const IN_SCOPE: [&str; 15] = [
    "claude",
    "grok",
    "codex",
    "copilot",
    "droid",
    "pi",
    "letta",
    "opencode",
    "omp",
    "kilo",
    "hermes",
    "mastracode",
    "devin",
    "cursor",
    "antigravity-cli",
];

#[test]
fn status_document_covers_every_state() {
    let root = common::temp("status-doc");
    let root = fs::canonicalize(&root).unwrap();
    let exe = env!("CARGO_BIN_EXE_rotter");
    for dir in [
        "claude",
        "grok/hooks",
        "codex",
        "copilot/hooks",
        "home/.factory",
        "pi",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
        common::set_mode(&root.join(dir), 0o700);
    }
    let integration = |args: &[&str], env: &[(&str, Option<&std::ffi::OsStr>)]| {
        let mut all = vec!["integration"];
        all.extend(args);
        rotter(&root, &all, &root, "", env)
    };
    assert_eq!(
        integration(&["install", "claude"], &[]).status.code(),
        Some(0)
    );
    assert_eq!(integration(&["install", "pi"], &[]).status.code(), Some(0));
    // pi: a shim rendered with another timeout; grok: a file rotter did not write; copilot:
    // another binary's; codex and droid: unreadable hook files.
    let shim = root.join("pi/extensions/rotter-review.ts");
    fs::write(
        &shim,
        rotter::integration::render_shim("pi", exe, 61).unwrap(),
    )
    .unwrap();
    let grok = root.join("grok/hooks/rotter.json");
    fs::write(&grok, r#"{"hooks": {"Stop": []}}"#).unwrap();
    common::set_mode(&grok, 0o600);
    let copilot = root.join("copilot/hooks/rotter.json");
    let other = serde_json::json!({ "version": 1, "hooks": { "agentStop": [{ "type": "command",
        "bash": "'/else/rotter' hook copilot || true", "timeoutSec": 90 }] } });
    fs::write(&copilot, other.to_string()).unwrap();
    common::set_mode(&copilot, 0o600);
    fs::write(root.join("codex/hooks.json"), "{").unwrap();
    fs::write(root.join("home/.factory/hooks.json"), "[1").unwrap();

    let output = integration(&["status"], &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let status = document(&output, "rotter.status/1");
    assert_eq!(
        keys(&status),
        ["tool", "schema", "hosts", "compat", "git", "executable"]
    );
    let hosts = status["hosts"].as_array().unwrap();
    let ids: Vec<&str> = hosts
        .iter()
        .map(|host| host["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, IN_SCOPE, "all 15 hosts in table order");
    for host in hosts {
        assert_eq!(
            keys(host),
            ["id", "support", "state", "path", "detail", "notes"]
        );
    }
    let host = |id: &str| hosts.iter().find(|host| host["id"] == id).unwrap();
    let path = |relative: &str| root.join(relative).display().to_string();
    assert_eq!(
        *host("claude"),
        serde_json::json!({ "id": "claude", "support": "stable", "state": "installed",
            "path": path("claude/settings.json"), "detail": "installed (current)", "notes": [] })
    );
    assert_eq!(host("pi")["state"], "mismatch");
    assert_eq!(host("pi")["support"], "experimental");
    assert_eq!(
        host("pi")["detail"],
        "installed (timeout 61, expected 90); run `rotter integration install pi`"
    );
    assert_eq!(host("pi")["path"], path("pi/extensions/rotter-review.ts"));
    assert_eq!(
        (&host("grok")["state"], &host("grok")["path"]),
        (&"foreign".into(), &serde_json::Value::Null)
    );
    assert_eq!(
        host("grok")["detail"],
        format!("{} is not managed by rotter; left alone", grok.display())
    );
    assert_eq!(host("copilot")["state"], "other_binary");
    assert_eq!(
        host("copilot")["detail"],
        "installed for another binary: '/else/rotter' hook copilot || true"
    );
    for id in ["codex", "droid"] {
        assert_eq!(host(id)["state"], "error", "{id}");
        assert!(
            host(id)["detail"].as_str().unwrap().contains("hooks.json"),
            "{id}"
        );
    }
    assert_eq!(
        host("codex")["notes"],
        serde_json::json!([
            "hooks feature on (default); Codex runs a new or changed hook only \
            after you trust it in /hooks (not checked)"
        ])
    );
    for id in ["letta", "opencode"] {
        assert_eq!(
            (&host(id)["state"], &host(id)["detail"]),
            (&"not_installed".into(), &"not installed".into()),
            "{id}"
        );
    }
    for id in &IN_SCOPE[8..] {
        assert_eq!(host(id)["support"], "unsupported", "{id}");
        assert_eq!(host(id)["state"], serde_json::Value::Null, "{id}");
        assert!(host(id)["detail"].is_string(), "{id}");
    }
    assert_eq!(
        status["compat"],
        serde_json::json!({ "state": "not_found", "detail": null })
    );
    let git = fs::canonicalize(common::real_git()).unwrap();
    assert_eq!(status["git"]["path"], git.display().to_string());
    assert!(
        status["git"]["detail"]
            .as_str()
            .unwrap()
            .starts_with("git version ")
    );
    assert_eq!(
        status["executable"],
        serde_json::json!({ "path": exe, "trusted": true, "detail": "safe to register" })
    );

    // Grok's Claude compatibility: found, then unknown.
    let dot = root.join("home/.claude");
    fs::create_dir_all(&dot).unwrap();
    let same = [("CLAUDE_CONFIG_DIR", Some(dot.as_os_str()))];
    assert_eq!(
        integration(&["install", "claude"], &same).status.code(),
        Some(0)
    );
    let status = document(&integration(&["status"], &same), "rotter.status/1");
    assert_eq!(status["compat"]["state"], "found");
    assert!(
        status["compat"]["detail"]
            .as_str()
            .unwrap()
            .ends_with(&format!("({})", dot.join("settings.json").display()))
    );
    fs::write(dot.join("settings.json"), "{").unwrap();
    let status = document(&integration(&["status"], &[]), "rotter.status/1");
    assert_eq!(status["compat"]["state"], "unknown");
    assert!(
        status["compat"]["detail"]
            .as_str()
            .unwrap()
            .starts_with("Claude compatibility entry unknown: "),
        "{status}"
    );

    // --pretty: structural only (paths, git and exe vary); aligned columns, no ESC.
    let output = integration(&["status", "--pretty", "--color=never"], &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains('\u{1b}'));
    // Host rows: the table up to the first blank line, without the indented details and notes.
    let rows: Vec<&str> = text
        .lines()
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter(|line| !line.starts_with(' '))
        .collect();
    let named: Vec<&str> = rows
        .iter()
        .map(|row| row.split(' ').next().unwrap())
        .collect();
    assert_eq!(named, IN_SCOPE);
    let header = text.lines().next().unwrap();
    assert!(header.starts_with("host "), "{text}");
    let column = header.find("support").unwrap();
    for row in &rows {
        assert!(row[..column].ends_with("  "), "{row:?}");
        assert!(!row[column..].starts_with(' '), "{row:?}");
    }
    for label in ["grok compat ", "git ", "executable "] {
        assert!(
            text.lines().any(|line| line.starts_with(label)),
            "{label}: {text}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn integration_and_parser_list_documents() {
    let root = common::temp("integration-doc");
    let root = fs::canonicalize(&root).unwrap();
    fs::create_dir_all(root.join("grok")).unwrap();
    common::set_mode(&root.join("grok"), 0o700);
    let file = root.join("grok/hooks/rotter.json").display().to_string();
    let run = |args: &[&str]| rotter(&root, args, &root, "", &[]);
    for (args, action, result) in [
        (["integration", "install", "grok"], "install", "installed"),
        (
            ["integration", "install", "grok"],
            "install",
            "already_installed",
        ),
        (["integration", "uninstall", "grok"], "uninstall", "removed"),
        (
            ["integration", "uninstall", "grok"],
            "uninstall",
            "not_installed",
        ),
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(
            document(&output, "rotter.integration/1"),
            serde_json::json!({ "tool": "rotter", "schema": "rotter.integration/1",
                "host": "grok", "action": action, "result": result, "path": file,
                "notes": [] })
        );
    }
    let output = run(&["integration", "install", "grok", "--pretty"]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("grok  installed  {file}\n")
    );
    let list = document(&run(&["parser", "list"]), "rotter.parsers/1");
    let parsers = list["parsers"].as_array().unwrap();
    assert!(!parsers.is_empty());
    for parser in parsers {
        assert_eq!(
            keys(parser),
            ["name", "enabled", "installed", "path", "source", "detail"]
        );
    }
    let python = parsers
        .iter()
        .find(|parser| parser["name"] == "python")
        .unwrap();
    assert_eq!(
        *python,
        serde_json::json!({ "name": "python", "enabled": false, "installed": false,
            "path": null, "source": null,
            "detail": "add \"python\" to languages in config.toml" })
    );
    let text = String::from_utf8(run(&["parser", "list", "--pretty"]).stdout).unwrap();
    assert!(text.starts_with("name "), "{text}");
    assert!(text.contains("\npython      available  -\n"), "{text}");
    fs::remove_dir_all(root).unwrap();
}
