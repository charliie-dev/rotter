use crate::config::{Untrusted, lstat, resolve_trusted, user};
use crate::grammar::{create_private, open_regular};
use crate::json::Json;
use crate::{
    Change, Detected, Grammar, Languages, ParseError, error_lines, full_units, parse_with_deadline,
    units,
};
use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Which pair of snapshots to compare; there is deliberately no default.
#[derive(Clone, Debug)]
pub enum Mode {
    /// HEAD against the index.
    Staged,
    /// HEAD against the working tree (staged and unstaged changes combined).
    Worktree,
    /// An explicit revision against the working tree.
    Base(String),
    /// Every tracked file in the working tree, without a diff.
    Full,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub mode: Mode,
    pub include_untracked: bool,
    /// Git pathspecs relative to the directory the command runs in; empty means the whole repo.
    pub paths: Vec<String>,
    /// `--lang` then config `[overrides]`: repository-relative glob, grammar and dialect label.
    pub languages: Vec<(String, Arc<Grammar>, String)>,
    /// Builtins plus enabled externals, for extension, shebang and file name detection.
    pub grammars: Languages,
    /// Per-file parse limit (`parse_timeout_seconds`).
    pub parse_timeout: Duration,
    /// Soft run-wide deadline: files not started by then are skipped as `parse_timeout`.
    pub deadline: Option<Instant>,
}

impl Options {
    /// Builtin languages, no overrides, the default 60 s parse limit and no run deadline.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            include_untracked: false,
            paths: Vec::new(),
            languages: Vec::new(),
            grammars: Languages::default(),
            parse_timeout: Duration::from_secs(crate::config::DEFAULT_PARSE_TIMEOUT),
            deadline: None,
        }
    }
}

pub struct Report {
    pub json: Json,
    /// False when any in-scope file could not be read or parsed.
    pub complete: bool,
}

fn git(dir: &Path, args: &[&OsStr], input: Option<&[u8]>, ok: &[i32]) -> Result<Vec<u8>, String> {
    git_in(dir, args, input, ok, None, None)
}

