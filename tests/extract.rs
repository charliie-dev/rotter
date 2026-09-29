use rotter::json::Json;
use rotter::{Mode, Options, Report, extract};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Repo(PathBuf, Vec<(String, Arc<rotter::Grammar>, String)>);

impl Repo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rotter-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let repo = Self(path, Vec::new());
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// Runs git isolated from the user's configuration.
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.0.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "commit"]);
    }

    fn extract(&self, mode: Mode, include_untracked: bool) -> Report {
        self.extract_in(&self.0, mode, include_untracked, &[])
    }

    fn extract_in(
        &self,
        dir: &Path,
        mode: Mode,
        include_untracked: bool,
        paths: &[&str],
    ) -> Report {
        let mut options = Options::new(mode);
        options.include_untracked = include_untracked;
        options.paths = paths.iter().map(|path| path.to_string()).collect();
        options.languages = self.1.clone();
        let report = extract(dir, &options).unwrap();
        self.check_ranges(&report.json);
        report
    }

    /// Every reported text must equal the snapshot bytes at its reported range.
    fn check_ranges(&self, report: &Json) {
        for file in report.get("files").as_arr() {
            for key in ["before", "after"] {
                let side = file.get(key);
                if !matches!(side.get("status").as_str(), Some("ok" | "partial")) {
                    continue;
                }
                let blob = side.get("blob").as_str().unwrap();
                let content = if key == "after" && report.get("after").as_str() == Some("worktree")
                {
                    fs::read_to_string(self.0.join(side.get("path").as_str().unwrap())).unwrap()
                } else {
                    self.git(&["cat-file", "blob", blob])
                };
                let hashed = Command::new("git")
                    .args(["hash-object", "--no-filters", "--stdin"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|mut child| {
                        use std::io::Write;
                        child.stdin.take().unwrap().write_all(content.as_bytes())?;
                        child.wait_with_output()
                    })
                    .unwrap();
                assert_eq!(String::from_utf8(hashed.stdout).unwrap().trim(), blob);
                for unit in side.get("units").as_arr() {
                    check_text(&content, unit);
                    for comment in unit.get("comments").as_arr() {
                        check_text(&content, comment);
                    }
                }
            }
        }
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn check_text(content: &str, item: &Json) {
    let range = item.get("range");
    let bytes = range.get("bytes").as_arr();
    let lines = range.get("lines").as_arr();
    let (start, end) = (
        bytes[0].as_u64().unwrap() as usize,
        bytes[1].as_u64().unwrap() as usize,
    );
    let text = item.get("text").as_str().unwrap();
    if item.get("text_truncated").as_bool() == Some(true) {
        assert!(content[start..end].starts_with(text) && text.len() < end - start);
    } else {
        assert_eq!(&content[start..end], text);
    }
    let line_of = |offset: usize| content[..offset].matches('\n').count() + 1;
    assert_eq!(lines[0].as_u64().unwrap() as usize, line_of(start));
    let last = content[start..end].trim_end_matches('\n');
    assert_eq!(
        lines[1].as_u64().unwrap() as usize,
        line_of(start + last.len())
    );
}

fn file<'a>(report: &'a Json, path: &str) -> &'a Json {
    report
        .get("files")
        .as_arr()
        .iter()
        .find(|file| {
            file.get("new_path").as_str() == Some(path)
                || file.get("old_path").as_str() == Some(path)
        })
        .unwrap_or_else(|| panic!("no file {path} in {report}"))
}

fn unit<'a>(side: &'a Json, name: &str) -> &'a Json {
    side.get("units")
        .as_arr()
        .iter()
        .find(|unit| unit.get("name").as_str() == Some(name))
        .unwrap_or_else(|| panic!("no unit {name} in {side}"))
}

/// Returns (relation, changed, directive) for the comment with this exact text.
fn comment<'a>(unit: &'a Json, text: &str) -> (&'a str, bool, Option<&'a str>) {
    let found = unit
        .get("comments")
        .as_arr()
        .iter()
        .find(|comment| comment.get("text").as_str().map(str::trim_end) == Some(text))
        .unwrap_or_else(|| panic!("no comment {text:?} in {unit}"));
    (
        found.get("relation").as_str().unwrap(),
        found.get("changed").as_bool().unwrap(),
        found.get("directive").as_str(),
    )
}

fn has_comment(unit: &Json, text: &str) -> bool {
    unit.get("comments")
        .as_arr()
        .iter()
        .any(|comment| comment.get("text").as_str().map(str::trim_end) == Some(text))
}

#[test]
fn unchanged_leading_comment_far_from_a_code_change_is_reported() {
    let repo = Repo::new();
    let body = "\tvalue += 0\n".repeat(80);
    repo.write(
        "count.go",
        &format!("package p\n\n// Count returns one.\nfunc Count() int {{\n\tvalue := 0\n{body}\treturn value + 1\n}}\n"),
    );
    repo.commit();
    repo.write(
        "count.go",
        &format!("package p\n\n// Count returns one.\nfunc Count() int {{\n\tvalue := 0\n{body}\treturn value + 2\n}}\n"),
    );
    let report = repo.extract(Mode::Worktree, false);
    assert!(report.complete);
    let after = file(&report.json, "count.go").get("after");
    let count = unit(after, "Count");
    assert_eq!(count.get("changed_lines").as_arr(), [Json::Num(86)]);
    assert_eq!(
        comment(count, "// Count returns one."),
        ("leading", false, None)
    );
}

