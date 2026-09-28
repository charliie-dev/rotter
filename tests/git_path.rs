//! Which git rotter runs, and with what environment: PATH entries inside the repository (under
//! any spelling), untrusted or non-native candidates and dispatchers are never run; in hook mode
//! the git child gets only the allowlisted environment.

mod common;

use common::{
    calls, changed_repo, claude_input, grok_input, is_block, native_as, native_git, real_git,
    rotter, set_mode, stdout, temp,
};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

/// `entries` in front of this process's PATH.
fn path_with(entries: &[&Path]) -> OsString {
    let rest = std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect::<Vec<_>>();
    std::env::join_paths(entries.iter().map(|entry| entry.to_path_buf()).chain(rest)).unwrap()
}

/// `hook claude-stop`, `hook grok-stop` and `extract --worktree` for `cwd` with `path`.
fn every_mode(
    root: &Path,
    cwd: &Path,
    path: &OsStr,
    extra: &[(&str, Option<&OsStr>)],
) -> Vec<Output> {
    let mut env = vec![("PATH", Some(path))];
    env.extend_from_slice(extra);
    vec![
        rotter(
            root,
            &["hook", "claude-stop"],
            root,
            &claude_input("s", cwd),
            &env,
        ),
        rotter(
            root,
            &["hook", "grok-stop"],
            root,
            &grok_input("g", cwd),
            &env,
        ),
        rotter(
            root,
            &["extract", "--worktree", "-C", cwd.to_str().unwrap()],
            root,
            "",
            &env,
        ),
    ]
}

/// Every mode found the change with the real git.
fn assert_reviewed(outputs: &[Output], what: &str) {
    assert!(is_block(&outputs[0]), "{what}: {:?}", outputs[0]);
    assert!(is_block(&outputs[1]), "{what}: {:?}", outputs[1]);
    assert_eq!(
        outputs[2].status.code(),
        Some(0),
        "{what}: {:?}",
        outputs[2]
    );
    assert!(stdout(&outputs[2]).contains("a.go"), "{what}");
}

fn fresh_state(root: &Path) {
    let _ = fs::remove_dir_all(root.join("state"));
}

