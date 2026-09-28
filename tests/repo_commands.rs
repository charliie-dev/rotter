//! Repository-defined commands (filter drivers, hooks, promisor fetches, submodule status) never
//! run while rotter extracts. Every marker case first runs rotter on a fresh fixture (negative),
//! then plain git on a rebuilt equivalent fixture (positive control), proving the fixture really
//! reaches the command.

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// An object id no fixture contains.
const MISSING: &str = "1234567890123456789012345678901234567890";

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    marker: PathBuf,
    /// Prepended to PATH for rotter (a git wrapper), if any.
    wrapper: Option<PathBuf>,
}

impl Fixture {
    /// A repository whose committed `a.rs` has a documented function, changed in the work tree.
    fn changed(name: &str) -> Self {
        // Only safe characters: the marker path is quoted into git config values.
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let root = std::env::temp_dir().join(format!(
            "rotter-cmd-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let fixture = Self {
            repo: root.join("repo"),
            marker: root.join("marker"),
            root,
            wrapper: None,
        };
        fs::create_dir(&fixture.repo).unwrap();
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.write("a.rs", "// F returns one.\nfn f() -> i32 {\n    1\n}\n");
        fixture.commit();
        fixture.write("a.rs", "// F returns one.\nfn f() -> i32 {\n    2\n}\n");
        fixture
    }

    /// Plain git isolated from the user's configuration; lazy fetches stay possible.
    fn plain_git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_NO_LAZY_FETCH")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.plain_git(args);
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.repo.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "commit"]);
    }

    /// Commits only `paths`, keeping the work-tree change to `a.rs`.
    fn commit_only(&self, paths: &[&str]) {
        let mut args = vec!["add", "--"];
        args.extend(paths);
        self.git(&args);
        self.git(&["commit", "-q", "-m", "more"]);
    }

    fn append_config(&self, text: &str) {
        let path = self.repo.join(".git/config");
        let mut config = fs::read_to_string(&path).unwrap();
        config.push_str(text);
        fs::write(path, config).unwrap();
    }

    fn attributes(&self, line: &str) {
        fs::create_dir_all(self.repo.join(".git/info")).unwrap();
        fs::write(self.repo.join(".git/info/attributes"), format!("{line}\n")).unwrap();
    }

    /// A shell command that creates the marker.
    fn touch(&self) -> String {
        format!("touch '{}'", self.marker.display())
    }

    /// An executable script that creates the marker and fails.
    fn script(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{}\nexit 1\n", self.touch())).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn marked(&self) -> bool {
        self.marker.exists()
    }

    /// Runs rotter in the repository with every user directory pinned under the fixture.
    fn rotter(&self, args: &[&str], input: Option<&str>) -> Output {
        let home = self.root.join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
        command
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("ROTTER_STATE_DIR", home.join("rotter-state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_NO_LAZY_FETCH");
        if let Some(wrapper) = &self.wrapper {
            let path = format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap());
            command.env("PATH", path);
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
            .write_all(input.unwrap_or_default().as_bytes())
            .unwrap();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("rotter {args:?} hangs");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }

    /// `hook claude-stop` for this repository in a fresh session.
    fn claude_stop(&self) -> Output {
        let session = format!("s{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let input = serde_json::json!({ "session_id": session, "cwd": self.repo }).to_string();
        let output = self.rotter(&["hook", "claude-stop"], Some(&input));
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        output
    }

    /// Replaces the HEAD commit's tree with one whose `a.rs` names a missing blob.
    fn missing_blob(&self) {
        let tree = self.pipe(
            &["mktree", "--missing"],
            &format!("100644 blob {MISSING}\ta.rs\n"),
        );
        let commit = self.git(&["commit-tree", &tree, "-p", "HEAD", "-m", "missing"]);
        self.git(&["update-ref", "refs/heads/main", &commit]);
    }

    /// Commits once more and deletes that commit's loose object: HEAD names a missing commit.
    fn missing_head(&self) {
        self.git(&["commit", "-q", "--allow-empty", "-m", "gone"]);
        let head = self.git(&["rev-parse", "HEAD"]);
        fs::remove_file(
            self.repo
                .join(format!(".git/objects/{}/{}", &head[..2], &head[2..])),
        )
        .unwrap();
    }

    fn pipe(&self, args: &[&str], input: &str) -> String {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// A `git` wrapper first on rotter's PATH: `version` prints `version`, `refuse` (a pattern of
    /// the whole argument list) exits 3, anything else runs the real git. Every call is logged.
    fn wrap(&mut self, version: Option<&str>, refuse: Option<&str>) {
        let dir = self.root.join("wrapper");
        fs::create_dir_all(&dir).unwrap();
        let log = self.root.join("wrapper.log");
        let mut script = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display());
        if let Some(version) = version {
            script += &format!("if [ \"$1\" = version ]; then echo '{version}'; exit 0; fi\n");
        }
        if let Some(pattern) = refuse {
            script += &format!("case \"$*\" in *'{pattern}'*) exit 3;; esac\n");
        }
        script += &format!("exec '{}' \"$@\"\n", real_git().display());
        fs::write(dir.join("git"), script).unwrap();
        fs::set_permissions(dir.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        self.wrapper = Some(dir);
    }

    fn wrapper_log(&self) -> String {
        fs::read_to_string(self.root.join("wrapper.log")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The git a plain `git` resolves to from the absolute PATH entries.
fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH")
}

/// The real git's `(major, minor, patch)`.
fn real_version() -> (u32, u32, u32) {
    let output = Command::new(real_git()).arg("version").output().unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let mut parts = text
        .trim()
        .trim_start_matches("git version ")
        .split(|c: char| !c.is_ascii_digit())
        .map(|part| part.parse().unwrap());
    (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    )
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}

fn paths(report: &Value) -> Vec<String> {
    report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["new_path"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Rotter through `hook claude-stop`, `extract --worktree` and `extract --base HEAD`: `a.rs` is
/// still reported and the marker never appears.
fn assert_rotter_safe(fixture: &Fixture, name: &str) {
    assert!(!fixture.marked(), "{name}: fixture setup ran the command");
    let hook = fixture.claude_stop();
    assert!(
        stdout(&hook).contains(r#""decision":"block""#),
        "{name}: {hook:?}"
    );
    assert!(!fixture.marked(), "{name}: hook claude-stop ran it");
    for args in [
        &["extract", "--worktree"][..],
        &["extract", "--base", "HEAD"],
    ] {
        let output = fixture.rotter(args, None);
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "{name}: {output:?}"
        );
        assert!(
            paths(&report(&output)).contains(&"a.rs".to_owned()),
            "{name} {args:?}: {output:?}"
        );
        assert!(!fixture.marked(), "{name}: {args:?} ran it");
    }
}

/// Negative run on a fresh fixture, then `positive` on a rebuilt one must create the marker.
fn case(name: &str, build: impl Fn(&Fixture), positive: impl Fn(&Fixture)) {
    let fixture = Fixture::changed(name);
    build(&fixture);
    assert_rotter_safe(&fixture, name);
    drop(fixture);
    let fixture = Fixture::changed(name);
    build(&fixture);
    assert!(!fixture.marked(), "{name}: fixture setup ran the command");
    positive(&fixture);
    assert!(fixture.marked(), "{name}: positive control never ran it");
}

fn plain_diff(fixture: &Fixture) {
    fixture.plain_git(&["diff", "--raw", "-M", "HEAD"]);
}

/// Sets `path`'s mtime far in the past without changing its content.
fn touch_old(path: &Path) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
}

#[test]
fn filter_drivers_never_run() {
    let body: String = (1..=12).map(|line| format!("// line {line}\n")).collect();
    // (config section header, attribute value): exact, `=`, `.`, section case and empty names.
    let drivers = [
        (r#"[filter "x"]"#, "x"),
        (r#"[filter "x=y"]"#, "x=y"),
        (r#"[filter "a.b"]"#, "a.b"),
        (r#"[FILTER "X"]"#, "X"),
        (r#"[filter ""]"#, ""),
    ];
    for (section, attribute) in drivers {
        for var in ["clean", "process"] {
            let name = format!("{section} {var}");
            let build = |fixture: &Fixture| {
                fixture.write("old.rs", &body);
                fixture.commit_only(&["old.rs"]);
                // A staged rename whose new file is modified: -M must read the work tree.
                fixture.git(&["mv", "old.rs", "new.rs"]);
                fixture.write("new.rs", &body.replace("line 12", "line twelve"));
                let then = if var == "clean" { "cat" } else { "exit 1" };
                fixture.append_config(&format!(
                    "{section}\n\t{var} = {}; {then}\n",
                    fixture.touch()
                ));
                fixture.attributes(&format!("*.rs filter={attribute}"));
            };
            case(&name, build, plain_diff);
        }
    }
}

#[test]
fn config_based_hooks_never_run() {
    if real_version() < (2, 54, 0) {
        eprintln!("skipped: config-based hooks need git 2.54");
        return;
    }
    let build = |fixture: &Fixture| {
        fixture.write("b.rs", "fn g() {}\n");
        fixture.commit_only(&["b.rs"]);
        touch_old(&fixture.repo.join("b.rs"));
        fixture.append_config(&format!(
            "[hook \"x\"]\n\tevent = post-index-change\n\tcommand = {}\n",
            fixture.touch()
        ));
    };
    case("config hook", build, plain_diff);
}

#[test]
fn hook_files_never_run() {
    let hook = |dir: &Path, fixture: &Fixture| {
        fs::create_dir_all(dir).unwrap();
        let script = fixture.script("hook");
        fs::rename(script, dir.join("post-index-change")).unwrap();
    };
    let build = |fixture: &Fixture| {
        fixture.write("b.rs", "fn g() {}\n");
        fixture.commit_only(&["b.rs"]);
        touch_old(&fixture.repo.join("b.rs"));
        hook(&fixture.repo.join(".git/hooks"), fixture);
    };
    case(".git/hooks", build, plain_diff);
    let build = |fixture: &Fixture| {
        fixture.write("b.rs", "fn g() {}\n");
        fixture.commit_only(&["b.rs"]);
        touch_old(&fixture.repo.join("b.rs"));
        hook(&fixture.repo.join("hooks"), fixture);
        fixture.append_config("[core]\n\thooksPath = hooks\n");
    };
    case("core.hooksPath", build, plain_diff);
}

#[test]
fn dirty_submodule_status_never_runs() {
    let build = |fixture: &Fixture| {
        let source = fixture.root.join("sub-source");
        fs::create_dir(&source).unwrap();
        let sub = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        sub(&["init", "-q", "-b", "main"]);
        fs::write(source.join("s.rs"), "fn s() -> i32 { 1 }\n").unwrap();
        sub(&["add", "-A"]);
        sub(&["commit", "-q", "-m", "s"]);
        let url = source.display().to_string();
        fixture.git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &url,
            "sub",
        ]);
        fixture.git(&["commit", "-q", "-m", "sub"]);
        // The submodule's own config and attributes filter its dirty (same-size) file.
        let inner = |args: &[&str]| {
            let mut all = vec!["-C", "sub"];
            all.extend(args);
            fixture.git(&all)
        };
        inner(&[
            "config",
            "filter.evil.clean",
            &format!("{}; cat", fixture.touch()),
        ]);
        let attributes = inner(&["rev-parse", "--git-path", "info/attributes"]);
        let attributes = fixture.repo.join("sub").join(attributes);
        fs::create_dir_all(attributes.parent().unwrap()).unwrap();
        fs::write(attributes, "*.rs filter=evil\n").unwrap();
        fs::write(fixture.repo.join("sub/s.rs"), "fn s() -> i32 { 2 }\n").unwrap();
    };
    case("submodule", build, plain_diff);
}

/// A local promisor remote whose upload-pack creates the marker.
fn promisor(fixture: &Fixture, config: &str) {
    let upload = fixture.script("upload-pack");
    fixture.append_config(
        &config
            .replace("UPLOAD", &upload.display().to_string())
            .replace("URL", &fixture.root.join("remote").display().to_string()),
    );
}

const PARTIAL_CLONE: &str = "[extensions]\n\tpartialClone = origin\n[remote \"origin\"]\n\t\
url = URL\n\tpromisor = true\n\tuploadpack = UPLOAD\n";

#[test]
fn promisor_objects_are_never_fetched_with_lazy_fetch_git() {
    assert!(
        real_version() >= (2, 45, 1),
        "this test needs git 2.45.1 or newer (GIT_NO_LAZY_FETCH) first on PATH"
    );
    // A missing before blob is that side's read_error.
    let build = |fixture: &Fixture| {
        promisor(fixture, PARTIAL_CLONE);
        fixture.missing_blob();
    };
    let fixture = Fixture::changed("lazy-blob");
    build(&fixture);
    for args in [
        &["extract", "--worktree"][..],
        &["extract", "--base", "HEAD"],
    ] {
        let output = fixture.rotter(args, None);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        let report = report(&output);
        assert_eq!(
            report["files"][0]["before"]["status"], "read_error",
            "{report}"
        );
    }
    let hook = stdout(&fixture.claude_stop());
    assert!(hook.contains("could not be analysed"), "{hook}");
    assert!(!fixture.marked(), "a lazy fetch ran");
    drop(fixture);
    let fixture = Fixture::changed("lazy-blob");
    build(&fixture);
    fixture.plain_git(&["cat-file", "blob", MISSING]);
    assert!(fixture.marked(), "positive control never fetched");
    drop(fixture);

    // A HEAD naming a missing commit is an extraction error.
    let build = |fixture: &Fixture| {
        promisor(fixture, PARTIAL_CLONE);
        fixture.missing_head();
    };
    let fixture = Fixture::changed("lazy-head");
    build(&fixture);
    let output = fixture.rotter(&["extract", "--worktree"], None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("HEAD does not name a readable commit"),
        "{output:?}"
    );
    assert_eq!(
        fixture
            .rotter(&["extract", "--base", "HEAD"], None)
            .status
            .code(),
        Some(2)
    );
    let hook = stdout(&fixture.claude_stop());
    assert!(hook.contains("extract failed"), "{hook}");
    assert!(!fixture.marked(), "a lazy fetch ran");
    drop(fixture);
    let fixture = Fixture::changed("lazy-head");
    build(&fixture);
    fixture.plain_git(&["rev-parse", "HEAD^{commit}"]);
    assert!(fixture.marked(), "positive control never fetched");
}

#[test]
fn gated_git_refuses_partial_clones_before_reading_objects() {
    let with =
        |key: &str| format!("{key}\n[remote \"origin\"]\n\turl = URL\n\tuploadpack = UPLOAD\n");
    let fixtures: Vec<(&str, String, bool, bool)> = vec![
        // (name, config, missing HEAD instead of a missing blob, positive control)
        ("partial clone", PARTIAL_CLONE.to_owned(), false, true),
        (
            "promisor yes",
            with("[remote \"origin\"]\n\tpromisor = yes"),
            false,
            true,
        ),
        (
            "filter only, blob",
            with("[remote \"origin\"]\n\tpartialclonefilter = blob:none"),
            false,
            true,
        ),
        (
            "filter only, head",
            with("[remote \"origin\"]\n\tpartialclonefilter = blob:none"),
            true,
            true,
        ),
        (
            "extension only",
            with("[extensions]\n\tpartialClone = origin"),
            false,
            true,
        ),
        (
            "promisor only",
            with("[remote \"origin\"]\n\tpromisor = true"),
            false,
            true,
        ),
        (
            "mixed-case extension",
            with("[Extensions]\n\tPartialClone = origin"),
            false,
            true,
        ),
        (
            "mixed-case remote",
            "[Remote \"Origin\"]\n\tPROMISOR = true\n\turl = URL\n\tuploadpack = UPLOAD\n"
                .to_owned(),
            false,
            true,
        ),
        // A nameless promisor cannot fetch: refusal only.
        (
            "nameless promisor",
            with("[remote]\n\tpromisor = true"),
            false,
            false,
        ),
    ];
    for version in ["git version 2.40.0", "git version 2.44.0"] {
        for (name, config, head, positive) in &fixtures {
            let name = format!("{version}: {name}");
            let build = |fixture: &Fixture| {
                promisor(fixture, config);
                if *head {
                    fixture.missing_head();
                } else {
                    fixture.missing_blob();
                }
            };
            let mut fixture = Fixture::changed("gated");
            build(&fixture);
            fixture.wrap(Some(version), None);
            let output = fixture.rotter(&["extract", "--worktree"], None);
            assert_eq!(output.status.code(), Some(2), "{name}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("partial clone"),
                "{name}: {output:?}"
            );
            let hook: Value = serde_json::from_slice(&fixture.claude_stop().stdout).unwrap();
            assert!(hook.get("decision").is_none(), "{name}: {hook}");
            assert!(
                hook["systemMessage"]
                    .as_str()
                    .unwrap()
                    .contains("partial clone"),
                "{name}: {hook}"
            );
            let log = fixture.wrapper_log();
            for call in ["cat-file", " diff ", "--verify", "hash-object", "ls-files"] {
                assert!(
                    !log.contains(call),
                    "{name}: {call} ran before the gate:\n{log}"
                );
            }
            assert!(!fixture.marked(), "{name}: fetched");
            drop(fixture);
            if *positive {
                let fixture = Fixture::changed("gated");
                build(&fixture);
                if *head {
                    fixture.plain_git(&["rev-parse", "HEAD^{commit}"]);
                } else {
                    fixture.plain_git(&["cat-file", "blob", MISSING]);
                }
                assert!(fixture.marked(), "{name}: positive control never fetched");
            }
        }
        // Without any of those keys the same wrapper extracts normally.
        let mut fixture = Fixture::changed("gated-plain");
        fixture.wrap(Some(version), None);
        let output = fixture.rotter(&["extract", "--worktree"], None);
        assert_eq!(output.status.code(), Some(0), "{version}: {output:?}");
        assert_eq!(paths(&report(&output)), ["a.rs"]);
    }
}

#[test]
fn git_below_the_minimum_is_refused_before_repository_discovery() {
    for version in [
        "git version 2.39.0",
        "git version 2.38.2",
        "git version 2.31.0",
        "git version abc",
    ] {
        let mut fixture = Fixture::changed("old-git");
        fixture.wrap(Some(version), None);
        let output = fixture.rotter(&["extract", "--worktree"], None);
        assert_eq!(output.status.code(), Some(2), "{version}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("2.39.1"),
            "{version}: {output:?}"
        );
        let hook: Value = serde_json::from_slice(&fixture.claude_stop().stdout).unwrap();
        assert!(hook.get("decision").is_none(), "{version}: {hook}");
        assert!(
            hook["systemMessage"].as_str().unwrap().contains("2.39.1"),
            "{version}: {hook}"
        );
        let log = fixture.wrapper_log();
        assert!(
            log.lines().all(|line| line == "version"),
            "{version}: git ran more than `git version`:\n{log}"
        );
    }
}

#[test]
fn the_hook_is_silent_without_git() {
    let fixture = Fixture::changed("no-git");
    let empty = fixture.root.join("empty-path");
    fs::create_dir_all(&empty).unwrap();
    let home = fixture.root.join("home");
    let input = serde_json::json!({ "session_id": "s-no-git", "cwd": fixture.repo }).to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(["hook", "claude-stop"])
        .current_dir(&fixture.repo)
        .env("PATH", &empty)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("ROTTER_STATE_DIR", home.join("rotter-state"))
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
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn failing_filter_listing_refuses_extraction() {
    let mut fixture = Fixture::changed("listing");
    fixture.wrap(None, Some("^(filter|hook)"));
    let output = fixture.rotter(&["extract", "--worktree"], None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing to read"),
        "{output:?}"
    );
    let hook = stdout(&fixture.claude_stop());
    assert!(hook.contains("refusing to read"), "{hook}");
    assert!(!hook.contains("decision"), "{hook}");
}

#[test]
fn a_hook_event_key_refuses_extraction() {
    let fixture = Fixture::changed("hook-event");
    fixture.append_config("[hook]\n\tevent = post-index-change\n");
    let output = fixture.rotter(&["extract", "--worktree"], None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("hook.event"),
        "{output:?}"
    );
    let hook = stdout(&fixture.claude_stop());
    assert!(
        hook.contains("hook.event") && !hook.contains("decision"),
        "{hook}"
    );
}

#[test]
fn embedded_bare_repository_is_never_used() {
    let fixture = Fixture::changed("bare");
    let bare = fixture.repo.join("vendor/evil.git");
    fixture.git(&["init", "-q", "--bare", "vendor/evil.git"]);
    // Positive control: plain git discovers the embedded bare repository.
    let found = fixture.git(&["-C", "vendor/evil.git", "rev-parse", "--absolute-git-dir"]);
    assert_eq!(fs::canonicalize(found).unwrap(), bare);
    let output = fixture.rotter(&["extract", "--worktree", "-C", "vendor/evil.git"], None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("bare repository"),
        "{output:?}"
    );
    let input = serde_json::json!({ "session_id": "b", "cwd": bare }).to_string();
    let hook = fixture.rotter(&["hook", "claude-stop"], Some(&input));
    assert_eq!(stdout(&hook), "", "{hook:?}");
}

#[test]
fn stat_only_changes_are_dropped_and_renames_kept() {
    let fixture = Fixture::changed("stat");
    fixture.write("b.rs", "// G.\nfn g() {}\n");
    fixture.write("README.md", "readme\n");
    fixture.write("c.rs", "// H.\nfn h() {}\n");
    fixture.commit_only(&["b.rs", "README.md", "c.rs"]);
    fixture.git(&["mv", "c.rs", "d.rs"]);
    touch_old(&fixture.repo.join("b.rs"));
    touch_old(&fixture.repo.join("README.md"));
    for args in [
        &["extract", "--worktree"][..],
        &["extract", "--base", "HEAD"],
    ] {
        let report = report(&fixture.rotter(args, None));
        assert_eq!(paths(&report), ["a.rs", "d.rs"], "{args:?}: {report}");
        assert_eq!(report["files"][1]["change"], "renamed");
        assert_eq!(report["files"][1]["similarity"], 100);
    }
    // Touching an unchanged file does not change the report, so it is not requested again.
    let input = serde_json::json!({ "session_id": "t", "cwd": fixture.repo }).to_string();
    let first = fixture.rotter(&["hook", "claude-stop"], Some(&input));
    assert!(
        stdout(&first).contains(r#""decision":"block""#),
        "{first:?}"
    );
    fs::File::options()
        .write(true)
        .open(fixture.repo.join("README.md"))
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    let second = fixture.rotter(&["hook", "claude-stop"], Some(&input));
    assert_eq!(stdout(&second), "", "{second:?}");
}