#[test]
fn comment_only_change_maps_to_the_documented_unit() {
    let repo = Repo::new();
    repo.write(
        "lib.rs",
        "/// Adds one.\n#[inline]\nfn add(x: i32) -> i32 {\n    x + 1\n}\n",
    );
    repo.commit();
    repo.write(
        "lib.rs",
        "/// Adds two.\n#[inline]\nfn add(x: i32) -> i32 {\n    x + 1\n}\n",
    );
    let report = repo.extract(Mode::Worktree, false);
    let after = file(&report.json, "lib.rs").get("after");
    let add = unit(after, "add");
    assert_eq!(
        add.get("range").get("lines").as_arr(),
        [Json::Num(2), Json::Num(5)]
    );
    assert_eq!(comment(add, "/// Adds two."), ("leading", true, None));
}

#[test]
fn seven_languages_relate_leading_inside_trailing_and_enclosing_comments() {
    let repo = Repo::new();
    let files = [
        (
            "a.go",
            "package p\n\n// T is a type.\ntype T struct {\n\t// A field.\n\tA int // trailing\n}\n\n// F does it.\n//\n//go:noinline\nfunc (t T) F() int {\n\t// inner\n\treturn 1\n}\n",
        ),
        (
            "a.lua",
            "local M = {}\n--- Adds.\n---@param a number\nfunction M.add(a)\n  -- inner\n  return a + 1\nend\nreturn M\n",
        ),
        (
            "a.nix",
            "{ pkgs, ... }:\n{\n  # the name\n  name = \"x\"; # trailing\n  nested = {\n    # inner\n    a = 1;\n  };\n}\n",
        ),
        (
            "a.sh",
            "#!/usr/bin/env bash\n# shellcheck disable=SC2034\n# greet prints\ngreet() {\n  # inner\n  echo hi # trailing\n}\n",
        ),
        (
            "a.yaml",
            "# top\na:\n  # before b\n  b: 1 # trailing\n  c: 2\n",
        ),
        (
            "a.toml",
            "# top\nx = 1\n# before table\n[t]\n# before k\nk = 2 # trailing\n",
        ),
        (
            "a.rs",
            "//! Module docs.\n\n/// Holder.\nstruct S {\n    /// Count.\n    n: u32, // trailing\n}\n\nimpl S {\n    /// Returns one.\n    fn one(&self) -> u32 {\n        // inner\n        1\n    }\n}\n",
        ),
    ];
    for (path, content) in files {
        repo.write(path, content);
    }
    repo.commit();
    let edits = [
        ("a.go", "return 1", "return 2"),
        ("a.go", "A int", "A int64"),
        ("a.lua", "a + 1", "a + 2"),
        ("a.nix", "\"x\"", "\"y\""),
        ("a.nix", "a = 1", "a = 2"),
        ("a.sh", "echo hi", "echo bye"),
        ("a.yaml", "b: 1", "b: 2"),
        ("a.toml", "k = 2", "k = 3"),
        ("a.rs", "n: u32", "n: u64"),
        ("a.rs", "        1\n", "        2\n"),
    ];
    for (path, from, to) in edits {
        let full = repo.0.join(path);
        let content = fs::read_to_string(&full).unwrap();
        assert!(content.contains(from), "{path}: {from}");
        fs::write(full, content.replacen(from, to, 1)).unwrap();
    }
    let report = repo.extract(Mode::Worktree, false);
    assert!(report.complete, "{}", report.json);
    let after = |path| file(&report.json, path).get("after");

    let go = after("a.go");
    let method = unit(go, "F");
    assert_eq!(
        comment(method, "// F does it.\n//"),
        ("leading", false, None)
    );
    assert_eq!(
        comment(method, "//go:noinline"),
        ("leading", false, Some("go_directive"))
    );
    assert_eq!(comment(method, "// inner"), ("inside", false, None));
    let field = unit(go, "A");
    assert_eq!(comment(field, "// A field."), ("leading", false, None));
    assert_eq!(comment(field, "// trailing"), ("trailing", true, None));
    assert_eq!(
        comment(field, "// T is a type."),
        ("enclosing_leading", false, None)
    );

    let lua = unit(after("a.lua"), "M.add");
    assert_eq!(comment(lua, "--- Adds."), ("leading", false, None));
    assert_eq!(
        comment(lua, "---@param a number"),
        ("leading", false, Some("lua_annotation"))
    );
    assert_eq!(comment(lua, "-- inner"), ("inside", false, None));

    let nix = after("a.nix");
    let name = unit(nix, "name");
    assert_eq!(comment(name, "# the name"), ("leading", false, None));
    assert_eq!(comment(name, "# trailing"), ("trailing", true, None));
    let a = unit(nix, "a");
    assert_eq!(comment(a, "# inner"), ("leading", false, None));

    let bash = unit(after("a.sh"), "greet");
    assert_eq!(
        comment(bash, "# shellcheck disable=SC2034"),
        ("leading", false, Some("shellcheck_directive"))
    );
    assert_eq!(comment(bash, "# greet prints"), ("leading", false, None));
    assert_eq!(comment(bash, "# trailing"), ("inside", true, None));
    assert!(!has_comment(bash, "#!/usr/bin/env bash"));

    let yaml = unit(after("a.yaml"), "b");
    assert_eq!(comment(yaml, "# before b"), ("leading", false, None));
    assert_eq!(comment(yaml, "# trailing"), ("trailing", true, None));
    assert_eq!(comment(yaml, "# top"), ("enclosing_leading", false, None));

    let toml = unit(after("a.toml"), "k");
    assert_eq!(comment(toml, "# before k"), ("leading", false, None));
    assert_eq!(comment(toml, "# trailing"), ("inside", true, None));
    assert_eq!(
        comment(toml, "# before table"),
        ("enclosing_leading", false, None)
    );

    let rust = after("a.rs");
    let one = unit(rust, "one");
    assert_eq!(comment(one, "/// Returns one."), ("leading", false, None));
    assert_eq!(comment(one, "// inner"), ("inside", false, None));
    assert!(!has_comment(one, "//! Module docs."));
    let n = unit(rust, "n");
    assert_eq!(comment(n, "/// Count."), ("leading", false, None));
    assert_eq!(
        comment(n, "/// Holder."),
        ("enclosing_leading", false, None)
    );
    assert_eq!(comment(n, "// trailing"), ("trailing", true, None));
}