#[test]
fn repository_git_on_path_is_never_run_under_any_spelling() {
    let root = temp("inject");
    let repo = root.join("repo");
    changed_repo(&repo);
    let sub = repo.join("sub");
    fs::create_dir(&sub).unwrap();
    let marker = root.join("marker");
    let bin = repo.join("bin");
    native_git(&bin, &[("marker", &marker.display().to_string())]);
    let physical = fs::canonicalize(&repo).unwrap();
    // An explicit symlink to the repository, and a directory outside any repository holding a
    // symlink into it (the physical parent of that cwd holds `.git`).
    symlink(&repo, root.join("link")).unwrap();
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&sub, outside.join("into")).unwrap();
    let mut cases: Vec<(String, PathBuf, PathBuf)> = vec![
        ("subdirectory cwd".into(), sub.clone(), bin.clone()),
        (
            "resolved PATH spelling".into(),
            sub.clone(),
            physical.join("bin"),
        ),
        (
            "symlinked cwd and PATH".into(),
            root.join("link/sub"),
            root.join("link/bin"),
        ),
        (
            "symlinked cwd, resolved PATH".into(),
            root.join("link/sub"),
            physical.join("bin"),
        ),
        (
            "cwd through a link".into(),
            outside.join("into"),
            bin.clone(),
        ),
    ];
    // A case-variant spelling, on a case-insensitive volume.
    let upper = PathBuf::from(repo.display().to_string().to_uppercase()).join("BIN");
    if fs::metadata(&upper).is_ok_and(|meta| meta.ino() == fs::metadata(&bin).unwrap().ino()) {
        cases.push(("case variant".into(), sub.clone(), upper));
    } else {
        eprintln!("case-sensitive volume: no case-variant spelling");
    }
    for (name, cwd, entry) in cases {
        fresh_state(&root);
        let outputs = every_mode(&root, &cwd, &path_with(&[&entry]), &[]);
        assert!(!marker.exists(), "{name}: the repository's git ran");
        assert_reviewed(&outputs, &name);
    }
    // Positive control: the same helper outside the repository is the git rotter runs.
    let elsewhere = root.join("elsewhere");
    native_git(&elsewhere, &[("marker", &marker.display().to_string())]);
    fresh_state(&root);
    every_mode(&root, &sub, &path_with(&[&elsewhere]), &[]);
    assert!(marker.exists(), "positive control");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn untrusted_or_misnamed_candidates_are_skipped() {
    let root = temp("skip");
    let repo = root.join("repo");
    changed_repo(&repo);
    let marker = root.join("marker");
    let marking = [("marker", marker.display().to_string())];
    let marking: Vec<(&str, &str)> = marking.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // A native program not named git, reached through a link named git.
    let misnamed = root.join("misnamed");
    let other = native_as(&misnamed, "other", &marking);
    symlink(&other, misnamed.join("git")).unwrap();
    // A native git in a group-writable directory.
    let shared = root.join("shared");
    native_git(&shared, &marking);
    set_mode(&shared, 0o775);
    for (name, entry) in [("misnamed", &misnamed), ("group-writable dir", &shared)] {
        fresh_state(&root);
        let outputs = every_mode(&root, &repo, &path_with(&[entry]), &[]);
        assert!(!marker.exists(), "{name}: ran");
        assert_reviewed(&outputs, name);
    }
    // With nothing usable after them: silent hooks, CLI exit 2 naming the skipped candidate.
    fresh_state(&root);
    let only = std::env::join_paths([&misnamed, &shared]).unwrap();
    let outputs = every_mode(&root, &repo, &only, &[]);
    assert!(!marker.exists());
    assert_eq!(
        (stdout(&outputs[0]), stdout(&outputs[1])),
        (String::new(), String::new())
    );
    assert_eq!(outputs[2].status.code(), Some(2), "{:?}", outputs[2]);
    let error = String::from_utf8_lossy(&outputs[2].stderr);
    assert!(error.contains("not named git"), "{error}");
    set_mode(&shared, 0o755);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn script_dispatchers_are_skipped_before_any_probe() {
    let root = temp("dispatch");
    let repo = root.join("repo");
    changed_repo(&repo);
    let marker = root.join("marker");
    // The repository selects its own git the way asdf's `.tool-versions` does.
    let payload = repo.join("payload/bin");
    fs::create_dir_all(&payload).unwrap();
    fs::write(
        payload.join("git"),
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    set_mode(&payload.join("git"), 0o755);
    fs::write(
        repo.join(".tool-versions"),
        format!("git path:{}/payload\n", repo.display()),
    )
    .unwrap();
    // A user-owned 0755 regular file named git: a dispatcher that re-selects from the cwd.
    let shims = root.join("shims");
    fs::create_dir(&shims).unwrap();
    fs::write(
        shims.join("git"),
        "#!/usr/bin/env bash\ndir=$PWD\nwhile [ ! -f \"$dir/.tool-versions\" ] && [ \"$dir\" != / ]; do \
         dir=$(dirname \"$dir\"); done\nexec \"$(sed -n 's/^git path://p' \"$dir/.tool-versions\")/bin/git\" \"$@\"\n",
    )
    .unwrap();
    set_mode(&shims.join("git"), 0o755);
    // Positive control: run from the repository, the dispatcher runs the repository's git.
    let control = std::process::Command::new(shims.join("git"))
        .arg("version")
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(marker.exists(), "positive control: {control:?}");
    fs::remove_file(&marker).unwrap();
    let outputs = every_mode(&root, &repo, &path_with(&[&shims]), &[]);
    assert!(!marker.exists(), "the dispatcher ran");
    assert_reviewed(&outputs, "dispatcher then real git");
    fresh_state(&root);
    let outputs = every_mode(&root, &repo, shims.as_os_str(), &[]);
    assert!(!marker.exists(), "the dispatcher ran");
    assert_eq!(
        (stdout(&outputs[0]), stdout(&outputs[1])),
        (String::new(), String::new())
    );
    assert_eq!(outputs[2].status.code(), Some(2), "{:?}", outputs[2]);
    assert!(
        String::from_utf8_lossy(&outputs[2].stderr).contains("not a native executable"),
        "{:?}",
        outputs[2]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn no_git_runs_without_a_git_directory_above_the_cwd() {
    let root = temp("nogit");
    let plain = root.join("plain");
    fs::create_dir(&plain).unwrap();
    let wrapper = root.join("wrapper");
    let log = root.join("git.log");
    native_git(
        &wrapper,
        &[
            ("log", &log.display().to_string()),
            ("real", &real_git().display().to_string()),
        ],
    );
    let path = path_with(&[&wrapper]);
    let env = [("PATH", Some(path.as_os_str()))];
    for (name, input) in [
        ("claude-stop", claude_input("s", &plain)),
        ("grok-stop", grok_input("s", &plain)),
    ] {
        let output = rotter(&root, &["hook", name], &root, &input, &env);
        assert_eq!(stdout(&output), "", "{output:?}");
    }
    assert!(
        !log.exists(),
        "{}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    // A continuation the hook caused, and Grok's session-end Stop: no git either.
    let repo = root.join("repo");
    changed_repo(&repo);
    let flagged = [
        (
            "claude",
            claude_input("c", &repo)
                .replace("\"stop_hook_active\":false", "\"stop_hook_active\":true"),
        ),
        (
            "grok",
            grok_input("c", &repo).replace("\"stopHookActive\":false", "\"stopHookActive\":true"),
        ),
        (
            "grok",
            grok_input("c", &repo).replace("end_turn", "shutdown"),
        ),
    ];
    for (name, input) in flagged {
        assert!(
            input.contains("true") || input.contains("shutdown"),
            "{input}"
        );
        let output = rotter(&root, &["hook", name], &root, &input, &env);
        assert_eq!(stdout(&output), "", "{output:?}");
    }
    assert!(
        !log.exists(),
        "{}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    // Positive control: in a repository the wrapper is used.
    let output = rotter(
        &root,
        &["hook", "claude"],
        &root,
        &claude_input("s", &repo),
        &env,
    );
    assert!(is_block(&output) && log.exists(), "{output:?}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_hook_git_child_gets_only_the_allowlist() {
    let root = temp("allowlist");
    let repo = root.join("repo");
    changed_repo(&repo);
    let log = root.join("git.env");
    let helper = root.join("helper");
    native_git(
        &helper,
        &[
            ("env", &log.display().to_string()),
            ("real", &real_git().display().to_string()),
        ],
    );
    // Reached through a symlinked PATH entry: the resolved path is what gets spawned.
    let link = root.join("link");
    symlink(&helper, &link).unwrap();
    let physical = fs::canonicalize(&helper).unwrap();
    let path = path_with(&[&link]);
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
    // rotter itself starts with these, so the library must load: on macOS one already loaded.
    let insert = if cfg!(target_os = "macos") {
        "/usr/lib/libSystem.B.dylib"
    } else {
        "/nonexistent/rotter-insert.dylib"
    };
    let hostile = [
        ("LD_PRELOAD", "/nonexistent/rotter-preload.so"),
        ("DYLD_INSERT_LIBRARIES", insert),
        ("DEVELOPER_DIR", "/nonexistent/rotter-developer"),
        ("SDKROOT", "/nonexistent/rotter-sdk"),
        ("GIT_CONFIG_GLOBAL", "/nonexistent/rotter-gitconfig"),
        ("GIT_EXEC_PATH", "/nonexistent/rotter-exec-path"),
        ("XDG_CONFIG_HOME", "/nonexistent/rotter-xdg"),
    ];
    let mut env: Vec<(&str, Option<&OsStr>)> = hostile
        .iter()
        .map(|(key, value)| (*key, Some(OsStr::new(value))))
        .collect();
    env.push(("PATH", Some(path.as_os_str())));
    let home = root.join("home");
    let allowed = [
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "GIT_OPTIONAL_LOCKS",
        "GIT_NO_LAZY_FETCH",
        "ROTTER_EMPTY_VALUE",
    ];
    for (name, input) in [
        ("claude-stop", claude_input("s", &repo)),
        ("grok-stop", grok_input("g", &repo)),
    ] {
        let _ = fs::remove_file(&log);
        let output = rotter(&root, &["hook", name], &root, &input, &env);
        assert!(is_block(&output), "{name}: {output:?}");
        let calls = calls(&log);
        assert!(calls.len() > 3, "{name}: {calls:?}");
        let (mut index, mut ceiling) = (false, false);
        for call in &calls {
            let get = |key: &str| {
                call.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.as_str())
            };
            assert_eq!(
                get("exe"),
                Some(physical.join("git").to_str().unwrap()),
                "{name}"
            );
            let vars: Vec<&str> = call
                .iter()
                .map(|(key, _)| key.as_str())
                .filter(|key| !["exe", "cwd", "args", "tmpdir"].contains(key))
                .collect();
            for key in &vars {
                assert!(
                    allowed.contains(key)
                        || ["GIT_INDEX_FILE", "GIT_CEILING_DIRECTORIES"].contains(key),
                    "{name}: {key} reached git"
                );
            }
            // The version probe carries only the base; repository calls carry everything.
            let base = if get("args") == Some("version") {
                5
            } else {
                allowed.len()
            };
            for key in &allowed[..base] {
                assert!(vars.contains(key), "{name}: {key} missing from {call:?}");
            }
            index |= vars.contains(&"GIT_INDEX_FILE");
            ceiling |= vars.contains(&"GIT_CEILING_DIRECTORIES");
            assert_eq!(get("HOME"), Some(home.to_str().unwrap()));
            assert_eq!((get("LANG"), get("LC_ALL")), (Some("C"), Some("C")));
            assert_eq!(get("cwd"), Some("/"), "{name}: a neutral cwd");
            let scratch = get("TMPDIR").unwrap();
            assert!(
                Path::new(scratch).parent() == Some(tmp.as_path())
                    && scratch.rsplit('/').next().unwrap().starts_with("rotter-"),
                "{name}: TMPDIR {scratch} is not a scratch dir under {}",
                tmp.display()
            );
            let uid = fs::metadata(&root).unwrap().uid();
            assert_eq!(get("tmpdir"), Some(format!("700 {uid}").as_str()), "{name}");
            let child_path = get("PATH").unwrap();
            assert!(!child_path.contains(repo.to_str().unwrap()), "{child_path}");
            assert!(
                child_path.split(':').all(|entry| entry.starts_with('/')),
                "{child_path}"
            );
        }
        assert!(
            index && ceiling,
            "{name}: the index and ceiling calls went through the hook"
        );
    }
    // The CLI keeps its environment (user-invoked), apart from TMPDIR.
    let _ = fs::remove_file(&log);
    let cli = [
        ("PATH", Some(path.as_os_str())),
        ("ROTTER_TEST_INHERITED", Some(OsStr::new("1"))),
    ];
    let output = rotter(
        &root,
        &["extract", "--worktree", "-C", repo.to_str().unwrap()],
        &root,
        "",
        &cli,
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let calls = calls(&log);
    assert!(
        calls[0]
            .iter()
            .any(|(key, _)| key == "ROTTER_TEST_INHERITED"),
        "{:?}",
        calls[0]
    );
    assert!(calls.iter().all(|call| {
        call.iter()
            .any(|(key, value)| key == "TMPDIR" && value.contains("/rotter-"))
    }));
    fs::remove_dir_all(root).unwrap();
}

/// Apple's /usr/bin/git selects the real git through xcrun, which honours DEVELOPER_DIR.
#[cfg(target_os = "macos")]
#[test]
fn developer_dir_never_reaches_the_apple_git_stub() {
    if !Path::new("/usr/bin/git").is_file() || !Path::new("/usr/bin/xcrun").is_file() {
        eprintln!("skipped: no Apple git stub");
        return;
    }
    let root = temp("devdir");
    let repo = root.join("repo");
    changed_repo(&repo);
    let marker = root.join("marker");
    let developer = repo.join("dev");
    fs::create_dir_all(developer.join("usr/bin")).unwrap();
    for tool in ["git", "xcrun"] {
        let path = developer.join("usr/bin").join(tool);
        fs::write(
            &path,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        set_mode(&path, 0o755);
    }
    // Positive control: the stub runs the repository's tools when DEVELOPER_DIR reaches it.
    let control = std::process::Command::new("/usr/bin/git")
        .arg("version")
        .env("DEVELOPER_DIR", &developer)
        .output()
        .unwrap();
    assert!(marker.exists(), "positive control: {control:?}");
    fs::remove_file(&marker).unwrap();
    let env = [
        ("PATH", Some(OsStr::new("/usr/bin:/bin"))),
        ("DEVELOPER_DIR", Some(developer.as_os_str())),
    ];
    for (name, input) in [
        ("claude-stop", claude_input("s", &repo)),
        ("grok-stop", grok_input("g", &repo)),
    ] {
        let output = rotter(&root, &["hook", name], &root, &input, &env);
        assert!(is_block(&output), "{name}: {output:?}");
        assert!(!marker.exists(), "{name}: DEVELOPER_DIR reached git");
    }
    fs::remove_dir_all(root).unwrap();
}
