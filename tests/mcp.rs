mod common;

use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn request(id: i64, method: &str, params: Value) -> String {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
    });
    format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    )
}

#[test]
fn mcp_serves_extract_and_the_skill_until_stdin_closes() {
    let root = common::temp("mcp");
    let repo = root.join("repo");
    common::changed_repo(&repo);
    let input = [
        request(1, "server/discover", json!({})),
        request(
            2,
            "tools/call",
            json!({ "name": "extract", "arguments": {
                "mode": "worktree", "directory": repo.display().to_string() } }),
        ),
        request(
            3,
            "tools/call",
            json!({ "name": "extract", "arguments": {
                "mode": "worktree", "directory": root.join("nowhere").display().to_string() } }),
        ),
        request(4, "prompts/get", json!({ "name": "rotter-comment-review" })),
    ]
    .concat();
    let output = common::rotter(&root, &["mcp"], &root, &input, &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let responses: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 4, "{stdout}");

    assert_eq!(
        responses[0]["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );
    assert_eq!(
        responses[0]["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let call = &responses[1]["result"];
    assert_eq!(call["resultType"], "complete");
    assert!(call.get("isError").is_none(), "{call}");
    let report = &call["structuredContent"];
    assert_eq!(report["schema"], "rotter.extract.poc/0");
    assert_eq!(report["mode"], "worktree");
    assert_eq!(report["files"][0]["new_path"], "a.go");
    assert_eq!(
        serde_json::from_str::<Value>(call["content"][0]["text"].as_str().unwrap()).unwrap(),
        *report,
        "the text content is the same report"
    );

    let failed = &responses[2]["result"];
    assert_eq!(failed["isError"], true, "{failed}");
    assert!(!failed["content"][0]["text"].as_str().unwrap().is_empty());

    let prompt = responses[3]["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    let skill = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/skills/rotter-comment-review/SKILL.md"
    ))
    .unwrap();
    assert!(
        prompt.starts_with("You reach rotter through MCP"),
        "{prompt}"
    );
    assert!(
        prompt.ends_with(&skill),
        "the prompt ends with the whole skill"
    );
    fs::remove_dir_all(&root).unwrap();
}

fn call(id: i64, directory: &Path) -> String {
    request(
        id,
        "tools/call",
        json!({ "name": "extract", "arguments": {
            "mode": "worktree", "directory": directory.display().to_string() } }),
    )
}

fn results(output: std::process::Output) -> Vec<Value> {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["result"].clone())
        .collect()
}

fn refused(result: &Value) -> bool {
    result["isError"] == true
        && result["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("the roots this server may read"))
}

#[test]
fn mcp_reads_only_repositories_under_its_roots() {
    let root = common::temp("mcp-roots");
    let allowed = root.join("allowed");
    let other = root.join("other");
    common::changed_repo(&allowed);
    common::changed_repo(&other);
    let pointer = allowed.join("pointer");
    fs::create_dir_all(&pointer).unwrap();
    fs::write(
        pointer.join(".git"),
        format!("gitdir: {}\n", other.join(".git").display()),
    )
    .unwrap();
    std::os::unix::fs::symlink(&other, allowed.join("link")).unwrap();
    // A worktree of the other repository, inside the root: git registered it there.
    common::git(
        &other,
        &[
            "worktree",
            "add",
            "-q",
            &allowed.join("tree").display().to_string(),
        ],
    );
    // The same with a relative back link (git 2.48 and later; older git writes an absolute one).
    common::git(
        &other,
        &[
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            "-q",
            &allowed.join("relative").display().to_string(),
        ],
    );
    // A `.git` file pointing at the worktree entry git registered for `tree`, not for itself.
    let borrowed = allowed.join("borrowed");
    fs::create_dir_all(&borrowed).unwrap();
    fs::write(
        borrowed.join(".git"),
        format!("gitdir: {}\n", other.join(".git/worktrees/tree").display()),
    )
    .unwrap();
    // A `.git` symlink to the `.git` file of a worktree outside the root: that worktree's entry
    // points back at the worktree outside, not at this directory.
    let outside_tree = root.join("outside-tree");
    common::git(
        &other,
        &["worktree", "add", "-q", &outside_tree.display().to_string()],
    );
    let linked = allowed.join("linked");
    fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(outside_tree.join(".git"), linked.join(".git")).unwrap();
    // A worktree registered for this very directory whose `.git` file was swapped for a symlink
    // to the same content: git wrote a file, so this is not what it registered.
    let swapped = allowed.join("swapped");
    common::git(
        &other,
        &["worktree", "add", "-q", &swapped.display().to_string()],
    );
    fs::rename(swapped.join(".git"), swapped.join("git-file")).unwrap();
    std::os::unix::fs::symlink(swapped.join("git-file"), swapped.join(".git")).unwrap();
    // An ordinary repository inside the root whose `.git/commondir` points at the other one: git
    // would read the other repository's refs and objects.
    let forged = allowed.join("forged");
    fs::create_dir_all(&forged).unwrap();
    common::git(&forged, &["init", "-q", "-b", "main"]);
    fs::write(
        forged.join(".git/commondir"),
        format!("{}\n", other.join(".git").display()),
    )
    .unwrap();
    let inner = allowed.join("inner");
    common::git(
        &allowed,
        &["worktree", "add", "-q", &inner.display().to_string()],
    );
    // A worktree entry written by hand inside the root, pointing back at its `.git` file but with
    // a `commondir` outside: no repository keeps it under its `worktrees/`.
    let bypass = allowed.join("bypass");
    let entry = allowed.join("entry");
    fs::create_dir_all(&bypass).unwrap();
    fs::create_dir_all(&entry).unwrap();
    fs::write(
        bypass.join(".git"),
        format!("gitdir: {}\n", entry.display()),
    )
    .unwrap();
    fs::write(
        entry.join("gitdir"),
        format!("{}\n", bypass.join(".git").display()),
    )
    .unwrap();
    fs::write(
        entry.join("commondir"),
        format!("{}\n", other.join(".git").display()),
    )
    .unwrap();
    for file in ["HEAD", "index"] {
        fs::copy(other.join(".git").join(file), entry.join(file)).unwrap();
    }
    let input = [
        call(1, &allowed),
        call(2, &other),
        call(3, &pointer),
        call(4, &allowed.join("link")),
        call(5, &allowed.join("tree")),
        call(6, &borrowed),
        call(7, &allowed.join("relative")),
        call(8, &linked),
        call(9, &swapped),
        call(10, &forged),
        call(11, &inner),
        call(12, &bypass),
    ]
    .concat();
    let allowed_arg = allowed.display().to_string();
    let results = results(common::rotter(
        &root,
        &["mcp", "--root", &allowed_arg],
        &root,
        &input,
        &[],
    ));
    assert!(results[0].get("isError").is_none(), "{}", results[0]);
    for (index, what) in [(1, "a sibling"), (2, "a .git file"), (3, "a symlink")] {
        assert!(refused(&results[index]), "{what}: {}", results[index]);
    }
    assert!(
        results[4].get("isError").is_none(),
        "a registered linked worktree: {}",
        results[4]
    );
    assert!(
        refused(&results[5]),
        "another worktree's entry: {}",
        results[5]
    );
    assert!(
        results[6].get("isError").is_none(),
        "a worktree registered with a relative path: {}",
        results[6]
    );
    assert!(
        refused(&results[7]),
        "a .git symlink to another worktree's .git: {}",
        results[7]
    );
    assert!(
        refused(&results[8]),
        "a registered worktree whose .git became a symlink: {}",
        results[8]
    );
    assert!(
        refused(&results[9]),
        "a commondir pointing outside the roots: {}",
        results[9]
    );
    assert!(
        results[10].get("isError").is_none(),
        "a worktree of a repository inside the root: {}",
        results[10]
    );
    assert!(
        refused(&results[11]),
        "a worktree entry written by hand with a commondir outside: {}",
        results[11]
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn mcp_default_root_admits_a_separate_git_directory() {
    let root = common::temp("mcp-separate");
    let work = root.join("work");
    let gitdir = root.join("gitdir");
    common::git(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            &gitdir.display().to_string(),
            &work.display().to_string(),
        ],
    );
    fs::write(work.join("a.go"), common::go(1)).unwrap();
    common::git(&work, &["add", "-A"]);
    common::git(&work, &["commit", "-q", "-m", "c"]);
    let results = results(common::rotter(&root, &["mcp"], &work, &call(1, &work), &[]));
    assert!(results[0].get("isError").is_none(), "{}", results[0]);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn mcp_defaults_to_the_repository_it_starts_in_and_returns_cli_notes() {
    let root = common::temp("mcp-default");
    let repo = root.join("repo");
    let other = root.join("other");
    common::changed_repo(&repo);
    common::changed_repo(&other);
    let sub = repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let input = [call(1, &repo), call(2, &other)].concat();
    let no_config = [
        ("XDG_CONFIG_HOME", Some(std::ffi::OsStr::new("relative"))),
        ("HOME", None),
    ];
    let results = results(common::rotter(&root, &["mcp"], &sub, &input, &no_config));
    assert!(results[0].get("isError").is_none(), "{}", results[0]);
    assert!(refused(&results[1]), "{}", results[1]);
    let note = results[0]["content"][1]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        note.contains("external languages disabled"),
        "{}",
        results[0]
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn mcp_help_prints_the_usage() {
    let root = common::temp("mcp-help");
    let output = common::rotter(&root, &["mcp", "--help"], &root, "", &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("rotter mcp [--root <dir>]..."), "{stdout}");
    fs::remove_dir_all(&root).unwrap();
}

/// A cancellation reaches the extract that is running, not only its response: the files it has
/// not started are skipped. Each file costs one `git hash-object`, so fewer of them than files
/// shows the extract stopped early.
#[test]
fn mcp_cancellation_stops_a_running_extract() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let root = common::temp("mcp-cancel");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let files = 300;
    for index in 0..files {
        fs::write(repo.join(format!("f{index}.go")), common::go(index)).unwrap();
    }
    common::git(&repo, &["init", "-q", "-b", "main"]);
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-q", "-m", "c"]);
    let wrapper = root.join("wrapper");
    let log = root.join("git.log");
    common::native_git(
        &wrapper,
        &[
            ("log", &log.display().to_string()),
            ("real", &common::real_git().display().to_string()),
        ],
    );
    let hashed = || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("hash-object --no-filters --stdin"))
            .count()
    };
    let path = format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap());
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(["mcp", "--root", &repo.display().to_string()])
        .current_dir(&root)
        .env("PATH", path)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_CACHE_HOME", root.join("xdg-cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let extract = json!({ "name": "extract",
        "arguments": { "mode": "full", "directory": repo.display().to_string() } });
    write!(stdin, "{}", request(1, "tools/call", extract)).unwrap();
    let start = Instant::now();
    while hashed() == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the extract never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": 1 } });
    writeln!(stdin, "{cancel}").unwrap();
    write!(stdin, "{}", request(2, "tools/list", json!({}))).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let ids: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].clone())
        .collect();
    assert_eq!(ids, [json!(2)], "the cancelled request got no response");
    assert!(
        hashed() < files,
        "the extract stopped early: {} of {files}",
        hashed()
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn mcp_does_not_wait_on_a_fifo_worktree_entry() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let root = common::temp("mcp-fifo");
    let allowed = root.join("allowed");
    let common_dir = allowed.join("fake.git");
    fs::create_dir_all(&allowed).unwrap();
    common::git(
        &allowed,
        &["init", "-q", "--bare", "-b", "main", "fake.git"],
    );
    let entry = common_dir.join("worktrees/e");
    fs::create_dir_all(&entry).unwrap();
    fs::write(entry.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(entry.join("commondir"), "../..\n").unwrap();
    let status = Command::new("mkfifo")
        .arg(entry.join("gitdir"))
        .status()
        .unwrap();
    assert!(status.success());
    let worktree = allowed.join("wt");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", entry.display()),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(["mcp", "--root", &allowed.display().to_string()])
        .current_dir(&root)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_CACHE_HOME", root.join("xdg-cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    write!(stdin, "{}", call(1, &worktree)).unwrap();
    drop(stdin);
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            panic!("the server waited on the FIFO");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let lines = String::from_utf8(output.stdout).unwrap();
    assert_eq!(lines.lines().count(), 1, "one response: {lines}");
    fs::remove_dir_all(&root).unwrap();
}