#[test]
fn deleted_function_is_reported_on_the_before_side() {
    let repo = Repo::new();
    repo.write(
        "a.rs",
        "/// Keep.\nfn keep() {}\n\n/// Gone.\nfn gone() {}\n",
    );
    repo.commit();
    repo.write("a.rs", "/// Keep.\nfn keep() {}\n");
    let report = repo.extract(Mode::Worktree, false);
    let entry = file(&report.json, "a.rs");
    let gone = unit(entry.get("before"), "gone");
    assert_eq!(comment(gone, "/// Gone."), ("leading", true, None));
    assert_eq!(entry.get("after").get("units").as_arr(), []);
}

#[test]
fn removed_lines_inside_a_function_select_the_after_function() {
    let repo = Repo::new();
    repo.write("a.go", "package p\n\n// Sum adds a and b.\nfunc Sum(a, b int) int {\n\ts := a\n\ts += b\n\treturn s\n}\n");
    repo.commit();
    repo.write(
        "a.go",
        "package p\n\n// Sum adds a and b.\nfunc Sum(a, b int) int {\n\ts := a\n\treturn s\n}\n",
    );
    let report = repo.extract(Mode::Worktree, false);
    let sum = unit(file(&report.json, "a.go").get("after"), "Sum");
    assert_eq!(sum.get("changed_lines").as_arr(), []);
    assert_eq!(
        sum.get("gaps_between_lines").as_arr(),
        [Json::Arr(vec![Json::Num(5), Json::Num(6)])]
    );
    assert_eq!(
        comment(sum, "// Sum adds a and b."),
        ("leading", false, None)
    );
}

#[test]
fn removed_doc_line_selects_the_documented_unit_on_both_sides() {
    let repo = Repo::new();
    repo.write(
        "a.rs",
        "/// Adds.\n/// Never panics.\n/// Returns x + 1.\nfn add(x: u8) -> u8 {\n    x + 1\n}\n",
    );
    repo.commit();
    repo.write(
        "a.rs",
        "/// Adds.\n/// Returns x + 1.\nfn add(x: u8) -> u8 {\n    x + 1\n}\n",
    );
    let report = repo.extract(Mode::Worktree, false);
    let entry = file(&report.json, "a.rs");
    assert_eq!(
        comment(
            unit(entry.get("before"), "add"),
            "/// Adds.\n/// Never panics.\n/// Returns x + 1."
        ),
        ("leading", true, None)
    );
    let after = unit(entry.get("after"), "add");
    assert_eq!(
        after.get("gaps_between_lines").as_arr(),
        [Json::Arr(vec![Json::Num(1), Json::Num(2)])]
    );
    assert_eq!(
        comment(after, "/// Adds.\n/// Returns x + 1."),
        ("leading", false, None)
    );
}

#[test]
fn file_deletion_addition_and_rename_keep_paths() {
    let repo = Repo::new();
    repo.write("old.lua", "-- Old.\nlocal x = 1\nreturn x\n");
    repo.write(
        "moved name.toml",
        "# Moved.\nname = \"a\"\nother = 1\nthird = 2\n",
    );
    repo.commit();
    fs::remove_file(repo.0.join("old.lua")).unwrap();
    fs::create_dir(repo.0.join("dir")).unwrap();
    repo.git(&["mv", "moved name.toml", "dir/new näme.toml"]);
    repo.write(
        "dir/new näme.toml",
        "# Moved.\nname = \"b\"\nother = 1\nthird = 2\n",
    );
    repo.write("new.nix", "# New.\n{ a = 1; }\n");
    repo.git(&["add", "new.nix"]);
    let report = repo.extract(Mode::Worktree, false);
    assert!(report.complete);

    let old = file(&report.json, "old.lua");
    assert_eq!(old.get("change").as_str(), Some("deleted"));
    assert_eq!(old.get("after"), &Json::Null);
    assert!(has_comment(
        &old.get("before").get("units").as_arr()[0],
        "-- Old."
    ));

    let moved = file(&report.json, "dir/new näme.toml");
    assert_eq!(moved.get("change").as_str(), Some("renamed"));
    assert_eq!(moved.get("old_path").as_str(), Some("moved name.toml"));
    assert_eq!(
        moved.get("before").get("path").as_str(),
        Some("moved name.toml")
    );
    assert_eq!(
        comment(unit(moved.get("after"), "name"), "# Moved."),
        ("leading", false, None)
    );

    let new = file(&report.json, "new.nix");
    assert_eq!(new.get("change").as_str(), Some("added"));
    assert_eq!(new.get("before"), &Json::Null);
}

