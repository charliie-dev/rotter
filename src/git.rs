use crate::config::{Kind, Meta, Sources, Untrusted, lstat, resolve_trusted, trusted_file, user};
use crate::grammar::{create_private, open_regular, trusted_input};
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Set from another thread to end the run as if its deadline had passed.
    pub cancel: Option<Arc<AtomicBool>>,
    /// The top level a caller already checked; finding another one is an error before anything
    /// in it is read.
    pub expect_top: Option<PathBuf>,
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
            cancel: None,
            expect_top: None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
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
    if !trusted_input(Meta::of(&meta), user()) || meta.mode() & 0o111 == 0 {
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

/// The first usable `git` on `path`, the usable entries (absolute, through
/// [`resolve_trusted`], directories, not inside `roots` and without `:` once resolved) and why
/// each entry before that git was refused as untrusted (e.g. a group-writable Homebrew `bin`).
fn select(
    path: Option<&OsStr>,
    roots: &[Identity],
) -> Result<(PathBuf, Vec<PathBuf>, Vec<String>), String> {
    let mut program = None;
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut skipped = Vec::new();
    let mut refused = Vec::new();
    for entry in path.map(std::env::split_paths).into_iter().flatten() {
        if !entry.is_absolute() {
            continue;
        }
        let resolved = match resolve_trusted(&entry, user(), &lstat) {
            Ok(resolved) => resolved,
            Err(Untrusted::Refused(why)) => {
                if program.is_none() {
                    // `why` is "<refused path>: <reason>"; when that path is the entry itself,
                    // name it once. A link target or a directory above it is named separately.
                    let shown = entry.display().to_string();
                    refused.push(if why.starts_with(&format!("{shown}: ")) {
                        format!("PATH entry {why}")
                    } else {
                        format!("PATH entry {shown} ({why})")
                    });
                }
                continue;
            }
            Err(Untrusted::Missing) => continue,
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
        Some(program) => Ok((program, entries, refused)),
        None => {
            skipped.extend(refused);
            if skipped.is_empty() {
                Err("cannot find git on the trusted absolute entries of PATH".to_owned())
            } else {
                Err(format!(
                    "cannot find a usable git on the trusted absolute entries of PATH; skipped {}",
                    skipped.join("; ")
                ))
            }
        }
    }
}

/// The one git a run uses, chosen before any git call, with the environment its children get
/// and the private scratch directory that is their TMPDIR.
pub struct Git {
    /// The resolved path, spawned exactly.
    program: PathBuf,
    /// The usable PATH entries, the PATH of an isolated child.
    entries: Vec<PathBuf>,
    /// Why each PATH entry before `program` was refused as untrusted, for `status`.
    refused: Vec<String>,
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
        let (program, entries, refused) =
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
            refused,
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
        let mut detail = match self.class() {
            Ok((_, version)) => version.to_owned(),
            Err(why) => why,
        };
        if !self.refused.is_empty() {
            detail.push_str(&format!(
                "; untrusted PATH entries before it were skipped: {}",
                self.refused.join("; ")
            ));
        }
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

    /// `git` with the run's environment: TMPDIR is always the scratch directory. Isolated, the
    /// environment is cleared first and gets only PATH (the usable entries), HOME, `LANG=C` and
    /// `LC_ALL=C` (no loader variables, `DEVELOPER_DIR`, `SDKROOT`, `TOOLCHAINS`, XDG_* or
    /// inherited GIT_*), and the child starts in `/` (every call names its directory with `-C`).
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

    /// `rev-parse --show-toplevel` for `dir` within `limit`, for the install work-tree probe:
    /// Ok(None) only for a clean "not a git repository", Ok(Some(top)) inside a work tree, and
    /// an error for anything else (a failure, a timeout, non-UTF-8 output).
    pub(crate) fn work_tree(&self, dir: &Path, limit: Duration) -> Result<Option<PathBuf>, String> {
        self.class()?;
        let args = ["rev-parse", "--show-toplevel"].map(OsStr::new);
        let mut child = self
            .prepare(dir, &args, false, Private::Nothing, &[])
            .spawn()
            .map_err(|error| format!("cannot run git: {error}"))?;
        let deadline = Instant::now() + limit;
        // The deadline is checked before each poll, so a zero limit always times out, however
        // fast git is.
        loop {
            if Instant::now().checked_duration_since(deadline).is_some() {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git rev-parse --show-toplevel timed out".to_owned());
            }
            if child
                .try_wait()
                .map_err(|error| format!("git failed: {error}"))?
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("git failed: {error}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        match output.status.code() {
            Some(0) => String::from_utf8(output.stdout)
                .map(|top| Some(PathBuf::from(top.trim_end_matches('\n'))))
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
            // A missing object is an error, never a promisor fetch.
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
        .map(|text| text.trim_end_matches('\n').to_owned())
        .map_err(|_| "git printed non-UTF-8 output".to_owned())
}

pub fn git_dir(git: &Git, top: &Path) -> Result<PathBuf, String> {
    Ok(PathBuf::from(git_text(
        git,
        top,
        &[],
        &["rev-parse", "--absolute-git-dir"],
    )?))
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
/// `.` in it survive. A middle-less key is skipped, except `hook.event`, which refuses.
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

/// The disk path and before blob of a working-tree `M` entry that may differ from that blob only
/// in stat data (git no longer checks: diff.autoRefreshIndex=false).
fn stat_candidate(entry: &Entry) -> Option<(&[u8], &str)> {
    // Only a modification: for a rename git reads the file itself (detection needs its content),
    // so it reports the file's real blob, never the zero id of an unread one.
    if entry.status != "M" {
        return None;
    }
    let (Some((_, old_mode, old_oid)), Some((path, new_mode, new_oid))) = (&entry.old, &entry.new)
    else {
        return None;
    };
    // A blob git already hashed differs from the before blob: a real change.
    if !is_zero(new_oid) {
        return None;
    }
    if old_mode != new_mode {
        return None;
    }
    matches!(old_mode.as_str(), "100644" | "100755").then_some((path.as_slice(), old_oid.as_str()))
}

/// Paths per `git hash-object` call, well below the argument-length limit.
const HASH_BATCH: usize = 500;

/// Drops the entries that differ from their before blob only in stat data. Each candidate is read
/// through rotter's own no-follow descriptor into a private scratch copy, and the copies are
/// hashed without filters, many per git call; any doubt keeps the entry. Once `passed` (the run's
/// deadline or its cancellation) is true the remaining candidates are kept unchecked, so the run
/// still ends soon after.
fn drop_stat_only(
    git: &Git,
    top: &Path,
    entries: &mut Vec<Entry>,
    guard: &[String],
    passed: &dyn Fn() -> bool,
) -> Result<(), String> {
    let candidates: Vec<(usize, &str)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| stat_candidate(entry).map(|(_, oid)| (index, oid)))
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let dir = git.scratch.diff_dir()?;
    let mut dropped = vec![false; entries.len()];
    for batch in candidates.chunks(HASH_BATCH) {
        if passed() {
            break;
        }
        let mut copies: Vec<(usize, &str, PathBuf)> = Vec::new();
        for (index, oid) in batch.iter() {
            if passed() {
                break;
            }
            let Some((path, _)) = stat_candidate(&entries[*index]) else {
                continue;
            };
            let relative = Path::new(OsStr::from_bytes(path));
            if unsafe_disk_path(top, relative).is_some() {
                continue;
            }
            let Ok(Some(mut file)) = open_regular(&top.join(relative)) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file.read_to_end(&mut bytes).is_err() {
                continue;
            }
            let name = index.to_string();
            match write_new(&dir, &name, &bytes) {
                Ok(copy) => copies.push((*index, oid, copy)),
                // No copy (e.g. a full TMPDIR; a file-size limit gets here only while SIGXFSZ is
                // ignored, else it kills rotter): hash these bytes on their own, from stdin, which
                // needs no disk; any failure keeps the entry.
                Err(_) => {
                    let _ = fs::remove_file(dir.join(&name));
                    let args = ["hash-object", "--no-filters", "--stdin"].map(OsStr::new);
                    if call(git, top, &args, Some(&bytes), &[0], guard)
                        .is_ok_and(|out| out.trim_ascii() == oid.as_bytes())
                    {
                        dropped[*index] = true;
                    }
                }
            }
        }
        if copies.is_empty() {
            continue;
        }
        let args: Vec<&OsStr> = ["hash-object", "--no-filters", "--"]
            .iter()
            .map(OsStr::new)
            .chain(copies.iter().map(|(_, _, copy)| copy.as_os_str()))
            .collect();
        let Ok(out) = call(git, top, &args, None, &[0], guard) else {
            continue;
        };
        let hashes: Vec<&[u8]> = out.split(|byte| *byte == b'\n').collect();
        for (at, (index, oid, copy)) in copies.iter().enumerate() {
            let _ = fs::remove_file(copy);
            if hashes.get(at).is_some_and(|hash| *hash == oid.as_bytes()) {
                dropped[*index] = true;
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    let mut index = 0;
    entries.retain(|_| {
        index += 1;
        !dropped[index - 1]
    });
    Ok(())
}

/// Private scratch directory under the trusted temp root for the index copy and diff inputs,
/// and every git child's TMPDIR; removed on drop.
pub(crate) struct Scratch {
    dir: PathBuf,
    /// The resolved temp root: the ceiling for `git diff --no-index`.
    root: PathBuf,
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
        // `-U0` does not cover `diff.interHunkContext`, which would merge nearby hunks and
        // report the unchanged lines between them as changed.
        "--inter-hunk-context=0",
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

#[derive(Clone, Copy)]
enum Source<'a> {
    Blob(&'a str),
    Disk,
}

/// Why a side's bytes were not read; each maps to the side's status in [`Run::load`].
enum Unread {
    /// A symlink in the working tree: `skipped_symlink`.
    Symlink,
    /// The first line put the file out of scope: `not_in_scope`.
    NotInScope,
    /// The first line named an unsupported shell dialect: `unsupported_dialect`.
    Dialect(String),
    /// A read failure: `read_error`.
    Error(String),
}

fn unsupported(mut side: Side, dialect: String) -> Side {
    side.json.push(("dialect", dialect.into()));
    side.status(
        "unsupported_dialect",
        Some("only Bash is supported".into()),
        true,
    )
}

/// Adds a parsed side's status: `ok`, or `partial` naming up to 20 lines with syntax errors.
/// False when the side is partial.
fn parse_status(json: &mut Vec<(&'static str, Json)>, tree: &tree_sitter::Tree) -> bool {
    let errors = error_lines(tree);
    if errors.is_empty() {
        json.push(("status", "ok".into()));
        json.push(("detail", Json::Null));
        return true;
    }
    let lines: Vec<String> = errors.iter().take(20).map(ToString::to_string).collect();
    json.push(("status", "partial".into()));
    json.push((
        "detail",
        format!("syntax errors near lines {}", lines.join(", ")).into(),
    ));
    false
}

/// Why a parse stopped: the run's budget when it was shorter than the per-file limit.
fn timeout_detail(budget: Duration, limit: Duration) -> String {
    if budget < limit {
        "stopped: the run's time budget ran out during this file".to_owned()
    } else {
        format!(
            "stopped after {} s; the limit is parse_timeout_seconds in \
             $XDG_CONFIG_HOME/rotter/config.toml",
            limit.as_secs()
        )
    }
}

/// The report's `change` and `similarity` for a diff status letter (and its score).
fn change_kind(status: &str) -> (&'static str, Option<u32>) {
    match status.split_at(1) {
        ("M", _) => ("modified", None),
        ("A", _) => ("added", None),
        ("D", _) => ("deleted", None),
        ("R", score) => ("renamed", score.parse::<u32>().ok()),
        // No "C": `-M` in diff_args overrides diff.renames=copies, so git never reports a copy.
        ("T", _) => ("type_changed", None),
        ("U", _) => ("unmerged", None),
        ("?", _) => ("untracked_added", None),
        ("F", _) => ("full", None),
        _ => ("unknown", None),
    }
}

fn hunks_json(hunks: &[Hunk]) -> Json {
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
    )
}

fn first_line(bytes: &[u8]) -> String {
    let line = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    String::from_utf8_lossy(line).into_owned()
}

struct Run<'a> {
    top: &'a Path,
    git: &'a Git,
    options: &'a Options,
    guard: &'a [String],
}

impl Run<'_> {
    fn changed(
        &self,
        dir: &Path,
        specs: &[&OsStr],
        index: Private<'_>,
        tree: &str,
    ) -> Result<Vec<Entry>, String> {
        let (git, guard) = (self.git, self.guard);
        if matches!(self.options.mode, Mode::Full) {
            let args = args_with(&["ls-files", "-s", "-z", "--full-name"], specs);
            let listed = git_in(git, dir, &args, None, &[0], index, guard)?;
            return snapshot_entries(self.top, &listed);
        }
        let args = args_with(&diff_args(&self.options.mode, tree), specs);
        let mut entries = parse_raw(&git_in(git, dir, &args, None, &[0], index, guard)?)?;
        if matches!(self.options.mode, Mode::Worktree | Mode::Base(_)) {
            drop_stat_only(git, self.top, &mut entries, guard, &|| {
                self.deadline_passed()
            })?;
        }
        Ok(entries)
    }

    fn deadline_passed(&self) -> bool {
        self.options.cancelled()
            || self
                .options
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
        let mut detected = self.detect(&display, relative);
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
        let bytes = match self.read(relative, source, &detected) {
            Ok(bytes) => bytes,
            Err(Unread::Symlink) => return side.status("skipped_symlink", None, false),
            Err(Unread::NotInScope) => return side.status("not_in_scope", None, false),
            Err(Unread::Dialect(dialect)) => return unsupported(side, dialect),
            Err(Unread::Error(error)) => return side.status("read_error", Some(error), true),
        };
        if matches!(detected, Detected::NeedsContent) {
            detected = (self.options.grammars).detect_content(relative, &first_line(&bytes));
        }
        let (language, dialect) = match detected {
            Detected::Supported(language, dialect) => (language, dialect),
            Detected::UnsupportedDialect(dialect) => return unsupported(side, dialect),
            Detected::NotInScope | Detected::NeedsContent => {
                return side.status("not_in_scope", None, false);
            }
        };
        // Only once the side is known to be in scope (an extensionless name is decided by its
        // first line): `path` shows invalid bytes as U+FFFD, so two such names can read the same.
        // The side is not analysed, and `detail` carries the exact bytes, escaped.
        if std::str::from_utf8(path).is_err() {
            return side.status(
                "non_utf8_path",
                Some(format!("path bytes: {}", path.escape_ascii())),
                true,
            );
        }
        side.json.push(("language", language.name().into()));
        side.json.push(("dialect", dialect.into()));
        // The only place a grammar that cannot be loaded becomes a status.
        if let Err(detail) = language.language() {
            return side.status("parser_not_installed", Some(detail), true);
        }
        match self.blob_id(source, &bytes) {
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

    fn detect(&self, display: &str, relative: &Path) -> Detected {
        let override_language = self
            .options
            .languages
            .iter()
            .find(|(pattern, _, _)| crate::glob_match(pattern, display));
        match override_language {
            Some((_, grammar, dialect)) => {
                Detected::Supported(Arc::clone(grammar), dialect.clone())
            }
            None => self.options.grammars.detect_path(relative),
        }
    }

    fn read(
        &self,
        relative: &Path,
        source: Source<'_>,
        detected: &Detected,
    ) -> Result<Vec<u8>, Unread> {
        match source {
            Source::Blob(oid) => call(
                self.git,
                self.top,
                &[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(oid)],
                None,
                &[0],
                self.guard,
            )
            .map_err(Unread::Error),
            Source::Disk => self.read_disk(relative, detected),
        }
    }

    fn read_disk(&self, relative: &Path, detected: &Detected) -> Result<Vec<u8>, Unread> {
        if let Some(unread) = unsafe_disk_path(self.top, relative) {
            return Err(unread);
        }
        let full = self.top.join(relative);
        let meta = fs::symlink_metadata(&full).map_err(|error| Unread::Error(error.to_string()))?;
        if meta.file_type().is_symlink() {
            return Err(Unread::Symlink);
        }
        if !meta.is_file() {
            return Err(Unread::Error("not a regular file".to_owned()));
        }
        // One no-follow, non-blocking descriptor for every read, so an entry swapped for a
        // symlink or FIFO after the lstat above is never followed or waited on.
        let file = open_regular(&full).map_err(Unread::Error)?.ok_or_else(|| {
            Unread::Error("the file disappeared while it was being read".to_owned())
        })?;
        if matches!(detected, Detected::NeedsContent) {
            return self.read_deciding(&file, relative);
        }
        let mut bytes = Vec::new();
        (&file)
            .read_to_end(&mut bytes)
            .map_err(|error| Unread::Error(error.to_string()))?;
        Ok(bytes)
    }

    fn read_deciding(&self, file: &fs::File, relative: &Path) -> Result<Vec<u8>, Unread> {
        let mut first = Vec::new();
        file.take(4096)
            .read_to_end(&mut first)
            .map_err(|error| Unread::Error(error.to_string()))?;
        match (self.options.grammars).detect_content(relative, &first_line(&first)) {
            Detected::NotInScope => Err(Unread::NotInScope),
            Detected::UnsupportedDialect(dialect) => Err(Unread::Dialect(dialect)),
            _ => {
                let mut rest = file;
                rest.read_to_end(&mut first)
                    .map_err(|error| Unread::Error(error.to_string()))?;
                Ok(first)
            }
        }
    }

    fn blob_id(&self, source: Source<'_>, bytes: &[u8]) -> Result<String, String> {
        match source {
            Source::Blob(oid) => Ok(oid.to_owned()),
            Source::Disk => call(
                self.git,
                self.top,
                &["hash-object", "--no-filters", "--stdin"].map(OsStr::new),
                Some(bytes),
                &[0],
                self.guard,
            )
            .map(|out| String::from_utf8_lossy(&out).trim().to_owned()),
        }
    }

    fn file(&self, entry: &Entry, after_on_disk: bool, full: bool) -> Result<(Json, bool), String> {
        let before = entry
            .old
            .as_ref()
            .map(|(path, mode, oid)| self.load(path, mode, Source::Blob(oid)));
        let after = entry
            .new
            .as_ref()
            .map(|(path, mode, oid)| self.after_side(path, mode, oid, after_on_disk));
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
        let before = self.side_json(before, &hunks, false, full, &mut complete);
        let after = self.side_json(after, &hunks, true, full, &mut complete);
        let (change, similarity) = change_kind(&entry.status);
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
            ("hunks", hunks_json(&hunks)),
            ("before", before.into()),
            ("after", after.into()),
        ]);
        Ok((json, complete))
    }

    /// The after side: from disk, the index blob, or `unmerged` for a conflicted index entry
    /// (its oid is all zeros).
    fn after_side(&self, path: &[u8], mode: &str, oid: &str, on_disk: bool) -> Side {
        if on_disk {
            return self.load(path, mode, Source::Disk);
        }
        if is_zero(oid) {
            return Side {
                json: vec![("path", String::from_utf8_lossy(path).into_owned().into())],
                text: None,
                language: None,
                in_scope: true,
            }
            .status("unmerged", None, true);
        }
        self.load(path, mode, Source::Blob(oid))
    }

    fn side_json(
        &self,
        side: Option<Side>,
        hunks: &[Hunk],
        is_after: bool,
        full: bool,
        complete: &mut bool,
    ) -> Option<Json> {
        let mut side = side?;
        if let (Some(text), Some(language)) = (&side.text, side.language.clone()) {
            let limit = self.options.parse_timeout;
            let budget = self.options.deadline.map_or(limit, |deadline| {
                limit.min(deadline.saturating_duration_since(Instant::now()))
            });
            match parse_with_deadline(&language, text, budget) {
                Ok(tree) => {
                    // Keep what parsed; units touching an error are flagged, not dropped.
                    *complete &= parse_status(&mut side.json, &tree);
                    let found = if full {
                        full_units(&language, text, &tree)
                    } else {
                        units(&language, text, &tree, &changes(hunks, is_after))
                    };
                    side.json.push(("units", found));
                }
                Err(ParseError::Timeout) => {
                    *complete = false;
                    side.json.push(("status", "parse_timeout".into()));
                    side.json
                        .push(("detail", timeout_detail(budget, limit).into()));
                }
                Err(error) => {
                    *complete = false;
                    side.json.push(("status", "parse_error".into()));
                    side.json.push(("detail", error.to_string().into()));
                }
            }
        }
        Some(Json::Obj(side.json))
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
    if let Some(expected) = &options.expect_top
        && *expected != top
    {
        return Err(format!(
            "{} now belongs to {}, not the checked repository {}",
            dir.display(),
            top.display(),
            expected.display()
        ));
    }
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
    let (rev, commit) = before_commit(git, &top, guard, &options.mode)?;
    // Pathspecs are relative to `dir`, so commands that take them run there with top-level
    // output paths; `:/` means the whole repository.
    let specs: Vec<&OsStr> = if options.paths.is_empty() {
        vec![OsStr::new(":/")]
    } else {
        options.paths.iter().map(OsStr::new).collect()
    };
    let tree = match &commit {
        Some(commit) => commit.clone(),
        None => empty_tree(git, &top, guard)?,
    };
    // `git diff <tree>` would refresh stat data and rewrite the index even with
    // GIT_OPTIONAL_LOCKS=0. `prepare` turns that off with diff.autoRefreshIndex=false, and every
    // index read also goes through a private copy.
    let index = git.scratch.dir.join("index");
    let real_index = top.join(git_text(
        git,
        &top,
        guard,
        &["rev-parse", "--git-path", "index"],
    )?);
    copy_index(&real_index, &index)?;
    let index = Private::Index(&index);
    let run = Run {
        top: &top,
        git,
        options,
        guard,
    };
    let mut entries = run.changed(dir, &specs, index, &tree)?;
    // HEAD→disk diffs report conflicted paths as plain modifications; keep them visible.
    let unmerged_args = args_with(&["ls-files", "--unmerged", "-z", "--full-name"], &specs);
    let unmerged = git_in(git, dir, &unmerged_args, None, &[0], index, guard)?;
    mark_unmerged(&mut entries, &unmerged);

    let untracked_args = args_with(
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--full-name",
        ],
        &specs,
    );
    let untracked = paths(&git_in(
        git,
        dir,
        &untracked_args,
        None,
        &[0],
        index,
        guard,
    )?);
    let include = options.include_untracked && !matches!(options.mode, Mode::Staged);
    // An untracked nested repository is listed as one `dir/` record; its files belong to that
    // repository, so it is never read as a file and stays under `not_covered`.
    let nested = |path: &Vec<u8>| path.ends_with(b"/");
    if include {
        entries.extend(
            untracked
                .iter()
                .filter(|path| !nested(path))
                .map(|path| Entry {
                    status: "?".to_owned(),
                    old: None,
                    new: Some((path.clone(), "100644".to_owned(), String::new())),
                }),
        );
    }

    let after_on_disk = !matches!(options.mode, Mode::Staged);
    let full = matches!(options.mode, Mode::Full);
    let mut complete = true;
    let mut files = Vec::new();
    for entry in &entries {
        let (json, file_complete) = run.file(entry, after_on_disk, full)?;
        complete &= file_complete;
        files.push(json);
    }
    let not_covered: Vec<String> = untracked
        .iter()
        .filter(|path| !include || nested(path))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    let json = report_json(
        options,
        &top,
        (rev, commit),
        (include, not_covered),
        complete,
        files,
    );
    Ok(Report { json, complete })
}

fn args_with<'a>(args: &[&'a str], specs: &[&'a OsStr]) -> Vec<&'a OsStr> {
    args.iter()
        .map(|arg| OsStr::new(*arg))
        .chain([OsStr::new("--")])
        .chain(specs.iter().copied())
        .collect()
}

/// The before side: the revision it names and its commit, None when there are no commits yet.
fn before_commit(
    git: &Git,
    top: &Path,
    guard: &[String],
    mode: &Mode,
) -> Result<(String, Option<String>), String> {
    match mode {
        Mode::Base(rev) => {
            if rev.is_empty() || rev.starts_with('-') {
                return Err(format!("invalid base revision: {rev:?}"));
            }
            let commit = git_text(
                git,
                top,
                guard,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{rev}^{{commit}}"),
                ],
            )
            .map_err(|_| format!("cannot resolve base revision {rev:?}"))?;
            Ok((rev.clone(), Some(commit)))
        }
        Mode::Staged | Mode::Worktree | Mode::Full => {
            // "No commits yet" means HEAD does not resolve; a HEAD naming a commit that cannot
            // be read (e.g. missing from a partial clone) is an error, not an empty snapshot.
            let commit = match git_text(
                git,
                top,
                guard,
                &["rev-parse", "--verify", "--quiet", "HEAD"],
            ) {
                Ok(_) => Some(
                    git_text(
                        git,
                        top,
                        guard,
                        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
                    )
                    .map_err(|_| "HEAD does not name a readable commit".to_owned())?,
                ),
                Err(_) => None,
            };
            Ok(("HEAD".to_owned(), commit))
        }
    }
}

/// The empty tree's id: the before side of a repository without commits.
fn empty_tree(git: &Git, top: &Path, guard: &[String]) -> Result<String, String> {
    let args = ["hash-object", "-t", "tree", "--stdin"].map(OsStr::new);
    Ok(
        String::from_utf8_lossy(&call(git, top, &args, Some(b""), &[0], guard)?)
            .trim()
            .to_owned(),
    )
}

fn diff_args<'a>(mode: &Mode, tree: &'a str) -> Vec<&'a str> {
    let mut args = vec!["diff"];
    if matches!(mode, Mode::Staged) {
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
        tree,
    ]);
    args
}

/// Why a path the index lists must not be touched on disk, if it must not. Git refuses `..` and
/// paths through a symlinked directory, but an index written by other means can list them, and
/// every disk access to such a path (`read_disk`, `drop_stat_only`, `snapshot_entries`) would
/// leave the working tree. The component check comes first: a `..` path has no ancestors to test.
fn unsafe_disk_path(top: &Path, relative: &Path) -> Option<Unread> {
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Some(Unread::Error(
            "the index names a path outside the working tree".to_owned(),
        ));
    }
    through_symlink(top, relative).then_some(Unread::Symlink)
}

/// Whether a directory between the top level and `relative` is a symlink.
fn through_symlink(top: &Path, relative: &Path) -> bool {
    relative
        .ancestors()
        .skip(1)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| {
            fs::symlink_metadata(top.join(dir)).is_ok_and(|meta| meta.file_type().is_symlink())
        })
}

/// One `F` entry per tracked file still on disk, from `git ls-files -s -z` output.
fn snapshot_entries(top: &Path, listed: &[u8]) -> Result<Vec<Entry>, String> {
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
        // Tracked files deleted from the working tree are not part of this snapshot. A path that
        // must not be read is kept, for read_disk to report, so the report never shows whether
        // something exists where it leads.
        let relative = Path::new(OsStr::from_bytes(&path));
        let on_disk = unsafe_disk_path(top, relative).is_some()
            || fs::symlink_metadata(top.join(relative)).is_ok();
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
    Ok(entries)
}

fn mark_unmerged(entries: &mut Vec<Entry>, listed: &[u8]) {
    let mut unmerged: Vec<Vec<u8>> = listed
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
}

fn paths(listed: &[u8]) -> Vec<Vec<u8>> {
    listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// The `rotter.extract.poc/0` document. `before` is the revision and its commit; `untracked`
/// whether untracked files were included and which ones the report does not cover.
fn report_json(
    options: &Options,
    top: &Path,
    (rev, commit): (String, Option<String>),
    (include, not_covered): (bool, Vec<String>),
    complete: bool,
    files: Vec<Json>,
) -> Json {
    let mode = match options.mode {
        Mode::Staged => "staged",
        Mode::Worktree => "worktree",
        Mode::Base(_) => "base",
        Mode::Full => "full",
    };
    let before = if matches!(options.mode, Mode::Full) {
        Json::Null
    } else {
        Json::Obj(vec![
            ("rev", rev.into()),
            ("commit", commit.clone().into()),
            ("empty_initial", commit.is_none().into()),
        ])
    };
    let after = if matches!(options.mode, Mode::Staged) {
        "index"
    } else {
        "worktree"
    };
    Json::Obj(vec![
        ("tool", "rotter".into()),
        ("schema", "rotter.extract.poc/0".into()),
        ("stage", "extraction".into()),
        ("semantic_verification", "not_performed".into()),
        ("mode", mode.into()),
        ("repository", top.to_string_lossy().into_owned().into()),
        ("before", before),
        ("pathspec", options.paths.clone().into()),
        ("after", after.into()),
        (
            "untracked",
            Json::Obj(vec![
                ("included", include.into()),
                ("not_covered", not_covered.into()),
            ]),
        ),
        ("complete", complete.into()),
        ("files", Json::Arr(files)),
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        Change, Entry, Git, GitClass, Hunk, changes, classify, diff, drop_stat_only, guard_args,
        identity, linux_native, macos_native, parse_raw, select, temp_root, timeout_detail, walk,
        write_new,
    };
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rotter-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::canonicalize(path).unwrap()
    }

    #[test]
    fn stat_only_checks_never_read_a_path_that_leaves_the_working_tree() {
        let root = temp("stat-escape");
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
        let content = "package p\n";
        for file in [root.join("x.go"), repo.join("y.go")] {
            fs::write(file, content).unwrap();
        }
        symlink(&root, repo.join("link")).unwrap();
        let oid = String::from_utf8(
            std::process::Command::new("git")
                .args(["hash-object", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(content.as_bytes())?;
                    child.wait_with_output()
                })
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let entry = |path: &str| Entry {
            status: "M".to_owned(),
            old: Some((path.as_bytes().to_vec(), "100644".to_owned(), oid.clone())),
            new: Some((
                path.as_bytes().to_vec(),
                "100644".to_owned(),
                "0".repeat(oid.len()),
            )),
        };
        let git = Git::cli(&repo).unwrap();
        // Content equal to the before blob would drop the entry; outside the tree it is never read.
        let mut entries = vec![entry("../x.go"), entry("link/x.go"), entry("y.go")];
        drop_stat_only(&git, &repo, &mut entries, &[], &|| false).unwrap();
        let kept: Vec<_> = entries
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.new.as_ref().unwrap().0).into_owned())
            .collect();
        assert_eq!(
            kept,
            ["../x.go", "link/x.go"],
            "y.go is the positive control"
        );
        fs::remove_dir_all(root).unwrap();
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
    fn a_timeout_names_the_run_budget_only_when_it_was_shorter_than_the_limit() {
        let detail =
            |budget, limit| timeout_detail(Duration::from_secs(budget), Duration::from_secs(limit));
        assert_eq!(
            detail(1, 60),
            "stopped: the run's time budget ran out during this file"
        );
        assert!(detail(60, 60).starts_with("stopped after 60 s;"));
        assert!(detail(90, 60).starts_with("stopped after 60 s;"));
    }

    #[test]
    fn the_work_tree_probe_gives_up_after_its_limit() {
        let root = temp("probe");
        let git = Git::cli(&root).unwrap();
        // No git answers before it has even started.
        let error = git.work_tree(&root, Duration::ZERO).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert_eq!(git.work_tree(&root, Duration::from_secs(60)), Ok(None));
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
    fn untrusted_path_entries_before_the_chosen_git_are_named() {
        let root = temp("untrusted-path");
        let native = fs::read(std::env::current_exe().unwrap()).unwrap();
        // Like Homebrew's default /opt/homebrew/bin: group-writable.
        let shared = root.join("shared");
        place(&shared, &native, 0o755);
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o775)).unwrap();
        let good = root.join("good");
        let real = place(&good, &native, 0o755);

        let (program, _, refused) = select(Some(&joined(&[&shared, &good])), &[]).unwrap();
        assert_eq!(program, real);
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            refused[0].contains(&shared.display().to_string()),
            "{refused:?}"
        );
        assert!(
            refused[0].contains("writable by other users"),
            "{refused:?}"
        );
        let (_, _, after) = select(Some(&joined(&[&good, &shared])), &[]).unwrap();
        assert!(
            after.is_empty(),
            "entries after the chosen git do not matter: {after:?}"
        );
        let error = select(Some(&joined(&[&shared])), &[]).unwrap_err();
        assert!(error.contains(&shared.display().to_string()), "{error}");
        // A link whose name prefixes its target's still names the PATH entry itself.
        let short = root.join("sh");
        std::os::unix::fs::symlink(&shared, &short).unwrap();
        let (_, _, refused) = select(Some(&joined(&[&short, &good])), &[]).unwrap();
        assert_eq!(
            refused,
            [format!(
                "PATH entry {} ({}: directory is owned by another user or writable by other users)",
                short.display(),
                shared.display()
            )]
        );
        fs::remove_dir_all(root).unwrap();
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
        let (program, entries, _) = select(Some(&joined(&all)), &[]).unwrap();
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
        let (program, entries, refused) = select(Some(&path), &[identity(&root).unwrap()]).unwrap();
        assert_eq!((program, entries), (fallback, vec![other.join("bin")]));
        assert!(refused.is_empty(), "{refused:?}");
        let error = select(Some(&joined(&[&good])), &[identity(&root).unwrap()]).unwrap_err();
        assert!(!error.contains("good"), "{error}");
        fs::remove_dir_all(other).unwrap();
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
hook.command\0hook.Pre.Commit.event\0";
        let reset = |key: &str| format!("--config-env={key}=ROTTER_EMPTY_VALUE");
        let filter = |name: &str| {
            ["clean", "smudge", "process", "required"]
                .map(|var| reset(&format!("filter.{name}.{var}")))
        };
        let mut expected: Vec<String> = Vec::new();
        for name in ["x=y", "a.b", "X", ""] {
            expected.extend(filter(name));
        }
        // Once, though the hook's event key is listed twice.
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
