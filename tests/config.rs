use rotter::{Mode, Options, extract};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LUA2: &str = r#"[language.lua2]
path = "lua-src"
symbol = "tree_sitter_lua"
extensions = ["lua2"]
units = ["function_declaration", "variable_declaration", "assignment_statement", "field"]
functions = ["function_declaration"]
function_values = ["function_definition"]
"#;

fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rotter-cfg-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
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

/// A committed repository with these files.
fn repo(files: &[(&str, &str)]) -> PathBuf {
    let repo = temp("repo");
    for (path, content) in files {
        fs::write(repo.join(path), content).unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    repo
}

fn write_config(xdg: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(xdg.join("rotter")).unwrap();
    let path = xdg.join("rotter/config.toml");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    path
}

struct Run {
    code: i32,
    report: Value,
    stderr: String,
}

impl Run {
    fn after(&self, path: &str) -> &Value {
        let file = self.report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["new_path"] == path)
            .unwrap_or_else(|| panic!("no {path} in {}", self.report));
        &file["after"]
    }

    fn status(&self, path: &str) -> &str {
        self.after(path)["status"].as_str().unwrap()
    }
}

/// Runs `rotter extract --full -C <repo>` with `env` (None removes the variable) in `cwd`.
fn extract_cli(repo: &Path, args: &[&str], env: &[(&str, Option<&Path>)], cwd: &Path) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
    command
        .args(["extract", "--full", "-C"])
        .arg(repo)
        .args(args)
        .current_dir(cwd);
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let output = command.output().unwrap();
    Run {
        code: output.status.code().unwrap(),
        report: serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// Config and cache pinned to `xdg`.
fn xdg_env(xdg: &Path) -> [(&'static str, Option<&Path>); 2] {
    [
        ("XDG_CONFIG_HOME", Some(xdg)),
        ("XDG_CACHE_HOME", Some(xdg)),
    ]
}

#[test]
fn enabled_external_without_library_is_parser_not_installed() {
    let repo = repo(&[
        ("a.lua2", "-- A.\nlocal a = 1\n"),
        ("b.txt", "-- B.\nlocal b = 1\n"),
        ("c.lua", "-- C.\nlocal c = 1\n"),
        ("e.lua", "# E.\ne() { :; }\n"),
        ("f.lua", "-- F.\nlocal f = 1\n"),
        ("README.md", "# readme\n"),
    ]);
    let xdg = temp("xdg");
    write_config(
        &xdg,
        &format!("{LUA2}\n[overrides]\n\"c.lua\" = \"lua2\"\n\"e.lua\" = \"lua2\"\n"),
    );
    let run = extract_cli(
        &repo,
        &["--lang", "b.txt=lua2", "--lang", "e.lua=bash"],
        &xdg_env(&xdg),
        &repo,
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    for (path, dialect) in [
        ("a.lua2", "lua2"),
        ("b.txt", "lua2-by-override"),
        ("c.lua", "lua2-by-override"),
    ] {
        let side = run.after(path);
        assert_eq!(side["status"], "parser_not_installed", "{path}: {side}");
        assert_eq!(side["language"], "lua2");
        assert_eq!(side["dialect"], dialect, "{path}");
        let detail = side["detail"].as_str().unwrap();
        assert!(detail.contains("rotter parser install lua2"), "{detail}");
        assert!(detail.contains("user approval"), "{detail}");
    }
    // --lang beats [overrides], which beats builtin extensions.
    assert_eq!(run.after("e.lua")["dialect"], "bash-by-override");
    assert_eq!(run.status("e.lua"), "ok");
    assert_eq!(run.status("f.lua"), "ok");
    assert_eq!(run.status("README.md"), "not_in_scope");
    assert_eq!(run.report["complete"], false);

    let plain = extract_cli(&repo, &[], &xdg_env(&xdg), &repo);
    assert_eq!(plain.status("b.txt"), "not_in_scope");
    let unknown = extract_cli(&repo, &["--lang", "b.txt=lua3"], &xdg_env(&xdg), &repo);
    assert_eq!(unknown.code, 2);
    assert!(unknown.stderr.contains("unknown --lang language: lua3"));
    for dir in [repo, xdg] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn invalid_config_is_an_error() {
    let repo = repo(&[("a.go", "package p\n")]);
    let xdg = temp("xdg");
    for text in [
        "parse_timeout_seconds = 3601\n".to_owned(),
        "parse_timeout_seconds = \"60\"\n".to_owned(),
        LUA2.replace("[\"lua2\"]", "[\"lua\"]"),
        "[overrides]\n\"*.go\" = \"python\"\n".to_owned(),
    ] {
        write_config(&xdg, &text);
        let run = extract_cli(&repo, &[], &xdg_env(&xdg), &repo);
        assert_eq!(run.code, 2, "{text}");
        assert!(run.stderr.contains("config.toml"), "{}", run.stderr);
    }
    for dir in [repo, xdg] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn untrusted_or_repository_config_is_ignored_with_a_note() {
    let repo = repo(&[("a.lua2", "-- A.\nlocal a = 1\n")]);
    let xdg = temp("xdg");
    let path = write_config(&xdg, LUA2);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    let run = extract_cli(&repo, &[], &xdg_env(&xdg), &repo);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("refused"), "{}", run.stderr);
    assert!(run.stderr.contains("external languages disabled"));
    assert_eq!(run.status("a.lua2"), "not_in_scope");

    // Inside the analysed repository, directly and through a symlink from outside.
    let inside = repo.join(".config");
    write_config(&inside, LUA2);
    let run = extract_cli(&repo, &[], &xdg_env(&inside), &repo);
    assert!(
        run.stderr.contains("inside the repository"),
        "{}",
        run.stderr
    );
    assert_eq!(run.status("a.lua2"), "not_in_scope");
    let linked = temp("linked");
    fs::create_dir_all(linked.join("rotter")).unwrap();
    symlink(
        inside.join("rotter/config.toml"),
        linked.join("rotter/config.toml"),
    )
    .unwrap();
    let run = extract_cli(&repo, &[], &xdg_env(&linked), &repo);
    assert!(
        run.stderr.contains("inside the repository"),
        "{}",
        run.stderr
    );
    assert_eq!(run.status("a.lua2"), "not_in_scope");
    for dir in [repo, xdg, linked] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn relative_xdg_config_home_is_ignored() {
    let repo = repo(&[("a.lua2", "-- A.\nlocal a = 1\n")]);
    let home = temp("home");
    write_config(&home.join(".config"), LUA2);
    // Used if the relative value were honoured: an invalid config in the working directory.
    write_config(&repo.join("rel"), "parse_timeout_seconds = 0\n");
    let env = [
        ("HOME", Some(home.as_path())),
        ("XDG_CONFIG_HOME", Some(Path::new("rel"))),
    ];
    let run = extract_cli(&repo, &[], &env, &repo);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert_eq!(run.status("a.lua2"), "parser_not_installed");
    for dir in [repo, home] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn without_home_no_config_is_read_from_the_working_directory() {
    let repo = repo(&[("a.lua2", "-- A.\nlocal a = 1\n")]);
    let cwd = temp("cwd");
    write_config(&cwd.join(".config"), LUA2);
    fs::create_dir_all(cwd.join(".cache/rotter/parsers")).unwrap();
    fs::write(
        cwd.join(".cache/rotter/parsers/lua2.dylib"),
        "not a library",
    )
    .unwrap();
    let env = [
        ("HOME", None),
        ("XDG_CONFIG_HOME", None),
        ("XDG_CACHE_HOME", None),
    ];
    let run = extract_cli(&repo, &[], &env, &cwd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("no config location"), "{}", run.stderr);
    assert_eq!(run.status("a.lua2"), "not_in_scope");
    for dir in [repo, cwd] {
        fs::remove_dir_all(dir).unwrap();
    }
}

fn options(mode: Mode) -> Options {
    let mut options = Options::new(mode);
    options.include_untracked = true;
    options
}

#[test]
fn zero_parse_timeout_reports_every_in_scope_side() {
    let repo = repo(&[
        ("a.go", "package p\n\n// F.\nfunc F() {}\n"),
        ("b.lua", "-- G.\nlocal function g() end\n"),
        ("README.md", "# readme\n"),
    ]);
    let status = |report: &rotter::Report, path: &str| {
        let file = report
            .json
            .get("files")
            .as_arr()
            .iter()
            .find(|file| file.get("new_path").as_str() == Some(path))
            .unwrap();
        file.get("after").get("status").as_str().unwrap().to_owned()
    };
    let mut timed = options(Mode::Full);
    timed.parse_timeout = Duration::ZERO;
    let report = extract(&repo, &timed).unwrap();
    assert!(!report.complete);
    assert_eq!(status(&report, "a.go"), "parse_timeout");
    assert_eq!(status(&report, "b.lua"), "parse_timeout");
    assert_eq!(status(&report, "README.md"), "not_in_scope");
    let detail = report.json.get("files").as_arr()[1]
        .get("after")
        .get("detail");
    assert!(
        detail.as_str().unwrap().contains("parse_timeout_seconds"),
        "{detail}"
    );

    let report = extract(&repo, &options(Mode::Full)).unwrap();
    assert!(report.complete, "{}", report.json);
    assert_eq!(status(&report, "a.go"), "ok");
    assert_eq!(
        report.json.get("files").as_arr()[1]
            .get("after")
            .get("units")
            .as_arr()
            .len(),
        1
    );
    fs::remove_dir_all(repo).unwrap();
}

#[test]
fn passed_run_deadline_skips_remaining_files() {
    let repo = repo(&[
        ("a.go", "package p\n\n// F.\nfunc F() {}\n"),
        ("b.lua", "-- G.\nlocal function g() end\n"),
    ]);
    fs::write(
        repo.join("a.go"),
        "package p\n\n// F.\nfunc F() { _ = 1 }\n",
    )
    .unwrap();
    fs::write(
        repo.join("b.lua"),
        "-- G.\nlocal function g() return 1 end\n",
    )
    .unwrap();
    let mut late = options(Mode::Worktree);
    late.deadline = Some(Instant::now());
    let report = extract(&repo, &late).unwrap();
    assert!(!report.complete);
    let files = report.json.get("files").as_arr();
    assert_eq!(files.len(), 2);
    for file in files {
        for side in ["before", "after"] {
            assert_eq!(
                file.get(side).get("status").as_str(),
                Some("parse_timeout"),
                "{file}"
            );
        }
        assert!(file.get("hunks").as_arr().is_empty(), "no diff was run");
    }
    fs::remove_dir_all(repo).unwrap();
}