#[test]
fn staged_and_worktree_modes_compare_different_snapshots() {
    let repo = Repo::new();
    repo.write("a.yaml", "# A.\na: 1\n# B.\nb: 1\n");
    repo.commit();
    repo.write("a.yaml", "# A.\na: 2\n# B.\nb: 1\n");
    repo.git(&["add", "a.yaml"]);
    repo.write("a.yaml", "# A.\na: 2\n# B.\nb: 2\n");

    let staged = repo.extract(Mode::Staged, false);
    assert_eq!(staged.json.get("after").as_str(), Some("index"));
    let after = file(&staged.json, "a.yaml").get("after");
    assert_eq!(after.get("units").as_arr().len(), 1);
    unit(after, "a");

    let worktree = repo.extract(Mode::Worktree, false);
    let after = file(&worktree.json, "a.yaml").get("after");
    assert_eq!(after.get("units").as_arr().len(), 2);
    assert_eq!(comment(unit(after, "b"), "# B."), ("leading", false, None));
}

#[test]
fn repository_without_head_uses_an_empty_before_snapshot() {
    let repo = Repo::new();
    repo.write("a.sh", "#!/bin/bash\n# Hi.\nhi() { echo hi; }\n");
    repo.git(&["add", "a.sh"]);
    let report = repo.extract(Mode::Staged, false);
    assert_eq!(
        report.json.get("before").get("empty_initial").as_bool(),
        Some(true)
    );
    assert_eq!(report.json.get("before").get("commit"), &Json::Null);
    let entry = file(&report.json, "a.sh");
    assert_eq!(entry.get("change").as_str(), Some("added"));
    assert_eq!(
        comment(unit(entry.get("after"), "hi"), "# Hi."),
        ("leading", true, None)
    );
}

#[test]
fn base_mode_requires_a_resolvable_revision() {
    let repo = Repo::new();
    repo.write("a.toml", "a = 1\n");
    repo.commit();
    let first = repo.git(&["rev-parse", "HEAD"]);
    repo.write("a.toml", "a = 2\n");
    repo.commit();
    repo.write("a.toml", "a = 3\n");
    let options = |rev: &str| Options::new(Mode::Base(rev.to_owned()));
    assert!(extract(&repo.0, &options("missing")).is_err());
    assert!(extract(&repo.0, &options("--output=x")).is_err());
    let report = repo.extract(Mode::Base(first.trim().to_owned()), false);
    assert_eq!(
        report.json.get("before").get("commit").as_str(),
        Some(first.trim())
    );
    let entry = file(&report.json, "a.toml");
    assert!(
        entry.get("before").get("units").as_arr()[0]
            .get("text")
            .as_str()
            == Some("a = 1")
    );
    assert!(
        entry.get("after").get("units").as_arr()[0]
            .get("text")
            .as_str()
            == Some("a = 3")
    );
}

#[test]
fn untracked_files_are_listed_unless_explicitly_included() {
    let repo = Repo::new();
    repo.write("tracked.go", "package p\n");
    repo.write(".gitignore", "ignored.go\n");
    repo.commit();
    repo.write("new.go", "package p\n\n// F.\nfunc F() {}\n");
    repo.write("ignored.go", "package p\n");

    let excluded = repo.extract(Mode::Worktree, false);
    assert_eq!(excluded.json.get("files").as_arr(), []);
    let untracked = excluded.json.get("untracked");
    assert_eq!(untracked.get("included").as_bool(), Some(false));
    assert_eq!(
        untracked.get("not_covered").as_arr(),
        [Json::Str("new.go".into())]
    );

    let included = repo.extract(Mode::Worktree, true);
    let entry = file(&included.json, "new.go");
    assert_eq!(entry.get("change").as_str(), Some("untracked_added"));
    assert_eq!(
        comment(unit(entry.get("after"), "F"), "// F."),
        ("leading", true, None)
    );
    assert!(
        included
            .json
            .get("files")
            .as_arr()
            .iter()
            .all(|file| file.get("new_path").as_str() != Some("ignored.go"))
    );
}

#[test]
fn unicode_crlf_and_special_file_names_keep_exact_ranges() {
    let repo = Repo::new();
    repo.write(
        "dir with space/é.rs",
        "// Café ☕ returns one.\r\nfn café() -> u8 {\r\n    1\r\n}\r\n",
    );
    repo.commit();
    repo.write(
        "dir with space/é.rs",
        "// Café ☕ returns one.\r\nfn café() -> u8 {\r\n    2\r\n}\r\n",
    );
    let report = repo.extract(Mode::Worktree, false);
    assert!(report.complete);
    let cafe = unit(
        file(&report.json, "dir with space/é.rs").get("after"),
        "café",
    );
    assert_eq!(
        comment(cafe, "// Café ☕ returns one."),
        ("leading", false, None)
    );
}

