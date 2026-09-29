//! `rotter parser install` and the loader, offline: the vendored tree-sitter-lua 0.5.0 sources
//! serve as a local `path` grammar (lua2); git grammars are exercised through wrapper scripts.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
const EXT: &str = "dylib";
#[cfg(not(target_os = "macos"))]
const EXT: &str = "so";

const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

/// A fresh canonical 0700 directory.
fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rotter-inst-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    fs::canonicalize(path).unwrap()
}

/// The vendored tree-sitter-lua 0.5.0 `src` directory, found without network access.
fn lua_src() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--offline", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let package = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "tree-sitter-lua" && package["version"] == "0.5.0")
        .expect("tree-sitter-lua 0.5.0 in cargo metadata");
    let src = Path::new(package["manifest_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .join("src");
    for file in ["parser.c", "scanner.c", "tree_sitter/parser.h"] {
        assert!(src.join(file).is_file(), "{file}");
    }
    src
}

/// A user-owned copy of the lua sources: 0644 files, 0700 directories.
fn copy_lua(dest: &Path) {
    let src = lua_src();
    fs::create_dir_all(dest.join("tree_sitter")).unwrap();
    for dir in [dest.to_owned(), dest.join("tree_sitter")] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for name in ["parser.c", "scanner.c"] {
        fs::copy(src.join(name), dest.join(name)).unwrap();
    }
    for entry in fs::read_dir(src.join("tree_sitter")).unwrap() {
        let entry = entry.unwrap();
        fs::copy(
            entry.path(),
            dest.join("tree_sitter").join(entry.file_name()),
        )
        .unwrap();
    }
    for entry in fs::read_dir(dest)
        .unwrap()
        .chain(fs::read_dir(dest.join("tree_sitter")).unwrap())
    {
        let path = entry.unwrap().path();
        if path.is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }
}

fn lua2(path: &Path) -> String {
    format!(
        "[language.lua2]\npath = \"{}\"\nsymbol = \"tree_sitter_lua\"\nextensions = [\"lua2\"]\n\
         units = [\"function_declaration\", \"variable_declaration\", \"assignment_statement\", \"field\"]\n\
         functions = [\"function_declaration\"]\nfunction_values = [\"function_definition\"]\n",
        path.display()
    )
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    for (path, content) in files {
        fs::write(repo.join(path), content).unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    repo
}

/// A test world: pinned config, cache, HOME and a cleared environment for every run.
struct World {
    root: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    env: BTreeMap<String, OsString>,
}

impl World {
    fn new(name: &str) -> Self {
        let root = temp(name);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let config = root.join("config");
        let mut env = BTreeMap::new();
        env.insert("PATH".to_owned(), std::env::var_os("PATH").unwrap());
        env.insert("HOME".to_owned(), home.into_os_string());
        env.insert("TMPDIR".to_owned(), std::env::temp_dir().into_os_string());
        env.insert(
            "XDG_CONFIG_HOME".to_owned(),
            config.clone().into_os_string(),
        );
        env.insert(
            "XDG_CACHE_HOME".to_owned(),
            root.join("cache").into_os_string(),
        );
        env.insert("GROK_HOME".to_owned(), root.join("grok").into_os_string());
        Self {
            cache: root.join("cache"),
            root,
            config,
            env,
        }
    }

    fn set(&mut self, key: &str, value: impl Into<OsString>) {
        self.env.insert(key.to_owned(), value.into());
    }

    fn write_config(&self, text: &str) {
        fs::create_dir_all(self.config.join("rotter")).unwrap();
        let path = self.config.join("rotter/config.toml");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rotter"));
        command
            .args(args)
            .env_clear()
            .envs(&self.env)
            .current_dir(&self.root);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// `rotter parser install` under `umask`.
    fn install_with_umask(&self, umask: &str, names: &[&str]) -> Output {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!(
                "umask {umask} && exec \"$0\" parser install \"$@\""
            ))
            .arg(env!("CARGO_BIN_EXE_rotter"))
            .args(names)
            .env_clear()
            .envs(&self.env)
            .current_dir(&self.root);
        command.output().unwrap()
    }

    fn install(&self, names: &[&str]) -> Output {
        let mut args = vec!["parser", "install"];
        args.extend(names);
        self.run(&args)
    }

    fn parsers(&self) -> PathBuf {
        fs::canonicalize(&self.cache)
            .unwrap()
            .join("rotter/parsers")
    }

    fn listing(&self, dir: &str) -> Vec<String> {
        let dir = fs::canonicalize(&self.cache)
            .unwrap()
            .join("rotter")
            .join(dir);
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// `extract --full` on `repo`: the after side of `path`.
    fn side(&self, repo: &Path, path: &str, extra: &[&str]) -> Value {
        let mut args = vec!["extract", "--full", "-C", repo.to_str().unwrap()];
        args.extend(extra);
        let output = self.run(&args);
        let report: Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"));
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["new_path"] == path)
            .unwrap_or_else(|| panic!("{path} in {report}"))["after"]
            .clone()
    }

    fn status(&self, repo: &Path, path: &str) -> String {
        self.side(repo, path, &[])["status"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The one JSON document on stdout, checked to be rotter's with `schema`.
fn document(output: &Output, schema: &str) -> Value {
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {output:?}"));
    assert_eq!(value["tool"], "rotter", "{value}");
    assert_eq!(value["schema"], schema, "{value}");
    value
}

/// Grammar `name` in a `rotter parser list` document.
fn listed<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["parsers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parser| parser["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {list}"))
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// `name` resolved from this test process's PATH.
fn real(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap()
}

/// PATH with `dir` first.
fn path_with(dir: &Path) -> OsString {
    let mut entries = vec![dir.to_owned()];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(entries).unwrap()
}

/// FNV-1a 64 over `location \0 symbol`, first 8 hex digits: the documented git key.
fn git_file(name: &str, location: &str, symbol: &str) -> String {
    let hash = [location.as_bytes(), b"\0", symbol.as_bytes()]
        .concat()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{name}-{REVISION}-{:08x}.{EXT}", hash >> 32)
}

const LUA_FILE: &str = "-- Adds one.\nlocal function inc(x) return x + 1 end\n";

#[test]
fn offline_lua2_install_then_extract() {
    let mut world = World::new("lua2");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    let repo = repo(
        &world.root,
        &[("a.lua2", LUA_FILE), ("b.txt", "-- B.\nlocal b = 1\n")],
    );
    // Missing cache base two levels deep; TMPDIR unwritable.
    world.cache = world.root.join("fresh/cache");
    world.set("XDG_CACHE_HOME", world.cache.clone());
    let readonly = world.root.join("readonly-tmp");
    fs::create_dir_all(&readonly).unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o500)).unwrap();

    for (path, extra) in [("a.lua2", vec![]), ("b.txt", vec!["--lang", "b.txt=lua2"])] {
        let side = world.side(&repo, path, &extra);
        assert_eq!(side["status"], "parser_not_installed", "{side}");
    }
    assert!(!world.cache.exists(), "extract creates no cache");

    world.set("TMPDIR", readonly.clone());
    let output = world.install_with_umask("002", &["lua2"]);
    ok(&output);
    let installed = document(&output, "rotter.parser_install/1");
    let notes = installed["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "{installed}");
    assert!(
        notes[0].as_str().unwrap().contains("in-process"),
        "trust statement: {installed}"
    );
    world.set("TMPDIR", std::env::temp_dir());
    let fresh = world.root.join("fresh");
    for dir in [
        fresh.clone(),
        world.cache.clone(),
        world.cache.join("rotter"),
        world.cache.join("rotter/parsers"),
        world.cache.join("rotter/build"),
    ] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    let libraries = world.listing("parsers");
    assert_eq!(libraries.len(), 1, "{libraries:?}");
    assert!(libraries[0].starts_with("lua2-") && libraries[0].ends_with(EXT));
    assert_eq!(mode(&world.parsers().join(&libraries[0])), 0o700);
    assert!(world.listing("build").is_empty(), "staging removed");

    let side = world.side(&repo, "a.lua2", &[]);
    assert_eq!(side["status"], "ok", "{side}");
    let unit = &side["units"][0];
    assert_eq!(unit["kind"], "function_declaration");
    assert_eq!(unit["comments"][0]["relation"], "leading");
    assert_eq!(unit["comments"][0]["text"], "-- Adds one.");
    let side = world.side(&repo, "b.txt", &["--lang", "b.txt=lua2"]);
    assert_eq!(side["units"][0]["comments"][0]["text"], "-- B.", "{side}");
    assert_eq!(
        installed["parsers"],
        serde_json::json!([{ "name": "lua2", "result": "installed",
            "path": world.parsers().join(&libraries[0]).display().to_string(),
            "detail": null }])
    );
    let output = world.run(&["parser", "list"]);
    ok(&output);
    let list = document(&output, "rotter.parsers/1");
    assert_eq!(
        *listed(&list, "lua2"),
        serde_json::json!({ "name": "lua2", "enabled": true, "installed": true,
            "path": world.parsers().join(&libraries[0]).display().to_string(),
            "source": src.display().to_string(), "detail": null })
    );
    let python = listed(&list, "python");
    assert_eq!(
        (&python["enabled"], &python["installed"]),
        (&false.into(), &false.into())
    );
}

#[test]
fn loader_refuses_unsafe_cache_layouts() {
    let mut world = World::new("loader");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    let repo = repo(&world.root, &[("a.lua2", LUA_FILE)]);
    let shared = world.root.join("shared");
    fs::create_dir_all(&shared).unwrap();
    world.cache = shared.join("cache");
    world.set("XDG_CACHE_HOME", world.cache.clone());
    ok(&world.install(&["lua2"]));
    assert_eq!(world.status(&repo, "a.lua2"), "ok");
    let parsers = world.parsers();
    let library = parsers.join(&world.listing("parsers")[0]);
    let refused = |world: &World, what: &str| {
        let side = world.side(&repo, "a.lua2", &[]);
        assert_eq!(side["status"], "parser_not_installed", "{what}: {side}");
        assert!(
            side["detail"]
                .as_str()
                .unwrap()
                .contains("rotter parser install lua2"),
            "{side}"
        );
    };

    fs::set_permissions(&library, fs::Permissions::from_mode(0o720)).unwrap();
    refused(&world, "group-writable library");
    fs::set_permissions(&library, fs::Permissions::from_mode(0o700)).unwrap();

    let elsewhere = world.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::rename(&library, elsewhere.join("lib")).unwrap();
    symlink(elsewhere.join("lib"), &library).unwrap();
    refused(&world, "symlinked library");
    fs::remove_file(&library).unwrap();
    fs::rename(elsewhere.join("lib"), &library).unwrap();
    assert_eq!(world.status(&repo, "a.lua2"), "ok");

    let moved = parsers.with_file_name("parsers.real");
    fs::rename(&parsers, &moved).unwrap();
    symlink(&moved, &parsers).unwrap();
    refused(&world, "symlinked parsers/");
    fs::remove_file(&parsers).unwrap();
    fs::rename(&moved, &parsers).unwrap();

    fs::set_permissions(&parsers, fs::Permissions::from_mode(0o770)).unwrap();
    refused(&world, "group-writable parsers/");
    fs::set_permissions(&parsers, fs::Permissions::from_mode(0o700)).unwrap();

    fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
    refused(&world, "group-writable non-sticky ancestor");
    // A symlinked cache base: into the unsafe directory, then with the target chain safe.
    let link = world.root.join("cache-link");
    symlink(&world.cache, &link).unwrap();
    world.set("XDG_CACHE_HOME", link.clone());
    refused(&world, "symlink into an unsafe directory");
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        world.status(&repo, "a.lua2"),
        "ok",
        "symlink with a safe chain"
    );

    let wrong = parsers.join(format!("lua2-0000000000000000.{EXT}"));
    fs::rename(&library, &wrong).unwrap();
    refused(&world, "wrong key");
}

#[test]
fn cache_inside_the_analysed_repository_is_not_loaded() {
    let mut world = World::new("repocache");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    let repo = repo(&world.root, &[("a.lua2", LUA_FILE)]);
    world.cache = repo.join(".cache");
    world.set("XDG_CACHE_HOME", world.cache.clone());
    ok(&world.install(&["lua2"]));
    assert_eq!(world.listing("parsers").len(), 1);
    let side = world.side(&repo, "a.lua2", &[]);
    assert_eq!(side["status"], "parser_not_installed", "{side}");
    assert!(
        side["detail"]
            .as_str()
            .unwrap()
            .contains("inside the repository")
    );
}

#[test]
fn library_inside_the_analysed_repository_is_not_loaded() {
    let world = World::new("repolib");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    ok(&world.install(&["lua2"]));
    // The cache base is outside, but the repository is `<cache>/rotter`, holding parsers/.
    let rotter = world.parsers().parent().unwrap().to_owned();
    fs::write(rotter.join("a.lua2"), LUA_FILE).unwrap();
    git(&rotter, &["init", "-q", "-b", "main"]);
    git(&rotter, &["add", "a.lua2"]);
    git(&rotter, &["commit", "-q", "-m", "c"]);
    let side = world.side(&rotter, "a.lua2", &[]);
    assert_eq!(side["status"], "parser_not_installed", "{side}");
    assert!(
        side["detail"]
            .as_str()
            .unwrap()
            .contains("inside the repository"),
        "{side}"
    );
    // Same for a repository rooted at parsers/ itself.
    let parsers = world.parsers();
    fs::remove_dir_all(rotter.join(".git")).unwrap();
    fs::rename(rotter.join("a.lua2"), parsers.join("a.lua2")).unwrap();
    git(&parsers, &["init", "-q", "-b", "main"]);
    git(&parsers, &["add", "a.lua2"]);
    git(&parsers, &["commit", "-q", "-m", "c"]);
    assert_eq!(world.status(&parsers, "a.lua2"), "parser_not_installed");
}

#[test]
fn changed_header_symbol_or_location_needs_a_reinstall() {
    let world = World::new("key");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    let lua3 = |location: &str, symbol: &str| {
        format!(
            "[language.lua3]\nurl = \"https://example.invalid/lua.git\"\nrevision = \"{REVISION}\"\n\
             {location}symbol = \"{symbol}\"\nextensions = [\"lua3\"]\nunits = [\"function_declaration\"]\n"
        )
    };
    world.write_config(&format!("{}\n{}", lua2(&src), lua3("", "tree_sitter_lua")));
    let repo = repo(&world.root, &[("a.lua2", LUA_FILE), ("a.lua3", LUA_FILE)]);
    ok(&world.install(&["lua2"]));
    assert_eq!(world.status(&repo, "a.lua2"), "ok");

    // A header-only change changes the key of a local grammar.
    let mut header = fs::OpenOptions::new()
        .append(true)
        .open(src.join("tree_sitter/alloc.h"))
        .unwrap();
    writeln!(header, "/* changed */").unwrap();
    assert_eq!(world.status(&repo, "a.lua2"), "parser_not_installed");
    ok(&world.install(&["lua2"]));
    assert_eq!(world.status(&repo, "a.lua2"), "ok");
    assert_eq!(world.listing("parsers").len(), 2);

    // A git grammar's file name covers revision, location and symbol: the lua library placed
    // under the name for (no location, tree_sitter_lua) loads; any change needs a reinstall.
    let library = world.parsers().join(&world.listing("parsers")[0]);
    let target = world
        .parsers()
        .join(git_file("lua3", "", "tree_sitter_lua"));
    fs::copy(&library, &target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    let side = world.side(&repo, "a.lua3", &[]);
    assert_eq!(side["status"], "ok", "{side}");
    assert_eq!(side["units"][0]["comments"][0]["text"], "-- Adds one.");
    for changed in [
        lua3("location = \"x\"\n", "tree_sitter_lua"),
        lua3("", "tree_sitter_lua_x"),
    ] {
        world.write_config(&format!("{}\n{changed}", lua2(&src)));
        assert_eq!(
            world.status(&repo, "a.lua3"),
            "parser_not_installed",
            "{changed}"
        );
    }
    assert_ne!(
        git_file("lua3", "x", "tree_sitter_lua"),
        git_file("lua3", "", "tree_sitter_lua")
    );
}

#[test]
fn failed_build_leaves_nothing() {
    let world = World::new("fail");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    fs::write(src.join("parser.c"), "this is not C\n").unwrap();
    world.write_config(&lua2(&src));
    let output = world.install(&["lua2"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cc "));
    assert!(world.listing("parsers").is_empty());
    assert!(
        world.listing("build").is_empty(),
        "no staging directory or lib.tmp remains"
    );
}

#[test]
fn unsafe_local_inputs_are_refused_before_cc_runs() {
    let mut world = World::new("inputs");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    let bin = world.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = world.root.join("cc-ran");
    executable(
        &bin.join("cc"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    );
    world.set("PATH", path_with(&bin));
    let refused = |world: &World, what: &str| {
        let output = world.install(&["lua2"]);
        assert_eq!(output.status.code(), Some(2), "{what}: {output:?}");
        assert!(!marker.exists(), "{what}: cc ran");
        assert!(world.listing("parsers").is_empty(), "{what}");
    };

    fs::set_permissions(src.join("parser.c"), fs::Permissions::from_mode(0o664)).unwrap();
    refused(&world, "group-writable parser.c");
    fs::set_permissions(src.join("parser.c"), fs::Permissions::from_mode(0o644)).unwrap();

    fs::set_permissions(src.join("tree_sitter"), fs::Permissions::from_mode(0o770)).unwrap();
    refused(&world, "group-writable tree_sitter/");
    fs::set_permissions(src.join("tree_sitter"), fs::Permissions::from_mode(0o700)).unwrap();

    let real_parser = world.root.join("parser.c");
    fs::rename(src.join("parser.c"), &real_parser).unwrap();
    symlink(&real_parser, src.join("parser.c")).unwrap();
    refused(&world, "symlinked parser.c");
    fs::remove_file(src.join("parser.c")).unwrap();
    fs::rename(&real_parser, src.join("parser.c")).unwrap();

    fs::write(src.join("scanner.cc"), "").unwrap();
    refused(&world, "C++ scanner");
    fs::remove_file(src.join("scanner.cc")).unwrap();

    // Everything safe: cc runs (and this fake one fails).
    let output = world.install(&["lua2"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(marker.exists(), "cc ran once the inputs were safe");
    assert!(world.listing("parsers").is_empty() && world.listing("build").is_empty());
}

/// Records argv, cwd and environment of every call, then runs `target`.
fn recorder(dir: &Path, name: &str, target: &Path, extra: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let log = dir.join(format!("{name}.log"));
    executable(
        &dir.join(name),
        &format!(
            "#!/bin/sh\n{{ printf 'ARGV'; for a in \"$@\"; do printf ' %s' \"$a\"; done; \
             printf '\\nCWD %s\\n' \"$(pwd -P)\"; env | sed 's/^/ENV /'; echo END; }} >> '{log}'\n\
             {extra}exec '{target}' \"$@\"\n",
            log = log.display(),
            target = target.display()
        ),
    );
    log
}

struct Call {
    argv: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
}

fn calls(log: &Path) -> Vec<Call> {
    let text = fs::read_to_string(log).unwrap_or_default();
    let mut calls = Vec::new();
    for block in text.split("END\n").filter(|block| !block.trim().is_empty()) {
        let mut call = Call {
            argv: Vec::new(),
            cwd: String::new(),
            env: BTreeMap::new(),
        };
        for line in block.lines() {
            if let Some(argv) = line.strip_prefix("ARGV") {
                call.argv = argv.split_whitespace().map(str::to_owned).collect();
            } else if let Some(cwd) = line.strip_prefix("CWD ") {
                call.cwd = cwd.to_owned();
            } else if let Some((key, value)) = line
                .strip_prefix("ENV ")
                .and_then(|pair| pair.split_once('='))
            {
                // Variables the recording shell sets itself.
                if !["PWD", "OLDPWD", "SHLVL", "_"].contains(&key) {
                    call.env.insert(key.to_owned(), value.to_owned());
                }
            }
        }
        calls.push(call);
    }
    calls
}

fn developer_dir() -> String {
    Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8(output.stdout).unwrap().trim().to_owned())
        .unwrap_or_else(|| "/usr".to_owned())
}

fn keys(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn compiler_environment_is_an_allowlist() {
    let mut world = World::new("ccenv");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    let bin = world.root.join("bin");
    let log = recorder(&bin, "cc", &real("cc"), "");
    world.set("PATH", path_with(&bin));
    let canaries: Vec<PathBuf> = ["deps-output", "sunpro"]
        .iter()
        .map(|name| world.root.join(name))
        .collect();
    for canary in &canaries {
        fs::write(canary, "canary\n").unwrap();
    }
    world.set("DEPENDENCIES_OUTPUT", canaries[0].clone());
    world.set("SUNPRO_DEPENDENCIES", canaries[1].clone());
    world.set("GIT_DIR", world.root.join("nowhere"));
    world.set("CPATH", "/nonexistent-include");
    world.set("COMPILER_PATH", "/nonexistent-compiler");
    world.set("DEVELOPER_DIR", developer_dir());
    ok(&world.install(&["lua2"]));

    let recorded = calls(&log);
    assert_eq!(recorded.len(), 3, "two compiles and one link");
    let base = fs::canonicalize(&world.cache).unwrap().join("rotter/build");
    let expected = keys(&[
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "TMP",
        "TEMP",
        "TEMPDIR",
        "CLANG_CRASH_DIAGNOSTICS_DIR",
        "DEVELOPER_DIR",
    ]);
    for call in &recorded {
        assert_eq!(call.env.keys().cloned().collect::<BTreeSet<_>>(), expected);
        assert!(Path::new(&call.cwd).starts_with(&base), "{}", call.cwd);
        for key in [
            "TMPDIR",
            "TMP",
            "TEMP",
            "TEMPDIR",
            "CLANG_CRASH_DIAGNOSTICS_DIR",
        ] {
            assert_eq!(call.env[key], call.cwd, "{key}");
        }
        assert_eq!(call.env["LANG"], "C");
        assert_eq!(call.env["DEVELOPER_DIR"], developer_dir());
        assert!(
            !call.env["PATH"]
                .split(':')
                .any(|entry| !entry.starts_with('/'))
        );
    }
    let staging = &recorded[0].cwd;
    for (call, object) in recorded[..2].iter().zip(["parser.o", "scanner.o"]) {
        assert!(call.argv.contains(&"-c".to_owned()), "{:?}", call.argv);
        let output = call
            .argv
            .iter()
            .skip_while(|arg| *arg != "-o")
            .nth(1)
            .unwrap();
        assert_eq!(output, &format!("{staging}/obj/{object}"));
    }
    assert_eq!(recorded[2].argv[0], "-shared");
    assert_eq!(
        recorded[2].argv.last().unwrap(),
        &format!("{staging}/lib.tmp")
    );
    for canary in &canaries {
        assert_eq!(fs::read_to_string(canary).unwrap(), "canary\n");
    }
    assert!(world.listing("build").is_empty());

    // A relative DEVELOPER_DIR is dropped.
    fs::remove_file(&log).unwrap();
    world.set("DEVELOPER_DIR", "relative/dev");
    ok(&world.install(&["lua2"]));
    assert!(
        calls(&log)
            .iter()
            .all(|call| !call.env.contains_key("DEVELOPER_DIR"))
    );
}

/// A git wrapper: records every call, requires DEVELOPER_DIR, never fetches, and fakes
/// `rev-parse --git-common-dir` according to `<dir>/mode`.
fn git_wrapper(dir: &Path) -> PathBuf {
    let real_git = real("git");
    let extra = format!(
        "[ -n \"$DEVELOPER_DIR\" ] || {{ echo 'shim needs DEVELOPER_DIR' >&2; exit 97; }}\n\
         case \" $* \" in\n\
         *' rev-parse --git-common-dir '*)\n\
           case \"$(cat '{dir}/mode' 2>/dev/null)\" in\n\
             outside) cat '{dir}/outside'; exit 0;;\n\
             missing) echo '/nonexistent/rotter-missing'; exit 0;;\n\
           esac\n\
           out=$('{git}' \"$@\") || exit $?\n\
           echo \"COMMONDIR $out\" >> '{dir}/git.log'; echo \"$out\"; exit 0;;\n\
         *' fetch '*) echo 'offline test: no fetch' >&2; exit 1;;\n\
         esac\n",
        dir = dir.display(),
        git = real_git.display()
    );
    recorder(dir, "git", &real_git, &extra)
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

const FAKE_GIT_GRAMMAR: &str = "[language.fake]\nurl = \"https://example.invalid/fake.git\"\n\
    revision = \"0123456789abcdef0123456789abcdef01234567\"\nsymbol = \"tree_sitter_fake\"\n\
    extensions = [\"fake\"]\nunits = [\"x\"]\n";

#[test]
fn git_ignores_ambient_repositories_and_config() {
    let mut world = World::new("gitenv");
    world.write_config(FAKE_GIT_GRAMMAR);
    let canary = world.root.join("canary");
    fs::create_dir_all(&canary).unwrap();
    git(&canary, &["init", "-q"]);
    fs::write(canary.join("f"), "x").unwrap();
    git(&canary, &["add", "-A"]);
    git(&canary, &["commit", "-q", "-m", "c"]);
    let before = snapshot(&canary.join(".git"));
    // A private HOME whose global config points init.templateDir at a template with commondir.
    let home = PathBuf::from(&world.env["HOME"]);
    let template = world.root.join("template");
    fs::create_dir_all(&template).unwrap();
    fs::write(
        template.join("commondir"),
        format!("{}\n", canary.join(".git").display()),
    )
    .unwrap();
    fs::write(
        home.join(".gitconfig"),
        format!("[init]\n\ttemplateDir = {}\n", template.display()),
    )
    .unwrap();
    let bin = world.root.join("bin");
    let log = git_wrapper(&bin);
    world.set("PATH", path_with(&bin));
    world.set("GIT_DIR", canary.join(".git"));
    world.set("GIT_COMMON_DIR", canary.join(".git"));
    world.set("GIT_CONFIG_PARAMETERS", "'core.hooksPath=/tmp/evil'");
    world.set("DEVELOPER_DIR", developer_dir());
    world.set("HTTPS_PROXY", "http://127.0.0.1:9");

    let output = world.install(&["fake"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("offline test: no fetch"));
    assert_eq!(snapshot(&canary.join(".git")), before, "canary unchanged");
    let text = fs::read_to_string(&log).unwrap();
    let common = text
        .lines()
        .find_map(|line| line.strip_prefix("COMMONDIR "))
        .unwrap();
    assert_eq!(
        common, ".git",
        "the common dir is the staging repository's own"
    );
    let recorded = calls(&log);
    let verbs: Vec<&str> = recorded
        .iter()
        .map(|call| {
            let skip = call
                .argv
                .iter()
                .position(|arg| arg == "core.attributesFile=/dev/null");
            call.argv[skip.unwrap() + 1].as_str()
        })
        .collect();
    assert_eq!(verbs[..3], ["init", "rev-parse", "rev-parse"], "{verbs:?}");
    assert!(
        verbs.contains(&"config") && verbs.last() == Some(&"fetch"),
        "{verbs:?}"
    );
    let expected = keys(&[
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "TMP",
        "TEMP",
        "TEMPDIR",
        "DEVELOPER_DIR",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_TERMINAL_PROMPT",
        "GIT_LFS_SKIP_SMUDGE",
        "HTTPS_PROXY",
    ]);
    let build = fs::canonicalize(&world.cache).unwrap().join("rotter/build");
    for call in &recorded {
        assert_eq!(call.env.keys().cloned().collect::<BTreeSet<_>>(), expected);
        assert_eq!(call.env["GIT_CONFIG_GLOBAL"], "/dev/null");
        assert_eq!(call.argv[0], "-C");
        assert!(
            Path::new(&call.argv[1]).starts_with(&build),
            "{:?}",
            call.argv
        );
        for flag in [
            "core.symlinks=false",
            "core.hooksPath=/dev/null",
            "protocol.allow=never",
            "protocol.https.allow=always",
            "submodule.recurse=false",
            "core.fsmonitor=false",
            "credential.helper=",
            "core.attributesFile=/dev/null",
        ] {
            assert!(
                call.argv.iter().any(|arg| arg == flag),
                "{flag}: {:?}",
                call.argv
            );
        }
    }
    assert!(world.listing("build").is_empty());
}

#[test]
fn outside_or_missing_common_dir_stops_before_fetch_or_config() {
    let mut world = World::new("commondir");
    world.write_config(FAKE_GIT_GRAMMAR);
    let bin = world.root.join("bin");
    let log = git_wrapper(&bin);
    world.set("PATH", path_with(&bin));
    world.set("DEVELOPER_DIR", developer_dir());
    let outside = world.root.join("outside/.git");
    fs::create_dir_all(&outside).unwrap();
    fs::write(bin.join("outside"), format!("{}\n", outside.display())).unwrap();
    for mode in ["outside", "missing"] {
        fs::write(bin.join("mode"), mode).unwrap();
        let _ = fs::remove_file(&log);
        let output = world.install(&["fake"]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("refusing to install"), "{mode}: {stderr}");
        let recorded = calls(&log);
        assert!(!recorded.is_empty());
        for call in recorded {
            assert!(
                !call
                    .argv
                    .iter()
                    .any(|arg| arg == "fetch" || arg == "config"),
                "{mode}: {:?}",
                call.argv
            );
        }
        assert!(world.listing("build").is_empty());
    }
}

#[test]
fn registry_dockerfile_detection_by_file_name() {
    let world = World::new("docker");
    world.write_config("languages = [\"dockerfile\", \"python\"]\n");
    let repo = repo(
        &world.root,
        &[
            ("Dockerfile", "# Base.\nFROM alpine\n"),
            ("Dockerfile.prod", "# Base.\nFROM alpine\n"),
            ("Containerfile", "# Base.\nFROM alpine\n"),
            ("script", "#!/bin/bash\n# F.\nf() { :; }\n"),
            ("a.py", "# X.\nx = 1\n"),
        ],
    );
    for path in ["Dockerfile", "Dockerfile.prod", "Containerfile"] {
        let side = world.side(&repo, path, &[]);
        assert_eq!(side["language"], "dockerfile", "{path}: {side}");
        assert_eq!(side["status"], "parser_not_installed", "{path}");
    }
    assert_eq!(world.side(&repo, "script", &[])["language"], "bash");
    assert_eq!(world.side(&repo, "a.py", &[])["language"], "python");
    let output = world.run(&["parser", "list"]);
    ok(&output);
    let list = document(&output, "rotter.parsers/1");
    let dockerfile = listed(&list, "dockerfile");
    assert_eq!(
        (
            &dockerfile["enabled"],
            &dockerfile["installed"],
            &dockerfile["path"]
        ),
        (&true.into(), &false.into(), &Value::Null),
        "{list}"
    );
    assert!(
        dockerfile["source"]
            .as_str()
            .unwrap()
            .contains("tree-sitter-dockerfile"),
        "{list}"
    );
    assert!(dockerfile["detail"].is_string(), "{list}");
    let hcl = listed(&list, "hcl");
    assert_eq!(
        (&hcl["enabled"], &hcl["installed"]),
        (&false.into(), &false.into()),
        "{list}"
    );
    let output = world.install(&["hcl"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not enabled"));
}

/// Runs the hook for `repo` with state, Claude settings and XDG dirs pinned inside the world.
fn hook(world: &World, repo: &Path) -> String {
    let state = world.root.join("state");
    let mut child = world
        .command(&["hook", "claude-stop"])
        .env("ROTTER_STATE_DIR", &state)
        .env("CLAUDE_CONFIG_DIR", world.root.join("claude"))
        .env("CODEX_HOME", world.root.join("codex"))
        .env("COPILOT_HOME", world.root.join("copilot"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!(r#"{{"session_id": "s", "cwd": "{}"}}"#, repo.display());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(60), "hook hangs");
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn hook_never_builds_and_uses_installed_grammars() {
    let mut world = World::new("hook");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    world.write_config(&lua2(&src));
    let original = "-- Leading comment.\nlocal function f()\n  return 1\nend\n";
    let repo = repo(&world.root, &[("a.lua2", original)]);
    fs::write(
        repo.join("a.lua2"),
        "-- Leading comment.\nlocal function f()\n  return 2\nend\n",
    )
    .unwrap();

    // Enabled but not installed: reported, and a cc on PATH is never run.
    let bin = world.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = world.root.join("cc-ran");
    executable(
        &bin.join("cc"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    let path = world.env["PATH"].clone();
    world.set("PATH", path_with(&bin));
    let output = hook(&world, &repo);
    assert!(output.contains("could not be analysed"), "{output}");
    assert!(!marker.exists(), "the hook never compiles");
    world.set("PATH", path);

    ok(&world.install(&["lua2"]));
    let report: Value = serde_json::from_slice(
        &world
            .run(&["extract", "--worktree", "-C", repo.to_str().unwrap()])
            .stdout,
    )
    .unwrap();
    let file = &report["files"][0];
    for side in ["before", "after"] {
        let unit = &file[side]["units"][0];
        assert_eq!(unit["kind"], "function_declaration", "{side}: {file}");
        let comment = &unit["comments"][0];
        assert_eq!(comment["relation"], "leading");
        assert_eq!(comment["text"], "-- Leading comment.");
        assert_eq!(comment["range"]["lines"], serde_json::json!([1, 1]));
        assert_eq!(comment["range"]["bytes"], serde_json::json!([0, 19]));
        assert_eq!(comment["changed"], false);
    }
    let first = hook(&world, &repo);
    assert!(first.contains(r#""decision":"block""#), "{first}");
    assert_eq!(
        hook(&world, &repo),
        "",
        "the same report is not reviewed twice"
    );
}

#[test]
fn fifo_parser_source_does_not_block_the_hook() {
    let world = World::new("fifo");
    let src = world.root.join("lua-src");
    copy_lua(&src);
    fs::remove_file(src.join("parser.c")).unwrap();
    let status = Command::new("mkfifo")
        .arg(src.join("parser.c"))
        .status()
        .unwrap();
    assert!(status.success());
    world.write_config(&lua2(&src));
    let repo = repo(&world.root, &[("a.lua2", LUA_FILE)]);
    fs::write(repo.join("a.lua2"), format!("{LUA_FILE}local y = 2\n")).unwrap();
    let output = hook(&world, &repo);
    assert!(output.contains("could not be analysed"), "{output}");
    assert_eq!(world.status(&repo, "a.lua2"), "parser_not_installed");
}