/// Runs git, resolved from the absolute PATH entries only; `index` points it at a private copy so
/// index refreshes never touch the repository, and `ceiling` stops repository discovery.
fn git_in(
    dir: &Path,
    args: &[&OsStr],
    input: Option<&[u8]>,
    ok: &[i32],
    index: Option<&Path>,
    ceiling: Option<&Path>,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(crate::install::git_program()?);
    if let Some(index) = index {
        // An unsplit private index keeps git from writing sharedindex files into $GIT_DIR.
        command
            .env("GIT_INDEX_FILE", index)
            .args(["-c", "core.splitIndex=false"]);
    }
    if let Some(ceiling) = ceiling {
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    let mut child = command
        .arg("-C")
        .arg(dir)
        .args(["--no-pager", "-c", "core.fsmonitor=false"])
        .args(args)
        // Never write the index opportunistically; the checked repository stays untouched.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(input)
            .map_err(|error| format!("cannot write to git: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("git failed: {error}"))?;
    match output.status.code() {
        Some(code) if ok.contains(&code) => Ok(output.stdout),
        _ => Err(format!(
            "git {} failed: {}",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let output = git(dir, &args, None, &[0])?;
    String::from_utf8(output)
        .map(|text| text.trim_end().to_owned())
        .map_err(|_| "git printed non-UTF-8 output".to_owned())
}

/// The repository's top-level directory as git reports it.
pub fn toplevel(dir: &Path) -> Result<PathBuf, String> {
    git_text(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// Private scratch directory under the trusted temp root for the index copy and diff inputs;
/// removed on drop.
struct Scratch {
    dir: PathBuf,
    /// `resolve_trusted(temp_dir())`: the ceiling for `git diff --no-index`.
    root: PathBuf,
    /// Numbers the per-diff `diff-<n>` subdirectories.
    diffs: Cell<usize>,
}

/// The temp root through [`resolve_trusted`]; refused when unsafe or not expressible as a
/// `GIT_CEILING_DIRECTORIES` entry.
fn temp_root(temp: &Path) -> Result<PathBuf, String> {
    let root = match resolve_trusted(temp, user(), &lstat) {
        Ok(root) => root,
        Err(Untrusted::Missing) => {
            return Err(format!(
                "temporary directory {} (TMPDIR) does not exist",
                temp.display()
            ));
        }
        Err(Untrusted::Refused(why)) => {
            return Err(format!(
                "temporary directory {} (TMPDIR) refused: {why}; set TMPDIR to a directory only \
                 you can write",
                temp.display()
            ));
        }
    };
    if root.as_os_str().as_bytes().contains(&b':') {
        return Err(format!(
            "temporary directory {} (TMPDIR) resolves to {}, which contains ':'; set TMPDIR to a \
             path without ':'",
            temp.display(),
            root.display()
        ));
    }
    Ok(root)
}

impl Scratch {
    fn new() -> Result<Self, String> {
        let root = temp_root(&std::env::temp_dir())?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos());
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = root.join(format!(
            "rotter-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_dir(&dir)?;
        Ok(Self {
            dir,
            root,
            diffs: Cell::new(0),
        })
    }

    /// A fresh `diff-<n>` subdirectory; one that already exists is an error.
    fn diff_dir(&self) -> Result<PathBuf, String> {
        let n = self.diffs.get();
        self.diffs.set(n + 1);
        let dir = self.dir.join(format!("diff-{n}"));
        private_dir(&dir)?;
        Ok(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Creates one directory (never its parents) with mode 0700; an existing entry is an error.
fn private_dir(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

/// Writes `dir/name` once: a pre-existing entry or symlink is an error, never followed.
fn write_new(dir: &Path, name: &str, content: &[u8]) -> Result<PathBuf, String> {
    let path = dir.join(name);
    create_private(&path)
        .and_then(|mut file| file.write_all(content))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path)
}

/// Copies the repository index into the scratch directory; only a regular file is copied.
fn copy_index(real: &Path, copy: &Path) -> Result<(), String> {
    let Some(mut source) = open_regular(real)? else {
        return Ok(());
    };
    let fail = |error: std::io::Error| format!("cannot copy {}: {error}", real.display());
    let mut target = create_private(copy).map_err(fail)?;
    std::io::copy(&mut source, &mut target).map_err(fail)?;
    // Git treats entries as racily clean by comparing them with the index file's mtime; a fresh
    // mtime would hide same-size edits made within the same timestamp granularity.
    let modified = source
        .metadata()
        .and_then(|meta| meta.modified())
        .map_err(fail)?;
    target
        .set_times(fs::FileTimes::new().set_modified(modified))
        .map_err(fail)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Hunk {
    before: (usize, usize),
    after: (usize, usize),
}

fn parse_range(text: &str) -> Option<(usize, usize)> {
    let (start, count) = text.split_once(',').unwrap_or((text, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
}

/// Line hunks between two texts, computed by Git from exactly these bytes.
fn diff(scratch: &Scratch, before: &str, after: &str) -> Result<Vec<Hunk>, String> {
    let dir = scratch.diff_dir()?;
    let output = diff_in(&dir, &scratch.root, before, after);
    let _ = fs::remove_dir_all(&dir);
    let mut hunks = Vec::new();
    for line in String::from_utf8_lossy(&output?).lines() {
        let Some(header) = line.strip_prefix("@@ -") else {
            continue;
        };
        let mut parts = header.split(' ');
        let before = parts.next().and_then(parse_range);
        let after = parts
            .next()
            .and_then(|part| part.strip_prefix('+'))
            .and_then(parse_range);
        match (before, after) {
            (Some(before), Some(after)) => hunks.push(Hunk { before, after }),
            _ => return Err(format!("unexpected diff header: {line}")),
        }
    }
    Ok(hunks)
}

/// `git diff --no-index` of `before` and `after`, written once each into the private `dir`.
fn diff_in(dir: &Path, ceiling: &Path, before: &str, after: &str) -> Result<Vec<u8>, String> {
    let old = write_new(dir, "before", before.as_bytes())?;
    let new = write_new(dir, "after", after.as_bytes())?;
    let args: Vec<&OsStr> = [
        "diff",
        "--no-index",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--text",
        "-U0",
        "--",
    ]
    .iter()
    .map(OsStr::new)
    .chain([old.as_os_str(), new.as_os_str()])
    .collect();
    // No repository discovery above the scratch directory.
    git_in(dir, &args, None, &[0, 1], None, Some(ceiling))
}

fn changes(hunks: &[Hunk], after: bool) -> Vec<Change> {
    hunks
        .iter()
        .map(|hunk| {
            let (start, count) = if after { hunk.after } else { hunk.before };
            if count == 0 {
                Change::Gap(start)
            } else {
                Change::Rows(start - 1..start - 1 + count)
            }
        })
        .collect()
}

struct Entry {
    status: String,
    old: Option<(Vec<u8>, String, String)>,
    new: Option<(Vec<u8>, String, String)>,
}

fn is_zero(oid: &str) -> bool {
    oid.bytes().all(|byte| byte == b'0')
}

/// Parses `git diff --raw -z` records: `:old_mode new_mode old_oid new_oid status\0path\0[path\0]`.
fn parse_raw(output: &[u8]) -> Result<Vec<Entry>, String> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(meta) = fields.next() {
        let meta = std::str::from_utf8(meta).map_err(|_| "unreadable diff record")?;
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = parts[..] else {
            return Err(format!("unexpected diff record: {meta}"));
        };
        let first = fields.next().ok_or("diff record without path")?.to_vec();
        let second = if status.starts_with(['R', 'C']) {
            Some(fields.next().ok_or("rename without target")?.to_vec())
        } else {
            None
        };
        let side = |path: Vec<u8>, mode: &str, oid: &str| (path, mode.to_owned(), oid.to_owned());
        let (old, new) = match (status.chars().next(), second) {
            (Some('A'), _) => (None, Some(side(first, new_mode, new_oid))),
            (Some('D'), _) => (Some(side(first, old_mode, old_oid)), None),
            (_, Some(target)) => (
                Some(side(first, old_mode, old_oid)),
                Some(side(target, new_mode, new_oid)),
            ),
            _ => (
                Some(side(first.clone(), old_mode, old_oid)),
                Some(side(first, new_mode, new_oid)),
            ),
        };
        entries.push(Entry {
            status: status.to_owned(),
            old,
            new,
        });
    }
    Ok(entries)
}

struct Side {
    json: Vec<(&'static str, Json)>,
    text: Option<String>,
    language: Option<Arc<Grammar>>,
    in_scope: bool,
}

impl Side {
    fn status(mut self, status: &str, detail: Option<String>, in_scope: bool) -> Self {
        self.json.push(("status", status.into()));
        self.json.push(("detail", detail.into()));
        self.in_scope = in_scope;
        self
    }

    fn ok(&self) -> bool {
        self.text.is_some()
    }
}

enum Source<'a> {
    Blob(&'a str),
    Disk,
}

struct Run<'a> {
    top: &'a Path,
    scratch: Scratch,
    options: &'a Options,
}

impl Run<'_> {
    fn deadline_passed(&self) -> bool {
        self.options
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn load(&self, path: &[u8], mode: &str, source: Source<'_>) -> Side {
        let display = String::from_utf8_lossy(path).into_owned();
        let relative = Path::new(OsStr::from_bytes(path));
        let mut side = Side {
            json: vec![("path", display.clone().into())],
            text: None,
            language: None,
            in_scope: false,
        };
        match mode {
            "120000" => return side.status("skipped_symlink", None, false),
            "160000" => return side.status("skipped_submodule", None, false),
            _ => {}
        }
        let grammars = &self.options.grammars;
        let override_language = self
            .options
            .languages
            .iter()
            .find(|(pattern, _, _)| crate::glob_match(pattern, &display));
        let mut detected = match override_language {
            Some((_, grammar, dialect)) => {
                Detected::Supported(Arc::clone(grammar), dialect.clone())
            }
            None => grammars.detect_path(relative),
        };
        if matches!(detected, Detected::NotInScope) {
            return side.status("not_in_scope", None, false);
        }
        if self.deadline_passed() {
            return side.status(
                "parse_timeout",
                Some("skipped: the run's time budget ran out before this file".into()),
                true,
            );
        }
        let bytes = match source {
            Source::Blob(oid) => git(
                self.top,
                &[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(oid)],
                None,
                &[0],
            ),
            Source::Disk => {
                let full = self.top.join(relative);
                match fs::symlink_metadata(&full) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return side.status("skipped_symlink", None, false);
                    }
                    Ok(meta) if !meta.is_file() => Err("not a regular file".to_owned()),
                    Ok(_) if matches!(detected, Detected::NeedsContent) => {
                        // Decide from the first line before reading the rest of the file.
                        let mut first = Vec::new();
                        let prefix = fs::File::open(&full)
                            .and_then(|file| file.take(4096).read_to_end(&mut first))
                            .map_err(|error| error.to_string());
                        match prefix.map(|_| {
                            let line = first
                                .split(|byte| *byte == b'\n')
                                .next()
                                .unwrap_or_default();
                            grammars.detect_content(relative, &String::from_utf8_lossy(line))
                        }) {
                            Ok(Detected::NotInScope) => {
                                return side.status("not_in_scope", None, false);
                            }
                            Ok(Detected::UnsupportedDialect(dialect)) => {
                                side.json.push(("dialect", dialect.into()));
                                return side.status(
                                    "unsupported_dialect",
                                    Some("only Bash is supported".into()),
                                    true,
                                );
                            }
                            Ok(_) => fs::read(&full).map_err(|error| error.to_string()),
                            Err(error) => Err(error),
                        }
                    }
                    Ok(_) => fs::read(&full).map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                }
            }
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(error) => return side.status("read_error", Some(error), true),
        };
        if matches!(detected, Detected::NeedsContent) {
            let first = bytes
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            detected = grammars.detect_content(relative, &String::from_utf8_lossy(first));
        }
        let (language, dialect) = match detected {
            Detected::Supported(language, dialect) => (language, dialect),
            Detected::UnsupportedDialect(dialect) => {
                side.json.push(("dialect", dialect.into()));
                return side.status(
                    "unsupported_dialect",
                    Some("only Bash is supported".into()),
                    true,
                );
            }
            Detected::NotInScope | Detected::NeedsContent => {
                return side.status("not_in_scope", None, false);
            }
        };
        side.json.push(("language", language.name().into()));
        side.json.push(("dialect", dialect.into()));
        // The only place a grammar that cannot be loaded becomes a status.
        if let Err(detail) = language.language() {
            return side.status("parser_not_installed", Some(detail), true);
        }
        let blob = match source {
            Source::Blob(oid) => Ok(oid.to_owned()),
            Source::Disk => git(
                self.top,
                &["hash-object", "--no-filters", "--stdin"].map(OsStr::new),
                Some(&bytes),
                &[0],
            )
            .map(|out| String::from_utf8_lossy(&out).trim().to_owned()),
        };
        match blob {
            Ok(blob) => side.json.push(("blob", blob.into())),
            Err(error) => return side.status("read_error", Some(error), true),
        }
        match String::from_utf8(bytes) {
            // Tree-sitter stops at NUL, so the rest of the file would be silently skipped.
            Ok(text) if text.contains('\0') => side.status("contains_nul", None, true),
            Ok(text) => {
                side.text = Some(text);
                side.language = Some(language);
                side.in_scope = true;
                side
            }
            Err(_) => side.status("not_utf8", None, true),
        }
    }

    fn file(&self, entry: &Entry, after_on_disk: bool, full: bool) -> Result<(Json, bool), String> {
        let before = entry
            .old
            .as_ref()
            .map(|(path, mode, oid)| self.load(path, mode, Source::Blob(oid)));
        let after = entry.new.as_ref().map(|(path, mode, oid)| {
            if after_on_disk {
                self.load(path, mode, Source::Disk)
            } else if is_zero(oid) {
                Side {
                    json: vec![("path", String::from_utf8_lossy(path).into_owned().into())],
                    text: None,
                    language: None,
                    in_scope: true,
                }
                .status("unmerged", None, true)
            } else {
                self.load(path, mode, Source::Blob(oid))
            }
        });
        let sides = [before, after];
        let in_scope = sides.iter().flatten().any(|side| side.in_scope);
        let readable = sides
            .iter()
            .flatten()
            .all(|side| !side.in_scope || side.ok());
        let hunks = if in_scope && readable && !full {
            let text = |side: &Option<Side>| {
                side.as_ref()
                    .and_then(|side| side.text.clone())
                    .unwrap_or_default()
            };
            diff(&self.scratch, &text(&sides[0]), &text(&sides[1]))?
        } else {
            Vec::new()
        };
        let mut complete = readable;
        let [before, after] = sides;
        let side_json = |side: Option<Side>, is_after: bool, complete: &mut bool| {
            let mut side = side?;
            if let (Some(text), Some(language)) = (&side.text, side.language.clone()) {
                let limit = self.options.parse_timeout;
                let budget = self.options.deadline.map_or(limit, |deadline| {
                    limit.min(deadline.saturating_duration_since(Instant::now()))
                });
                match parse_with_deadline(&language, text, budget) {
                    Ok(tree) => {
                        // Keep what parsed; units touching an error are flagged, not dropped.
                        let errors = error_lines(&tree);
                        if errors.is_empty() {
                            side.json.push(("status", "ok".into()));
                            side.json.push(("detail", Json::Null));
                        } else {
                            *complete = false;
                            let lines: Vec<String> =
                                errors.iter().take(20).map(ToString::to_string).collect();
                            side.json.push(("status", "partial".into()));
                            side.json.push((
                                "detail",
                                format!("syntax errors near lines {}", lines.join(", ")).into(),
                            ));
                        }
                        let found = if full {
                            full_units(&language, text, &tree)
                        } else {
                            units(&language, text, &tree, &changes(&hunks, is_after))
                        };
                        side.json.push(("units", found));
                    }
                    Err(ParseError::Timeout) => {
                        *complete = false;
                        side.json.push(("status", "parse_timeout".into()));
                        let detail = if budget < limit {
                            "stopped: the run's time budget ran out during this file".to_owned()
                        } else {
                            format!(
                                "stopped after {} s; the limit is parse_timeout_seconds in \
                                 $XDG_CONFIG_HOME/rotter/config.toml",
                                limit.as_secs()
                            )
                        };
                        side.json.push(("detail", detail.into()));
                    }
                    Err(error) => {
                        *complete = false;
                        side.json.push(("status", "parse_error".into()));
                        side.json.push(("detail", error.to_string().into()));
                    }
                }
            }
            Some(Json::Obj(side.json))
        };
        let before = side_json(before, false, &mut complete);
        let after = side_json(after, true, &mut complete);
        let (change, similarity) = match entry.status.split_at(1) {
            ("M", _) => ("modified", None),
            ("A", _) => ("added", None),
            ("D", _) => ("deleted", None),
            ("R", score) => ("renamed", score.parse::<u32>().ok()),
            ("C", score) => ("copied", score.parse::<u32>().ok()),
            ("T", _) => ("type_changed", None),
            ("U", _) => ("unmerged", None),
            ("?", _) => ("untracked_added", None),
            ("F", _) => ("full", None),
            _ => ("unknown", None),
        };
        if change == "unmerged" || change == "unknown" {
            complete = false;
        }
        let path = |side: &Option<(Vec<u8>, String, String)>| {
            side.as_ref()
                .map(|(path, _, _)| String::from_utf8_lossy(path).into_owned())
        };
        let json = Json::Obj(vec![
            ("change", change.into()),
            ("similarity", similarity.into()),
            ("old_path", path(&entry.old).into()),
            ("new_path", path(&entry.new).into()),
            ("complete", complete.into()),
            (
                "hunks",
                Json::Arr(
                    hunks
                        .iter()
                        .map(|hunk| {
                            Json::Obj(vec![
                                ("before", vec![hunk.before.0, hunk.before.1].into()),
                                ("after", vec![hunk.after.0, hunk.after.1].into()),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("before", before.into()),
            ("after", after.into()),
        ]);
        Ok((json, complete))
    }
}

/// Extracts changed units and their comments for one explicitly chosen diff mode.
pub fn extract(dir: &Path, options: &Options) -> Result<Report, String> {
    let top = toplevel(dir)?;
    let (rev, commit) = match &options.mode {
        Mode::Base(rev) => {
            if rev.is_empty() || rev.starts_with('-') {
                return Err(format!("invalid base revision: {rev:?}"));
            }
            let commit = git_text(
                &top,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{rev}^{{commit}}"),
                ],
            )
            .map_err(|_| format!("cannot resolve base revision {rev:?}"))?;
            (rev.clone(), Some(commit))
        }
        Mode::Staged | Mode::Worktree | Mode::Full => (
            "HEAD".to_owned(),
            git_text(&top, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).ok(),
        ),
    };
    let full = matches!(options.mode, Mode::Full);
    // Pathspecs are relative to `dir`, so commands that take them run there with top-level
    // output paths; `:/` means the whole repository.
    let specs: Vec<&OsStr> = if options.paths.is_empty() {
        vec![OsStr::new(":/")]
    } else {
        options.paths.iter().map(OsStr::new).collect()
    };
    fn args_with<'a>(args: &[&'a str], specs: &[&'a OsStr]) -> Vec<&'a OsStr> {
        args.iter()
            .map(|arg| OsStr::new(*arg))
            .chain([OsStr::new("--")])
            .chain(specs.iter().copied())
            .collect()
    }
    let with_specs = |args: &[&'static str]| args_with(args, &specs);
    let tree = match &commit {
        Some(commit) => commit.clone(),
        None => {
            let args = ["hash-object", "-t", "tree", "--stdin"].map(OsStr::new);
            String::from_utf8_lossy(&git(&top, &args, Some(b""), &[0])?)
                .trim()
                .to_owned()
        }
    };
    // `git diff <tree>` refreshes stat data and rewrites the index even with
    // GIT_OPTIONAL_LOCKS=0, so every index read goes through a private copy.
    let scratch = Scratch::new()?;
    let index = scratch.dir.join("index");
    let real_index = top.join(git_text(&top, &["rev-parse", "--git-path", "index"])?);
    copy_index(&real_index, &index)?;
    let index = Some(index.as_path());
    let mut args = vec!["diff"];
    if matches!(options.mode, Mode::Staged) {
        args.push("--cached");
    }
    args.extend([
        "--raw",
        "--no-abbrev",
        "-z",
        "-M",
        "--no-ext-diff",
        "--no-textconv",
        "--no-relative",
        tree.as_str(),
    ]);
    let mut entries = if full {
        let listed = git_in(
            dir,
            &with_specs(&["ls-files", "-s", "-z", "--full-name"]),
            None,
            &[0],
            index,
            None,
        )?;
        let mut entries: Vec<Entry> = Vec::new();
        for record in listed
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or("unexpected ls-files record")?;
            let mode = String::from_utf8_lossy(&record[..6]).into_owned();
            let path = record[tab + 1..].to_vec();
            // Tracked files deleted from the working tree are not part of this snapshot.
            let on_disk = fs::symlink_metadata(top.join(OsStr::from_bytes(&path))).is_ok();
            if on_disk
                && entries
                    .last()
                    .is_none_or(|last| last.new.as_ref().is_none_or(|new| new.0 != path))
            {
                entries.push(Entry {
                    status: "F".to_owned(),
                    old: None,
                    new: Some((path, mode, String::new())),
                });
            }
        }
        entries
    } else {
        parse_raw(&git_in(
            dir,
            &args_with(&args, &specs),
            None,
            &[0],
            index,
            None,
        )?)?
    };

    // HEAD→disk diffs report conflicted paths as plain modifications; keep them visible.
    let unmerged_args = with_specs(&["ls-files", "--unmerged", "-z", "--full-name"]);
    let mut unmerged: Vec<Vec<u8>> = git_in(dir, &unmerged_args, None, &[0], index, None)?
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let tab = record.iter().position(|byte| *byte == b'\t')?;
            Some(record[tab + 1..].to_vec())
        })
        .collect();
    unmerged.dedup();
    for path in unmerged {
        let has = |side: &Option<(Vec<u8>, String, String)>| {
            side.as_ref().is_some_and(|(other, _, _)| *other == path)
        };
        match entries
            .iter_mut()
            .find(|entry| has(&entry.new) || has(&entry.old))
        {
            Some(entry) => entry.status = "U".to_owned(),
            // Conflicts whose disk content equals the before side produce no diff record.
            None => entries.push(Entry {
                status: "U".to_owned(),
                old: None,
                new: Some((path, "100644".to_owned(), String::new())),
            }),
        }
    }

    let untracked_args = with_specs(&[
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
        "--full-name",
    ]);
    let untracked: Vec<Vec<u8>> = git_in(dir, &untracked_args, None, &[0], index, None)?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    let include = options.include_untracked && !matches!(options.mode, Mode::Staged);
    if include {
        entries.extend(untracked.iter().map(|path| Entry {
            status: "?".to_owned(),
            old: None,
            new: Some((path.clone(), "100644".to_owned(), String::new())),
        }));
    }

    let run = Run {
        top: &top,
        scratch,
        options,
    };
    let after_on_disk = !matches!(options.mode, Mode::Staged);
    let mut complete = true;
    let mut files = Vec::new();
    for entry in &entries {
        let (json, file_complete) = run.file(entry, after_on_disk, full)?;
        complete &= file_complete;
        files.push(json);
    }
    let mode = match options.mode {
        Mode::Staged => "staged",
        Mode::Worktree => "worktree",
        Mode::Base(_) => "base",
        Mode::Full => "full",
    };
    let not_covered: Vec<String> = if include {
        Vec::new()
    } else {
        untracked
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect()
    };
    let json = Json::Obj(vec![
        ("tool", "rotter".into()),
        ("schema", "rotter.extract.poc/0".into()),
        ("stage", "extraction".into()),
        ("semantic_verification", "not_performed".into()),
        ("mode", mode.into()),
        ("repository", top.to_string_lossy().into_owned().into()),
        (
            "before",
            if full {
                Json::Null
            } else {
                Json::Obj(vec![
                    ("rev", rev.into()),
                    ("commit", commit.clone().into()),
                    ("empty_initial", commit.is_none().into()),
                ])
            },
        ),
        ("pathspec", options.paths.clone().into()),
        (
            "after",
            (if after_on_disk { "worktree" } else { "index" }).into(),
        ),
        (
            "untracked",
            Json::Obj(vec![
                ("included", include.into()),
                ("not_covered", not_covered.into()),
            ]),
        ),
        ("complete", complete.into()),
        ("files", Json::Arr(files)),
    ]);
    Ok(Report { json, complete })
}

#[cfg(test)]
mod tests {
    use super::{Change, Hunk, Scratch, changes, diff, parse_raw, temp_root, write_new};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rotter-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::canonicalize(path).unwrap()
    }

    #[test]
    fn write_helper_never_follows_a_planted_symlink() {
        let root = temp("planted");
        let canary = root.join("canary");
        fs::write(&canary, "canary").unwrap();
        let dir = root.join("diff-0");
        fs::create_dir(&dir).unwrap();
        symlink(&canary, dir.join("before")).unwrap();
        let error = write_new(&dir, "before", b"payload").unwrap_err();
        assert!(error.contains("exists"), "{error}");
        assert_eq!(fs::read_to_string(&canary).unwrap(), "canary");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn each_diff_uses_a_fresh_subdirectory_removed_afterwards() {
        let scratch = Scratch::new().unwrap();
        let hunks = diff(&scratch, "a\n", "b\n").unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            fs::read_dir(&scratch.dir).unwrap().count(),
            0,
            "diff-0 removed"
        );
        fs::create_dir(scratch.dir.join("diff-1")).unwrap();
        let error = diff(&scratch, "a\n", "b\n").unwrap_err();
        assert!(error.contains("diff-1"), "{error}");
        assert_eq!(diff(&scratch, "a\n", "a\nb\n").unwrap().len(), 1, "diff-2");
    }

    #[test]
    fn unsafe_or_colon_temp_roots_are_refused() {
        let root = temp("roots");
        assert_eq!(temp_root(&root).unwrap(), root);
        let shared = root.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
        let error = temp_root(&shared).unwrap_err();
        assert!(
            error.contains("TMPDIR") && error.contains("refused"),
            "{error}"
        );
        let colon = root.join("a:b");
        fs::create_dir(&colon).unwrap();
        let error = temp_root(&colon).unwrap_err();
        assert!(error.contains("TMPDIR") && error.contains("':'"), "{error}");
        // A link without ':' whose target has one is refused too: the resolved path counts.
        symlink(&colon, root.join("plain")).unwrap();
        assert!(temp_root(&root.join("plain")).unwrap_err().contains("':'"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn raw_records_keep_both_rename_paths() {
        let raw = b":100644 100644 aaaa bbbb R090\0old name.go\0new name.go\0:000000 100644 0000 cccc A\0x.lua\0";
        let entries = parse_raw(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, "R090");
        assert_eq!(entries[0].old.as_ref().unwrap().0, b"old name.go");
        assert_eq!(entries[0].new.as_ref().unwrap().0, b"new name.go");
        assert!(entries[1].old.is_none());
        assert_eq!(entries[1].new.as_ref().unwrap().2, "cccc");
    }

    #[test]
    fn zero_count_hunks_become_gaps() {
        let hunks = [Hunk {
            before: (5, 0),
            after: (6, 2),
        }];
        assert_eq!(changes(&hunks, false), [Change::Gap(5)]);
        assert_eq!(changes(&hunks, true), [Change::Rows(5..7)]);
    }
}