#[test]
fn unreadable_inputs_make_the_report_incomplete() {
    let repo = Repo::new();
    repo.write("ok.go", "package p\n");
    repo.write("posix.sh", "#!/bin/sh\n# Prints one.\np() { echo 1; }\n");
    repo.write("z.sh", "#!/bin/zsh\necho 1\n");
    repo.write("notes.md", "# notes\n");
    repo.commit();
    repo.write("ok.go", "package p\nfunc broken( {\n");
    repo.write("posix.sh", "#!/bin/sh\n# Prints one.\np() { echo 2; }\n");
    repo.write("z.sh", "#!/bin/zsh\necho 2\n");
    repo.write("notes.md", "# changed\n");
    let report = repo.extract(Mode::Worktree, false);
    assert!(!report.complete);
    let status = |path, key| {
        file(&report.json, path)
            .get(key)
            .get("status")
            .as_str()
            .map(str::to_owned)
    };
    assert_eq!(status("ok.go", "after").as_deref(), Some("partial"));
    assert_eq!(
        status("z.sh", "after").as_deref(),
        Some("unsupported_dialect")
    );
    let posix = file(&report.json, "posix.sh").get("after");
    assert_eq!(posix.get("status").as_str(), Some("ok"));
    assert_eq!(posix.get("dialect").as_str(), Some("sh-parsed-as-bash"));
    assert_eq!(
        comment(unit(posix, "p"), "# Prints one."),
        ("leading", false, None)
    );
    assert_eq!(status("notes.md", "after").as_deref(), Some("not_in_scope"));
    assert_eq!(
        file(&report.json, "notes.md").get("complete").as_bool(),
        Some(true)
    );
}

#[test]
fn extraction_does_not_modify_the_repository() {
    let repo = Repo::new();
    repo.write("a.go", "package p\n\n// F.\nfunc F() int { return 1 }\n");
    repo.commit();
    repo.write("a.go", "package p\n\n// F.\nfunc F() int { return 2 }\n");
    repo.write("b.go", "package p\n");
    repo.git(&["add", "b.go"]);
    repo.write("c.go", "package p\n");
    let snapshot = || {
        (
            fs::read(repo.0.join(".git/index")).unwrap(),
            repo.git(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]),
            fs::read_to_string(repo.0.join("a.go")).unwrap(),
            list(&repo.0),
        )
    };
    // A stat-dirty tracked file makes `git diff <tree>` want to refresh the index.
    fs::File::options()
        .write(true)
        .open(repo.0.join("a.go"))
        .unwrap()
        .set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000),
        )
        .unwrap();
    let before = snapshot();
    for mode in [Mode::Staged, Mode::Worktree, Mode::Base("HEAD".into())] {
        let untracked = !matches!(mode, Mode::Staged);
        repo.extract(mode, untracked);
    }
    assert_eq!(before, snapshot());
}

fn list(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        if meta.is_dir() {
            found.extend(list(&entry.path()));
        } else {
            found.push((entry.path(), meta.len()));
        }
    }
    found.sort();
    found
}

