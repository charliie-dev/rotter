use crate::config::{Kind, Sources, Untrusted, lstat, resolve_trusted, trusted_file, user};
use crate::grammar::{create_private, open_regular};
use crate::json::Json;
use crate::{
    Change, Detected, Grammar, Languages, ParseError, error_lines, full_units, parse_with_deadline,
    units,
};
use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
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

fn call(
    git: &Git,
    dir: &Path,
    args: &[&OsStr],
    input: Option<&[u8]>,
    ok: &[i32],
    guard: &[String],
) -> Result<Vec<u8>, String> {
    git_in(git, dir, args, input, ok, Private::Nothing, guard)
}

/// Oldest git rotter reads a repository with: `safe.bareRepository` exists from 2.38 and the
/// `.gitattributes` overflow CVE-2022-23521 is fixed in 2.39.1.
const MIN_GIT: (u32, u32, u32) = (2, 39, 1);
/// First git whose `GIT_NO_LAZY_FETCH` rotter relies on; older ones get the promisor gate.
const LAZY_FETCH_GIT: (u32, u32, u32) = (2, 45, 1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitClass {
    BelowMinimum,
    /// Partial clones and promisor remotes are refused.
    Gated,
    LazyFetch,
}

/// Classifies `git version` output by its leading numeric `X.Y.Z`; other suffixes are ignored,
/// except that `.rc`/`-rc` ranks below the release. Anything without the triple is too old.
pub(crate) fn classify(output: &str) -> GitClass {
    let Some(mut rest) = output.strip_prefix("git version ") else {
        return GitClass::BelowMinimum;
    };
    let mut triple = [0u32; 3];
    for (index, part) in triple.iter_mut().enumerate() {
        if index > 0 {
            let Some(after) = rest.strip_prefix('.') else {
                return GitClass::BelowMinimum;
            };
            rest = after;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(value) = rest[..digits].parse() else {
            return GitClass::BelowMinimum;
        };
        *part = value;
        rest = &rest[digits..];
    }
    let release = !(rest.starts_with(".rc") || rest.starts_with("-rc"));
    let version = (triple[0], triple[1], triple[2], release);
    let at = |(x, y, z): (u32, u32, u32)| (x, y, z, true);
    if version < at(MIN_GIT) {
        GitClass::BelowMinimum
    } else if version < at(LAZY_FETCH_GIT) {
        GitClass::Gated
    } else {
        GitClass::LazyFetch
    }
}

/// A filesystem object's identity, `(st_dev, st_ino)`: the same directory under any spelling
/// (symlinks, firmlinks, case variants on a case-insensitive volume).
type Identity = (u64, u64);

fn identity(path: &Path) -> io::Result<Identity> {
    let meta = fs::metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

/// Whether `dir` or any directory above it is one of `roots`; an ancestor that cannot be
/// stat'ed counts as one.
fn inside(dir: &Path, roots: &[Identity]) -> bool {
    dir.ancestors().any(|ancestor| match identity(ancestor) {
        Ok(id) => roots.contains(&id),
        Err(_) => true,
    })
}

/// The trees no git may come from for a run in some directory.
pub(crate) struct Walk {
    /// The topmost physical ancestor of the directory (itself included) that holds a `.git`
    /// entry.
    pub(crate) repository: Option<PathBuf>,
    /// Identities of the physical directory and `repository`.
    roots: Vec<Identity>,
}

/// Resolves `cwd` physically and walks its parents for `.git` entries (file or directory; an
/// entry that cannot be checked counts as present). No git runs.
pub(crate) fn walk(cwd: &Path) -> Result<Walk, String> {
    let fail = |error: io::Error| format!("cannot resolve {}: {error}", cwd.display());
    let physical = fs::canonicalize(cwd).map_err(fail)?;
    let repository = physical
        .ancestors()
        .filter(|dir| match fs::symlink_metadata(dir.join(".git")) {
            Ok(_) => true,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        })
        .last()
        .map(Path::to_owned);
    let mut roots = vec![identity(&physical).map_err(fail)?];
    if let Some(top) = &repository {
        roots.push(identity(top).map_err(fail)?);
    }
    Ok(Walk { repository, roots })
}

/// Mach-O (either byte order, 32 or 64 bit) or a universal binary whose big-endian `nfat_arch`
/// is 1..=20 (a Java class file shares `ca fe ba be` and has its version, ≥ 45, there).
fn macos_native(header: &[u8]) -> bool {
    match header {
        [0xcf, 0xfa, 0xed, 0xfe, ..]
        | [0xce, 0xfa, 0xed, 0xfe, ..]
        | [0xfe, 0xed, 0xfa, 0xcf, ..]
        | [0xfe, 0xed, 0xfa, 0xce, ..] => true,
        [0xca, 0xfe, 0xba, 0xbe, a, b, c, d, ..] => {
            (1..=20).contains(&u32::from_be_bytes([*a, *b, *c, *d]))
        }
        _ => false,
    }
}

fn linux_native(header: &[u8]) -> bool {
    header.starts_with(b"\x7fELF")
}

/// Whether the first bytes of a file are this OS's native executable magic; `#!` scripts and
/// anything else are not.
fn native(header: &[u8]) -> bool {
    if cfg!(target_os = "macos") {
        macos_native(header)
    } else {
        linux_native(header)
    }
}

/// `<dir>/git` as a candidate: the resolved target of a [`trusted_file`], named `git`, outside
/// `roots`, and a native executable by its magic, read through [`open_regular`] with owner and
/// mode taken from that descriptor. None when there is no `git` there; an error names why this
/// one is skipped. Nothing is run to classify it.
fn candidate(dir: &Path, roots: &[Identity]) -> Result<Option<PathBuf>, String> {
    let path = dir.join("git");
    let resolved = match trusted_file(&path, user(), &lstat) {
        Ok(resolved) => resolved,
        Err(Untrusted::Missing) => return Ok(None),
        Err(Untrusted::Refused(why)) => return Err(format!("{}: {why}", path.display())),
    };
    let skip = |why: &str| {
        Err(format!(
            "{} ({}): {why}",
            path.display(),
            resolved.display()
        ))
    };
    if resolved.file_name() != Some(OsStr::new("git")) {
        return skip(
            "resolves to a program not named git (a dispatcher such as a version-manager shim)",
        );
    }
    if resolved.parent().is_none_or(|parent| inside(parent, roots)) {
        return skip("inside the repository");
    }
    let mut file = match open_regular(&resolved) {
        Ok(Some(file)) => file,
        Ok(None) => return skip("vanished"),
        Err(why) => return skip(&why),
    };
    let meta = match file.metadata() {
        Ok(meta) => meta,
        Err(error) => return skip(&error.to_string()),
    };
    if !meta.is_file()
        || (meta.uid() != user() && meta.uid() != 0)
        || meta.mode() & 0o022 != 0
        || meta.mode() & 0o111 == 0
    {
        return skip("not an executable owned by you or root that others cannot write");
    }
    let mut header = Vec::new();
    if let Err(error) = Read::by_ref(&mut file).take(8).read_to_end(&mut header) {
        return skip(&error.to_string());
    }
    if !native(&header) {
        return skip("not a native executable (a script or wrapper)");
    }
    Ok(Some(resolved))
}

/// The first usable `git` on `path` and the usable entries: absolute, through
/// [`resolve_trusted`], directories, not inside `roots` and without `:` once resolved.
fn select(path: Option<&OsStr>, roots: &[Identity]) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let mut program = None;
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut skipped = Vec::new();
    for entry in path.map(std::env::split_paths).into_iter().flatten() {
        if !entry.is_absolute() {
            continue;
        }
        let Ok(resolved) = resolve_trusted(&entry, user(), &lstat) else {
            continue;
        };
        if lstat(&resolved).map_or(true, |meta| meta.kind != Kind::Dir)
            || inside(&resolved, roots)
            || resolved.as_os_str().as_bytes().contains(&b':')
        {
            continue;
        }
        if program.is_none() {
            match candidate(&resolved, roots) {
                Ok(found) => program = found,
                Err(why) => skipped.push(why),
            }
        }
        entries.push(resolved);
    }
    match program {
        Some(program) => Ok((program, entries)),
        None if skipped.is_empty() => {
            Err("cannot find git on the trusted absolute entries of PATH".to_owned())
        }
        None => Err(format!(
            "cannot find a usable git on the trusted absolute entries of PATH; skipped {}",
            skipped.join("; ")
        )),
    }
}

/// The one git a run uses, chosen before any git call, with the environment its children get
/// and the private scratch directory that is their TMPDIR.
pub struct Git {
    /// The resolved path, spawned exactly.
    program: PathBuf,
    /// The usable PATH entries, the PATH of an isolated child.
    entries: Vec<PathBuf>,
    /// Some in hook mode: children get only the allowlist, with this HOME.
    isolated: Option<Option<PathBuf>>,
    scratch: Scratch,
    /// `git version`, asked once, outside any repository.
    version: Result<(GitClass, String), String>,
}

impl Git {
    /// Picks the git for a run outside `walk`'s trees; `sources` gives PATH (and, `isolated`,
    /// HOME).
    /// The scratch directory already exists, so even the version probe gets it as TMPDIR.
    pub(crate) fn new(
        walk: &Walk,
        scratch: Scratch,
        sources: &Sources,
        isolated: bool,
    ) -> Result<Self, String> {
        let (program, entries) =
            select(sources.path.as_deref(), &walk.roots).map_err(|why| match &walk.repository {
                Some(repository) => format!(
                    "{why} (entries inside the repository {} are not used)",
                    repository.display()
                ),
                None => why,
            })?;
        let mut git = Self {
            program,
            entries,
            isolated: isolated.then(|| sources.home.clone()),
            scratch,
            version: Err(String::new()),
        };
        git.version = git
            .command()
            .arg("version")
            .current_dir("/")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run git: {error}"))
            .map(|output| {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                (classify(&text), text)
            });
        Ok(git)
    }

    /// The git for a CLI run in `dir`, in the inherited environment (TMPDIR aside).
    pub fn cli(dir: &Path) -> Result<Self, String> {
        let sources = Sources::from_env();
        let walk = walk(dir)?;
        Self::new(&walk, Scratch::new(&sources.temp)?, &sources, false)
    }

    /// The resolved path and its `git version` text, or why that git is not used, for `status`.
    pub(crate) fn summary(&self) -> (String, String) {
        let detail = match self.class() {
            Ok((_, version)) => version.to_owned(),
            Err(why) => why,
        };
        (self.program.display().to_string(), detail)
    }

    /// The class and `git version` text; below the minimum (or unrecognised) is an error.
    pub(crate) fn class(&self) -> Result<(GitClass, &str), String> {
        let (class, version) = self.version.as_ref().map_err(Clone::clone)?;
        if *class == GitClass::BelowMinimum {
            return Err(format!(
                "git reports {version:?}; rotter needs git 2.39.1 or newer to read repositories \
                 safely; put a newer git earlier on PATH"
            ));
        }
        Ok((*class, version))
    }

    /// `git` with the run's environment: TMPDIR is always the scratch directory; isolated, the
    /// environment is cleared first and gets only PATH (the usable entries), HOME, `LANG=C` and
    /// `LC_ALL=C`, and the child starts in `/` (every call names its directory with `-C`). No
    /// loader variables, `DEVELOPER_DIR`, `SDKROOT`, `TOOLCHAINS`, XDG_* or inherited GIT_*.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        if let Some(home) = &self.isolated {
            command.env_clear().current_dir("/");
            if let Ok(path) = std::env::join_paths(&self.entries) {
                command.env("PATH", path);
            }
            if let Some(home) = home {
                command.env("HOME", home);
            }
            command.env("LANG", "C").env("LC_ALL", "C");
        }
        command.env("TMPDIR", &self.scratch.dir);
        command
    }

    /// `rev-parse --show-toplevel` for `dir` with a time limit, for the install work-tree probe:
    /// Ok(None) only for a clean "not a git repository", Ok(Some(top)) inside a work tree, and
    /// an error for anything else (a failure, a timeout, non-UTF-8 output).
    pub(crate) fn work_tree(&self, dir: &Path) -> Result<Option<PathBuf>, String> {
        self.class()?;
        let args = ["rev-parse", "--show-toplevel"].map(OsStr::new);
        let mut child = self
            .prepare(dir, &args, false, Private::Nothing, &[])
            .spawn()
            .map_err(|error| format!("cannot run git: {error}"))?;
        let started = Instant::now();
        while child
            .try_wait()
            .map_err(|error| format!("git failed: {error}"))?
            .is_none()
        {
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git rev-parse --show-toplevel timed out".to_owned());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("git failed: {error}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        match output.status.code() {
            Some(0) => String::from_utf8(output.stdout)
                .map(|top| Some(PathBuf::from(top.trim_end())))
                .map_err(|_| "git printed non-UTF-8 output".to_owned()),
            // Apple's git stub may print xcrun warnings first.
            Some(128)
                if stderr
                    .lines()
                    .any(|line| line.starts_with("fatal: not a git repository")) =>
            {
                Ok(None)
            }
            _ => Err(format!(
                "git rev-parse --show-toplevel failed: {}",
                stderr.trim()
            )),
        }
    }

    /// The hardened git command for one call in `dir`, with `private` (see [`Private`]) and
    /// `guard`, the repository's filter and hook resets from [`guard_args`].
    fn prepare(
        &self,
        dir: &Path,
        args: &[&OsStr],
        input: bool,
        private: Private<'_>,
        guard: &[String],
    ) -> Command {
        let mut command = self.command();
        match private {
            Private::Nothing => {}
            Private::Index(index) => {
                // An unsplit private index keeps git from writing sharedindex files into
                // $GIT_DIR.
                command
                    .env("GIT_INDEX_FILE", index)
                    .args(["-c", "core.splitIndex=false"]);
            }
            Private::Ceiling(ceiling) => {
                command.env("GIT_CEILING_DIRECTORIES", ceiling);
            }
        }
        command
            .arg("-C")
            .arg(dir)
            .args(["--no-pager", "-c", "core.fsmonitor=false"])
            // Repository config must not run commands: no implicitly found bare repository, no
            // $GIT_DIR/hooks, and no index write (what fires index hooks, file or config based).
            .args([
                "-c",
                "safe.bareRepository=explicit",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "diff.autoRefreshIndex=false",
            ])
            .args(guard)
            .args(args)
            // Never write the index opportunistically; the checked repository stays untouched.
            .env("GIT_OPTIONAL_LOCKS", "0")
            // A missing object is an error, never a promisor fetch (honoured from git 2.45.1).
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("ROTTER_EMPTY_VALUE", "")
            .stdin(if input { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Stops when `top` (a repository top level git reported) holds the chosen git or one of the
    /// PATH entries its children get: git may place the work tree elsewhere than the `.git` walk
    /// found (GIT_DIR, core.worktree, a gitfile).
    fn outside(&self, top: &Path) -> Result<(), String> {
        let id = identity(top).map_err(|error| format!("{}: {error}", top.display()))?;
        let dirs = self
            .program
            .parent()
            .into_iter()
            .chain(self.entries.iter().map(PathBuf::as_path));
        match dirs.into_iter().find(|dir| inside(dir, &[id])) {
            Some(dir) => Err(format!(
                "refusing to run git: {} is inside the repository {}",
                dir.display(),
                top.display()
            )),
            None => Ok(()),
        }
    }
}

/// What a git call is pointed at besides the repository.
#[derive(Clone, Copy)]
enum Private<'a> {
    Nothing,
    /// A private index copy, so index refreshes never touch the repository.
    Index(&'a Path),
    /// A ceiling that stops repository discovery.
    Ceiling(&'a Path),
}

/// Runs one git call of the run (see [`Git::prepare`]); an exit code outside `ok` is an error.
fn git_in(
    git: &Git,
    dir: &Path,
    args: &[&OsStr],
    input: Option<&[u8]>,
    ok: &[i32],
    private: Private<'_>,
    guard: &[String],
) -> Result<Vec<u8>, String> {
    let mut child = git
        .prepare(dir, args, input.is_some(), private, guard)
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

fn git_text(git: &Git, dir: &Path, guard: &[String], args: &[&str]) -> Result<String, String> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let output = call(git, dir, &args, None, &[0], guard)?;
    String::from_utf8(output)
        .map(|text| text.trim_end().to_owned())
        .map_err(|_| "git printed non-UTF-8 output".to_owned())
}

/// The repository's top-level directory as git reports it; git's version is checked first, and
/// a top level holding the chosen git or its PATH entries stops the run.
pub fn toplevel(git: &Git, dir: &Path) -> Result<PathBuf, String> {
    git.class()?;
    let top = PathBuf::from(git_text(git, dir, &[], &["rev-parse", "--show-toplevel"])?);
    git.outside(&top)?;
    Ok(top)
}

/// `git config --null --name-only --get-regexp <pattern>` in `top`: the matching keys, empty
/// when none (exit 1); any other failure is an error.
fn config_keys(git: &Git, top: &Path, pattern: &str) -> Result<Vec<u8>, String> {
    let args = ["config", "--null", "--name-only", "--get-regexp", pattern].map(OsStr::new);
    git_in(git, top, &args, None, &[0, 1], Private::Nothing, &[])
}

/// Refuses partial clones and promisor remotes, by key presence: git before 2.45.1 may fetch a
/// missing object through repository-configured commands. Keys are canonical (lower-case
/// section and variable); the optional middle also catches a nameless `remote.promisor`.
fn promisor_gate(git: &Git, top: &Path, version: &str) -> Result<(), String> {
    let keys = config_keys(
        git,
        top,
        r"^(extensions\.partialclone|remote\.(.*\.)?(promisor|partialclonefilter))$",
    )
    .map_err(|error| format!("cannot check for a partial clone: {error}"))?;
    if keys.is_empty() {
        return Ok(());
    }
    Err(format!(
        "refusing to read {}: it is a partial clone or has a promisor remote, and {version} \
         (older than 2.45.1) could fetch missing objects through repository-configured \
         commands; put git 2.45.1 or newer earlier on PATH",
        top.display()
    ))
}

/// `--config-env` resets from `git config --null --name-only` keys under `filter.` and `hook.`:
/// every filter driver name gets empty `clean`, `smudge`, `process` and `required`, every hook
/// name with an `event` key empty events. The name is the text up to the last dot, so `=` and
/// `.` in it survive (git splits `--config-env` at the last `=`). A middle-less key is skipped,
/// except `hook.event`, which refuses: git 2.54 builds a hook from it.
fn guard_args(keys: &[u8]) -> Result<Vec<String>, String> {
    let keys = std::str::from_utf8(keys).map_err(|_| "non-UTF-8 filter or hook name")?;
    let mut filters: Vec<&str> = Vec::new();
    let mut hooks: Vec<&str> = Vec::new();
    for key in keys.split('\0').filter(|key| !key.is_empty()) {
        if let Some(rest) = key.strip_prefix("filter.") {
            if let Some((name, _)) = rest.rsplit_once('.')
                && !filters.contains(&name)
            {
                filters.push(name);
            }
        } else if let Some(rest) = key.strip_prefix("hook.") {
            match rest.rsplit_once('.') {
                Some((name, "event")) if !hooks.contains(&name) => hooks.push(name),
                Some(_) => {}
                None if rest == "event" => {
                    return Err("the repository config has a hook.event key".to_owned());
                }
                None => {}
            }
        }
    }
    let reset = |key: String| format!("--config-env={key}=ROTTER_EMPTY_VALUE");
    Ok(filters
        .iter()
        .flat_map(|name| {
            ["clean", "smudge", "process", "required"]
                .map(|var| reset(format!("filter.{name}.{var}")))
        })
        .chain(hooks.iter().map(|name| reset(format!("hook.{name}.event"))))
        .collect())
}

/// Whether a working-tree `M` entry differs from its before blob only in stat data (git no
/// longer checks: diff.autoRefreshIndex=false). The disk file is read through rotter's own
/// no-follow descriptor and hashed from those bytes without filters; any doubt keeps the entry.
fn stat_only(git: &Git, top: &Path, entry: &Entry, guard: &[String]) -> bool {
    let (Some((_, old_mode, old_oid)), Some((path, new_mode, new_oid))) = (&entry.old, &entry.new)
    else {
        return false;
    };
    if entry.status != "M"
        || !is_zero(new_oid)
        || old_mode != new_mode
        || !matches!(old_mode.as_str(), "100644" | "100755")
    {
        return false;
    }
    let Ok(Some(mut file)) = open_regular(&top.join(OsStr::from_bytes(path))) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return false;
    }
    let args = ["hash-object", "--no-filters", "--stdin"].map(OsStr::new);
    call(git, top, &args, Some(&bytes), &[0], guard)
        .is_ok_and(|out| out.trim_ascii() == old_oid.as_bytes())
}

/// Private scratch directory under the trusted temp root for the index copy and diff inputs,
/// and every git child's TMPDIR; removed on drop.
pub(crate) struct Scratch {
    dir: PathBuf,
    /// The resolved temp root: the ceiling for `git diff --no-index`.
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
    /// A fresh directory under the temp root `temp` (`Sources::temp`).
    pub(crate) fn new(temp: &Path) -> Result<Self, String> {
        let root = temp_root(temp)?;
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
fn diff(git: &Git, before: &str, after: &str) -> Result<Vec<Hunk>, String> {
    let dir = git.scratch.diff_dir()?;
    let output = diff_in(git, &dir, &git.scratch.root, before, after);
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
fn diff_in(
    git: &Git,
    dir: &Path,
    ceiling: &Path,
    before: &str,
    after: &str,
) -> Result<Vec<u8>, String> {
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
    git_in(
        git,
        dir,
        &args,
        None,
        &[0, 1],
        Private::Ceiling(ceiling),
        &[],
    )
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
    git: &'a Git,
    options: &'a Options,
    guard: &'a [String],
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
            Source::Blob(oid) => call(
                self.git,
                self.top,
                &[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(oid)],
                None,
                &[0],
                self.guard,
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
            Source::Disk => call(
                self.git,
                self.top,
                &["hash-object", "--no-filters", "--stdin"].map(OsStr::new),
                Some(&bytes),
                &[0],
                self.guard,
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
            diff(self.git, &text(&sides[0]), &text(&sides[1]))?
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

/// Extracts changed units and their comments for one explicitly chosen diff mode, with the git
/// [`Git::cli`] picks for `dir`.
pub fn extract(dir: &Path, options: &Options) -> Result<Report, String> {
    extract_with(&Git::cli(dir)?, dir, options)
}

/// [`extract`] with an already chosen git; one extraction per [`Git`] (its scratch directory
/// holds the index copy).
pub fn extract_with(git: &Git, dir: &Path, options: &Options) -> Result<Report, String> {
    let top = toplevel(git, dir)?;
    // Before any object is read: the promisor gate for older git, then the filter and hook
    // resets that every later call carries.
    let (class, version) = git.class()?;
    if class == GitClass::Gated {
        promisor_gate(git, &top, version)?;
    }
    let guard = config_keys(git, &top, r"^(filter|hook)\.")
        .and_then(|keys| guard_args(&keys))
        .map_err(|error| format!("refusing to read {}: {error}", top.display()))?;
    let guard = guard.as_slice();
    let (rev, commit) = match &options.mode {
        Mode::Base(rev) => {
            if rev.is_empty() || rev.starts_with('-') {
                return Err(format!("invalid base revision: {rev:?}"));
            }
            let commit = git_text(
                git,
                &top,
                guard,
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
        Mode::Staged | Mode::Worktree | Mode::Full => {
            // "No commits yet" means HEAD does not resolve; a HEAD naming a commit that cannot
            // be read (e.g. missing from a partial clone) is an error, not an empty snapshot.
            let commit = match git_text(
                git,
                &top,
                guard,
                &["rev-parse", "--verify", "--quiet", "HEAD"],
            ) {
                Ok(_) => Some(
                    git_text(
                        git,
                        &top,
                        guard,
                        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
                    )
                    .map_err(|_| "HEAD does not name a readable commit".to_owned())?,
                ),
                Err(_) => None,
            };
            ("HEAD".to_owned(), commit)
        }
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
            String::from_utf8_lossy(&call(git, &top, &args, Some(b""), &[0], guard)?)
                .trim()
                .to_owned()
        }
    };
    // `git diff <tree>` would refresh stat data and rewrite the index even with
    // GIT_OPTIONAL_LOCKS=0 (now off through diff.autoRefreshIndex), so every index read also
    // goes through a private copy.
    let index = git.scratch.dir.join("index");
    let real_index = top.join(git_text(
        git,
        &top,
        guard,
        &["rev-parse", "--git-path", "index"],
    )?);
    copy_index(&real_index, &index)?;
    let index = Private::Index(&index);
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
        // No `git status` inside submodules, which would run with their own config.
        "--ignore-submodules=dirty",
        tree.as_str(),
    ]);
    let mut entries = if full {
        let listed = git_in(
            git,
            dir,
            &with_specs(&["ls-files", "-s", "-z", "--full-name"]),
            None,
            &[0],
            index,
            guard,
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
        let mut entries = parse_raw(&git_in(
            git,
            dir,
            &args_with(&args, &specs),
            None,
            &[0],
            index,
            guard,
        )?)?;
        if matches!(options.mode, Mode::Worktree | Mode::Base(_)) {
            entries.retain(|entry| !stat_only(git, &top, entry, guard));
        }
        entries
    };

    // HEAD→disk diffs report conflicted paths as plain modifications; keep them visible.
    let unmerged_args = with_specs(&["ls-files", "--unmerged", "-z", "--full-name"]);
    let mut unmerged: Vec<Vec<u8>> = git_in(git, dir, &unmerged_args, None, &[0], index, guard)?
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
    let untracked: Vec<Vec<u8>> = git_in(git, dir, &untracked_args, None, &[0], index, guard)?
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
        git,
        options,
        guard,
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
    use super::{
        Change, Git, GitClass, Hunk, changes, classify, diff, guard_args, identity, linux_native,
        macos_native, parse_raw, select, temp_root, walk, write_new,
    };
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

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
        let root = temp("diffs");
        let git = Git::cli(&root).unwrap();
        let hunks = diff(&git, "a\n", "b\n").unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            fs::read_dir(&git.scratch.dir).unwrap().count(),
            0,
            "diff-0 removed"
        );
        fs::create_dir(git.scratch.dir.join("diff-1")).unwrap();
        let error = diff(&git, "a\n", "b\n").unwrap_err();
        assert!(error.contains("diff-1"), "{error}");
        assert_eq!(diff(&git, "a\n", "a\nb\n").unwrap().len(), 1, "diff-2");
        drop(git);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_magic_is_checked_per_os_with_fat_header_sanity() {
        let fat = |count: u32| [&[0xca, 0xfe, 0xba, 0xbe][..], &count.to_be_bytes()].concat();
        for magic in [
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xfe, 0xed, 0xfa, 0xce],
        ] {
            assert!(macos_native(&magic), "{magic:x?}");
            assert!(!linux_native(&magic), "{magic:x?}");
        }
        assert!(macos_native(&fat(1)) && macos_native(&fat(20)));
        assert!(!macos_native(&fat(0)) && !macos_native(&fat(21)));
        // A Java class file: `ca fe ba be`, minor 0, major 52.
        assert!(!macos_native(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52]));
        assert!(!macos_native(&[0xca, 0xfe, 0xba, 0xbe]), "no nfat_arch");
        assert!(linux_native(b"\x7fELF\x02\x01"));
        for other in [
            &b"\x7fELF"[..],
            b"#!/bin/sh\n",
            b"\xcf\xfa\xed",
            b"",
            b"MZ\x90\x00",
        ] {
            assert!(!macos_native(other), "{other:x?}");
        }
        for other in [&b"\x7fEL"[..], b"#!/usr/bin/env bash", &fat(2), b""] {
            assert!(!linux_native(other), "{other:x?}");
        }
    }

    /// `<dir>/git` holding `bytes` with `mode`.
    fn place(dir: &Path, bytes: &[u8], mode: u32) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let git = dir.join("git");
        fs::write(&git, bytes).unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(mode)).unwrap();
        git
    }

    fn joined(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn unusable_git_candidates_are_skipped_and_the_search_continues() {
        let root = temp("candidates");
        // This test binary is a native executable of this OS.
        let native = fs::read(std::env::current_exe().unwrap()).unwrap();
        let good = root.join("good");
        let real = place(&good, &native, 0o755);
        let cases: Vec<(&str, PathBuf)> = vec![
            (
                "script",
                place(
                    &root.join("script"),
                    b"#!/usr/bin/env bash\nexit 0\n",
                    0o755,
                ),
            ),
            ("short", place(&root.join("short"), b"\x7fE", 0o755)),
            (
                "unreadable",
                place(&root.join("unreadable"), &native, 0o311),
            ),
            (
                "not executable",
                place(&root.join("noexec"), &native, 0o644),
            ),
            (
                "group-writable",
                place(&root.join("shared"), &native, 0o775),
            ),
        ];
        // A dispatcher: a link named git to a native program with another name.
        let shim = root.join("shim");
        fs::create_dir_all(&shim).unwrap();
        fs::copy(&real, shim.join("mise")).unwrap();
        symlink(shim.join("mise"), shim.join("git")).unwrap();
        let mut dirs: Vec<PathBuf> = cases
            .iter()
            .map(|(_, git)| git.parent().unwrap().to_owned())
            .collect();
        dirs.push(shim.clone());
        let bad: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
        let mut all = bad.clone();
        all.push(&good);
        let (program, entries) = select(Some(&joined(&all)), &[]).unwrap();
        assert_eq!(program, real, "the first usable one, after every skip");
        assert_eq!(
            entries.len(),
            all.len(),
            "every entry stays on the child PATH"
        );
        let error = select(Some(&joined(&bad)), &[]).unwrap_err();
        for (name, git) in &cases {
            assert!(
                error.contains(&git.display().to_string()),
                "{name}: {error}"
            );
        }
        assert!(error.contains("not a native executable"), "{error}");
        assert!(error.contains("not named git"), "{error}");
        // Inside an excluded tree (by identity, the tree's root being an ancestor): the entry is
        // not even a candidate, nor on the child PATH.
        let other = temp("candidates-other");
        let fallback = place(&other.join("bin"), &native, 0o755);
        let path = joined(&[&good, &other.join("bin")]);
        let (program, entries) = select(Some(&path), &[identity(&root).unwrap()]).unwrap();
        assert_eq!((program, entries), (fallback, vec![other.join("bin")]));
        let error = select(Some(&joined(&[&good])), &[identity(&root).unwrap()]).unwrap_err();
        assert!(!error.contains("good"), "{error}");
        fs::remove_dir_all(other).unwrap();
        // Relative entries and a missing PATH find nothing.
        assert!(select(Some(&OsString::from("good")), &[]).is_err());
        assert!(select(None, &[]).is_err());
        fs::set_permissions(
            root.join("unreadable/git"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn excluded_roots_are_the_physical_cwd_and_the_topmost_git_ancestor() {
        let root = temp("walk");
        let inner = root.join("outer/inner/deep");
        fs::create_dir_all(&inner).unwrap();
        fs::create_dir(root.join("outer/.git")).unwrap();
        fs::write(root.join("outer/inner/.git"), "gitdir: x\n").unwrap();
        symlink(&inner, root.join("link")).unwrap();
        let found = walk(&root.join("link")).unwrap();
        assert_eq!(
            found.repository,
            Some(root.join("outer")),
            "topmost, physically"
        );
        assert_eq!(
            found.roots,
            [
                identity(&inner).unwrap(),
                identity(&root.join("outer")).unwrap()
            ]
        );
        let plain = root.join("plain");
        fs::create_dir(&plain).unwrap();
        assert_eq!(walk(&plain).unwrap().repository, None);
        assert!(walk(&root.join("missing")).is_err());
        fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn git_versions_classify_by_their_leading_triple() {
        let below = [
            "git version 2.37.9",
            "git version 2.38.0",
            "git version 2.38.2",
            "git version 2.39.0",
            "git version 2.39.1.rc1",
            "git version abc",
            "",
            "git version 2.45",
            "git version 2.31.0",
        ];
        for output in below {
            assert_eq!(classify(output), GitClass::BelowMinimum, "{output:?}");
        }
        let gated = [
            "git version 2.39.1",
            "git version 2.39.5 (Apple Git-154)",
            "git version 2.40.10",
            "git version 2.39.10",
            "git version 2.40.0.rc0",
            "git version 2.45.0",
            "git version 2.45.1.rc0",
            "git version 2.44.0-rc2",
        ];
        for output in gated {
            assert_eq!(classify(output), GitClass::Gated, "{output:?}");
        }
        let lazy = [
            "git version 2.45.1",
            "git version 2.46.0.windows.1",
            "git version 2.54.0 (Apple Git-157)",
            "git version 2.50.0.5.gdeadbee",
            "git version 3.0.0",
        ];
        for output in lazy {
            assert_eq!(classify(output), GitClass::LazyFetch, "{output:?}");
        }
    }

    #[test]
    fn guard_args_reset_exact_filter_and_hook_names() {
        let keys = b"filter.x=y.clean\0filter.a.b.process\0filter.X.smudge\0filter.x=y.required\0\
filter..clean\0filter.clean\0hook.Pre.Commit.event\0hook.Pre.Commit.command\0hook.noevent.command\0\
hook.command\0";
        let reset = |key: &str| format!("--config-env={key}=ROTTER_EMPTY_VALUE");
        let filter = |name: &str| {
            ["clean", "smudge", "process", "required"]
                .map(|var| reset(&format!("filter.{name}.{var}")))
        };
        let mut expected: Vec<String> = Vec::new();
        for name in ["x=y", "a.b", "X", ""] {
            expected.extend(filter(name));
        }
        expected.push(reset("hook.Pre.Commit.event"));
        assert_eq!(guard_args(keys).unwrap(), expected);
        assert_eq!(guard_args(b"").unwrap(), Vec::<String>::new());
        let error = guard_args(b"filter.x.clean\0hook.event\0").unwrap_err();
        assert!(error.contains("hook.event"), "{error}");
        assert!(
            guard_args(b"filter.\xff.clean\0").is_err(),
            "non-UTF-8 fails closed"
        );
    }
}