#[test]
fn cli_requires_an_explicit_mode_and_reports_incomplete_status() {
    let repo = Repo::new();
    repo.write("a.go", "package p\n");
    repo.commit();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rotter"))
            .args(args)
            .current_dir(&repo.0)
            .output()
            .unwrap()
    };
    let skill = run(&["--skill"]);
    assert_eq!(skill.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(skill.stdout).unwrap(),
        include_str!("../skills/rotter-comment-review/SKILL.md")
    );
    assert_eq!(run(&["extract"]).status.code(), Some(2));
    assert_eq!(
        run(&["extract", "--staged", "--worktree"]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(&["extract", "--staged", "--include-untracked"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(run(&["extract", "--base", "nope"]).status.code(), Some(2));
    let clean = run(&["extract", "--worktree"]);
    assert_eq!(clean.status.code(), Some(0));
    assert!(
        String::from_utf8(clean.stdout)
            .unwrap()
            .contains("\"complete\": true")
    );
    repo.write("a.go", "package p\nfunc broken( {\n");
    let broken = run(&["extract", "--worktree"]);
    assert_eq!(broken.status.code(), Some(1));
    assert!(
        String::from_utf8(broken.stdout)
            .unwrap()
            .contains("\"partial\"")
    );
}

#[test]
fn changed_names_pull_in_same_file_users_with_a_cap() {
    let repo = Repo::new();
    let users: String = (0..25)
        .map(|index| format!("// U{index} runs up to 3 times.\nfunc U{index}() {{ for i := 0; i < maxAttempts; i++ {{}} }}\n\n"))
        .collect();
    repo.write(
        "a.go",
        &format!("package p\n\nconst maxAttempts = 3\n\n{users}"),
    );
    repo.commit();
    repo.write(
        "a.go",
        &format!("package p\n\nconst maxAttempts = 5\n\n{users}"),
    );
    let report = repo.extract(Mode::Worktree, false);
    let units = file(&report.json, "a.go")
        .get("after")
        .get("units")
        .as_arr();
    let changed = unit(file(&report.json, "a.go").get("after"), "maxAttempts");
    assert_eq!(changed.get("selected_by").as_str(), Some("change"));
    assert_eq!(changed.get("omitted_reference_units").as_u64(), Some(5));
    assert_eq!(units.len(), 21);
    let user = unit(file(&report.json, "a.go").get("after"), "U0");
    assert_eq!(user.get("selected_by").as_str(), Some("reference"));
    assert_eq!(user.get("referenced_name").as_str(), Some("maxAttempts"));
    assert_eq!(
        comment(user, "// U0 runs up to 3 times."),
        ("leading", false, None)
    );
}

#[test]
fn omitted_reference_units_are_counted_once_each() {
    let repo = Repo::new();
    // 20 users fill the cap; the 5 after it use the name three times each.
    let users: String = (0..25)
        .map(|index| {
            let uses = if index < 20 { 1 } else { 3 };
            let body = "_ = maxAttempts; ".repeat(uses);
            format!("// U{index} runs.\nfunc U{index}() {{ {body}}}\n\n")
        })
        .collect();
    let source = |value: u8| format!("package p\n\nconst maxAttempts = {value}\n\n{users}");
    repo.write("a.go", &source(3));
    repo.commit();
    repo.write("a.go", &source(5));
    let report = repo.extract(Mode::Worktree, false);
    let after = file(&report.json, "a.go").get("after");
    let changed = unit(after, "maxAttempts");
    assert_eq!(changed.get("omitted_reference_units").as_u64(), Some(5));
    assert_eq!(after.get("units").as_arr().len(), 21);
}

#[test]
fn long_names_still_pull_in_their_users() {
    let repo = Repo::new();
    let long = format!("maxAttempts{}", "X".repeat(80));
    let source = |value: u8| {
        format!("package p\n\nconst {long} = {value}\n\n// U runs.\nfunc U() {{ _ = {long} }}\n")
    };
    repo.write("a.go", &source(3));
    repo.commit();
    repo.write("a.go", &source(5));
    let report = repo.extract(Mode::Worktree, false);
    let after = file(&report.json, "a.go").get("after");
    let shown: String = long.chars().take(80).collect();
    assert_eq!(
        unit(after, &shown).get("selected_by").as_str(),
        Some("change")
    );
    let user = unit(after, "U");
    assert_eq!(user.get("selected_by").as_str(), Some("reference"));
    // JSON names stay at 80 characters; matching used the full one.
    assert_eq!(user.get("referenced_name").as_str(), Some(shown.as_str()));
}

#[test]
fn function_values_are_units_and_keep_their_leading_comments() {
    let repo = Repo::new();
    repo.write(
        "a.lua",
        "local M = {}\n-- M.h doc.\nM.h = function()\n  local x = 1\n  return x\nend\nreturn M\n",
    );
    repo.write(
        "a.go",
        "package p\n\n// H doc.\nvar H = func() int {\n\tvar x = 1\n\treturn x\n}\n",
    );
    repo.write(
        "a.nix",
        "{ pkgs }:\n{\n  # f doc.\n  f = x: let a = 9; in x;\n}\n",
    );
    repo.commit();
    for (path, from, to) in [
        ("a.lua", "x = 1", "x = 2"),
        ("a.go", "x = 1", "x = 2"),
        ("a.nix", "a = 9", "a = 8"),
    ] {
        let content = fs::read_to_string(repo.0.join(path)).unwrap();
        repo.write(path, &content.replacen(from, to, 1));
    }
    let report = repo.extract(Mode::Worktree, false);
    for (path, text) in [
        ("a.lua", "-- M.h doc."),
        ("a.go", "// H doc."),
        ("a.nix", "# f doc."),
    ] {
        let units = file(&report.json, path).get("after").get("units").as_arr();
        assert_eq!(units.len(), 1, "{path}");
        assert_eq!(comment(&units[0], text), ("leading", false, None), "{path}");
    }
}

#[test]
fn nul_bytes_make_the_file_incomplete() {
    let repo = Repo::new();
    repo.write("a.yaml", "# note \0 here\nk: 1\n");
    repo.commit();
    repo.write("a.yaml", "# note \0 here\nk: 2\n");
    let report = repo.extract(Mode::Worktree, false);
    assert!(!report.complete);
    let entry = file(&report.json, "a.yaml");
    assert_eq!(
        entry.get("after").get("status").as_str(),
        Some("contains_nul")
    );
}

#[test]
fn unmerged_paths_are_reported_even_without_a_diff_record() {
    for resolution in ["ours", "deleted"] {
        let repo = Repo::new();
        repo.write("a.go", "package p\n\nfunc F() int { return 1 }\n");
        repo.commit();
        repo.git(&["checkout", "-q", "-b", "other"]);
        repo.write("a.go", "package p\n\nfunc F() int { return 2 }\n");
        repo.commit();
        repo.git(&["checkout", "-q", "main"]);
        repo.write("a.go", "package p\n\nfunc F() int { return 3 }\n");
        repo.commit();
        let merge = Command::new("git")
            .args(["-C"])
            .arg(&repo.0)
            .args(["merge", "-q", "other"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(!merge.status.success());
        match resolution {
            "ours" => repo.write("a.go", &repo.git(&["show", "HEAD:a.go"])),
            _ => fs::remove_file(repo.0.join("a.go")).unwrap(),
        }
        for mode in [Mode::Worktree, Mode::Base("HEAD".into())] {
            let report = repo.extract(mode, false);
            assert!(!report.complete, "{resolution}");
            assert_eq!(
                file(&report.json, "a.go").get("change").as_str(),
                Some("unmerged")
            );
        }
    }
}

#[test]
fn syntax_errors_keep_the_units_that_parsed() {
    let repo = Repo::new();
    let script = |value: &str| {
        format!(
            "#!/usr/bin/env bash\n# Deploys to REMOTE.\ndeploy() {{\n  echo {value}\n}}\nREMOTE=\"${{REMOTE:?ERROR: $HOST not found for $ENV (run: tf --env=$ENV)}}\"\n"
        )
    };
    repo.write("deploy", &script("1"));
    repo.commit();
    repo.write("deploy", &script("2"));
    let report = repo.extract(Mode::Worktree, false);
    assert!(!report.complete);
    let after = file(&report.json, "deploy").get("after");
    assert_eq!(after.get("status").as_str(), Some("partial"));
    assert!(
        after.get("detail").as_str().unwrap().contains("line"),
        "{after}"
    );
    let deploy = unit(after, "deploy");
    assert_eq!(deploy.get("overlaps_syntax_error").as_bool(), Some(false));
    assert_eq!(
        comment(deploy, "# Deploys to REMOTE."),
        ("leading", false, None)
    );
}

#[test]
fn full_mode_reports_each_comment_once_with_its_own_unit() {
    let repo = Repo::new();
    let long_body = "    x += 1;\n".repeat(100);
    repo.write(
        "src/a.rs",
        &format!("/// Holder.\nstruct S;\n\n/// Impl docs.\nimpl S {{\n    /// One.\n    fn one(&self) -> u8 {{ 1 }} // trailing\n\n    /// Long.\n    fn long(&self) {{\n        let mut x = 0;\n{long_body}    }}\n}}\n"),
    );
    repo.write("conf/b.yaml", "# Top.\na:\n  b: 1 # trailing b\n");
    repo.write("gone.go", "package p\n// Gone.\n");
    repo.write("notes.md", "# not code\n");
    repo.commit();
    fs::remove_file(repo.0.join("gone.go")).unwrap();
    repo.write("new.lua", "-- New.\nlocal x = 1\n");

    let report = repo.extract(Mode::Full, false);
    assert!(report.complete, "{}", report.json);
    assert_eq!(report.json.get("mode").as_str(), Some("full"));
    assert_eq!(report.json.get("before"), &Json::Null);
    let paths: Vec<_> = report
        .json
        .get("files")
        .as_arr()
        .iter()
        .map(|file| file.get("new_path").as_str().unwrap())
        .collect();
    assert_eq!(paths, ["conf/b.yaml", "notes.md", "src/a.rs"]);

    let rust = file(&report.json, "src/a.rs").get("after");
    let implementation = rust
        .get("units")
        .as_arr()
        .iter()
        .find(|unit| unit.get("kind").as_str() == Some("impl_item"))
        .unwrap();
    assert_eq!(
        implementation.get("comments").as_arr().len(),
        1,
        "{implementation}"
    );
    assert_eq!(
        comment(implementation, "/// Impl docs."),
        ("leading", false, None)
    );
    let one = unit(rust, "one");
    assert_eq!(one.get("selected_by").as_str(), Some("full"));
    assert_eq!(comment(one, "/// One."), ("leading", false, None));
    assert_eq!(comment(one, "// trailing"), ("trailing", false, None));
    assert!(!has_comment(one, "/// Impl docs."));
    let long = unit(rust, "long");
    assert_eq!(long.get("text_truncated").as_bool(), Some(true));
    assert_eq!(long.get("range").get("lines").as_arr()[1], Json::Num(112));

    let yaml = file(&report.json, "conf/b.yaml").get("after");
    assert_eq!(comment(unit(yaml, "a"), "# Top."), ("leading", false, None));
    assert_eq!(
        comment(unit(yaml, "b"), "# trailing b"),
        ("trailing", false, None)
    );

    let with_untracked = repo.extract(Mode::Full, true);
    file(&with_untracked.json, "new.lua");
}

#[test]
fn pathspecs_limit_every_mode_relative_to_the_working_directory() {
    let repo = Repo::new();
    repo.write(
        "a/one.go",
        "package a\n\n// One.\nfunc One() int { return 1 }\n",
    );
    repo.write(
        "b/two.go",
        "package b\n\n// Two.\nfunc Two() int { return 2 }\n",
    );
    repo.commit();
    repo.write(
        "a/one.go",
        "package a\n\n// One.\nfunc One() int { return 3 }\n",
    );
    repo.write(
        "b/two.go",
        "package b\n\n// Two.\nfunc Two() int { return 4 }\n",
    );
    let paths = |report: &Report| -> Vec<String> {
        report
            .json
            .get("files")
            .as_arr()
            .iter()
            .map(|file| file.get("new_path").as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        paths(&repo.extract_in(&repo.0, Mode::Worktree, false, &["a"])),
        ["a/one.go"]
    );
    let from_b = repo.0.join("b");
    assert_eq!(
        paths(&repo.extract_in(&from_b, Mode::Worktree, false, &["two.go"])),
        ["b/two.go"]
    );
    assert_eq!(
        paths(&repo.extract_in(&from_b, Mode::Worktree, false, &[])).len(),
        2
    );
    assert_eq!(
        paths(&repo.extract_in(&from_b, Mode::Full, false, &["."])),
        ["b/two.go"]
    );
}

#[test]
fn language_overrides_cover_helpers_without_a_shebang() {
    let mut repo = Repo::new();
    repo.write("tasks/lib/helper", "# Prints one.\nhelper() { echo 1; }\n");
    repo.write("conf/x.txt", "# Port.\nport: 1\n");
    repo.commit();
    repo.write("tasks/lib/helper", "# Prints one.\nhelper() { echo 2; }\n");
    repo.write("conf/x.txt", "# Port.\nport: 2\n");
    let report = repo.extract(Mode::Worktree, false);
    let status = |report: &Report, path| {
        file(&report.json, path)
            .get("after")
            .get("status")
            .as_str()
            .map(str::to_owned)
    };
    assert_eq!(
        status(&report, "tasks/lib/helper").as_deref(),
        Some("not_in_scope")
    );

    for (pattern, name) in [("**/lib/*", "bash"), ("conf/*.txt", "yaml")] {
        let (language, dialect) = rotter::Languages::default().by_name(name).unwrap();
        repo.1.push((pattern.to_owned(), language, dialect));
    }
    let report = repo.extract(Mode::Worktree, false);
    assert!(report.complete, "{}", report.json);
    let helper = file(&report.json, "tasks/lib/helper").get("after");
    assert_eq!(helper.get("dialect").as_str(), Some("bash-by-override"));
    assert_eq!(
        comment(unit(helper, "helper"), "# Prints one."),
        ("leading", false, None)
    );
    let yaml = file(&report.json, "conf/x.txt").get("after");
    assert_eq!(
        comment(unit(yaml, "port"), "# Port."),
        ("leading", false, None)
    );
}

/// Runs the CLI in `repo.0` with TMPDIR = `tmp` and every user directory pinned under `home`.
fn cli_with_tmp(repo: &Repo, home: &Path, tmp: &Path, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotter"))
        .args(args)
        .current_dir(&repo.0)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("CODEX_HOME", home.join("codex"))
        .env("COPILOT_HOME", home.join("copilot"))
        .env("ROTTER_STATE_DIR", home.join("rotter-state"))
        .env("TMPDIR", tmp)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "rotter hangs"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// A fresh 0700 directory next to the test repositories.
fn private_temp(name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!(
        "rotter-test-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    fs::canonicalize(path).unwrap()
}

#[test]
fn multi_file_worktree_and_staged_diffs_clean_up_their_scratch() {
    let repo = Repo::new();
    let names = ["a.go", "b.go", "c.go"];
    let body = |name: &str, value: u32| {
        format!(
            "package p\n\n// F{name} returns a value.\nfunc F{}() int {{ return {value} }}\n",
            &name[..1]
        )
    };
    for name in names {
        repo.write(name, &body(name, 1));
    }
    repo.commit();
    for name in names {
        repo.write(name, &body(name, 2));
    }
    repo.git(&["add", "-A"]);
    let home = private_temp("scratch-home");
    let tmp = private_temp("scratch-tmp");
    for mode in ["--worktree", "--staged"] {
        let output = cli_with_tmp(&repo, &home, &tmp, &["extract", mode]);
        assert_eq!(output.status.code(), Some(0), "{mode}: {output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let files = report["files"].as_array().unwrap();
        assert_eq!(files.len(), 3, "{mode}: {report}");
        for file in files {
            assert_eq!(file["hunks"].as_array().unwrap().len(), 1, "{file}");
            assert_eq!(
                file["after"]["units"].as_array().unwrap().len(),
                1,
                "{file}"
            );
        }
        assert_eq!(
            fs::read_dir(&tmp).unwrap().count(),
            0,
            "{mode}: scratch and every diff-<n> removed"
        );
    }
    fs::remove_dir_all(home).unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn unsafe_temp_root_is_refused_before_any_write() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    repo.write("a.go", "package p\n\n// F.\nfunc F() int { return 1 }\n");
    repo.commit();
    repo.write("a.go", "package p\n\n// F.\nfunc F() int { return 2 }\n");
    let home = private_temp("unsafe-home");
    let outside = private_temp("unsafe-outside");
    let canary = outside.join("canary");
    fs::write(&canary, "canary").unwrap();
    let shared = outside.join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
    let index = fs::read(repo.0.join(".git/index")).unwrap();

    let output = cli_with_tmp(&repo, &home, &shared, &["extract", "--worktree"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TMPDIR") && stderr.contains("refused"),
        "{stderr}"
    );
    assert_eq!(fs::read_dir(&shared).unwrap().count(), 0, "nothing created");
    assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index);

    // The hook reports it as one systemMessage per session and does not block.
    let run_hook = |session: &str| {
        let mut hook = Command::new(env!("CARGO_BIN_EXE_rotter"))
            .args(["hook", "claude-stop"])
            .current_dir(&home)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("CODEX_HOME", home.join("codex"))
            .env("COPILOT_HOME", home.join("copilot"))
            .env("ROTTER_STATE_DIR", home.join("rotter-state"))
            .env("TMPDIR", &shared)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            let input = format!(
                r#"{{"session_id": "{session}", "cwd": "{}"}}"#,
                repo.0.display()
            );
            hook.stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        let hook = hook.wait_with_output().unwrap();
        assert!(hook.status.success());
        hook.stdout
    };
    let first = run_hook("s");
    let value: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert!(value.get("decision").is_none(), "{value}");
    assert!(
        value["systemMessage"].as_str().unwrap().contains("TMPDIR"),
        "{value}"
    );
    assert!(
        run_hook("s").is_empty(),
        "same failure is not repeated in a session"
    );
    assert!(!run_hook("t").is_empty(), "a new session is told again");
    assert_eq!(fs::read_dir(&shared).unwrap().count(), 0);
    assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");

    // A temp root whose resolved path contains ':' cannot be a ceiling directory.
    let colon = outside.join("a:b");
    fs::create_dir(&colon).unwrap();
    let output = cli_with_tmp(&repo, &home, &colon, &["extract", "--worktree"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("':'"));
    fs::remove_dir_all(home).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn fifo_index_fails_fast() {
    let repo = Repo::new();
    repo.write("a.go", "package p\n");
    repo.commit();
    let index = repo.0.join(".git/index");
    fs::remove_file(&index).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&index)
            .status()
            .unwrap()
            .success()
    );
    let home = private_temp("fifo-home");
    let tmp = private_temp("fifo-tmp");
    let output = cli_with_tmp(&repo, &home, &tmp, &["extract", "--worktree"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a regular file"),
        "{output:?}"
    );
    assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0);
    fs::remove_file(&index).unwrap();
    fs::remove_dir_all(home).unwrap();
    fs::remove_dir_all(tmp).unwrap();
}
