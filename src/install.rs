//! `rotter parser install` and `rotter parser list`. The only code in rotter that fetches or
//! compiles anything: grammar sources are fetched with git at a pinned commit (or copied from a
//! local directory), compiled with `cc` inside a staging directory in the verified cache, and the
//! library is renamed into `parsers/`. Children get an allowlisted environment only.

use crate::ExternalSource;
use crate::config::{self, Config, Kind, Untrusted, absolute_var, lstat, resolve_trusted, user};
use crate::grammar::{
    Grammar, git_key, inputs_key, library_file, private_dir, read_inputs, trusted_cache_base,
};
use crate::json::Json;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Passed on every git call and written to the temporary repository's config.
const GIT_FLAGS: [(&str, &str); 8] = [
    ("core.symlinks", "false"),
    ("core.hooksPath", "/dev/null"),
    ("protocol.allow", "never"),
    ("protocol.https.allow", "always"),
    ("submodule.recurse", "false"),
    ("core.fsmonitor", "false"),
    ("credential.helper", ""),
    ("core.attributesFile", "/dev/null"),
];

/// Proxy and CA variables git may receive (read-only trust roots; the commit id is the integrity
/// check).
const GIT_NETWORK_VARS: [&str; 13] = [
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_SSL_CERT_FILE",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
];

const CFLAGS: [&str; 5] = [
    "-O2",
    "-std=c11",
    "-fPIC",
    "-fstack-protector-strong",
    "-D_FORTIFY_SOURCE=2",
];

const TRUST: &str = "Note: an installed grammar is C code that rotter runs in-process in every \
repository it analyses, including through the Claude hook. Repository content, including old \
versions from history, is fed to its scanner, so a memory-safety bug in the scanner is code \
execution as you, and library constructors run when it is loaded. Updating a pinned revision is \
a code-review event.";

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Git,
    Cc,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Cc => "cc",
        }
    }
}

/// The absolute entries of the inherited PATH; relative ones are dropped.
fn absolute_path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|entry| entry.is_absolute())
                .collect()
        })
        .unwrap_or_default()
}

/// `git` or `cc` from the absolute PATH entries only.
fn find_tool(tool: Tool) -> Result<PathBuf, String> {
    absolute_path_entries()
        .into_iter()
        .map(|dir| dir.join(tool.name()))
        .find(|path| {
            fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| {
            format!(
                "cannot find {} on the absolute entries of PATH",
                tool.name()
            )
        })
}

/// The complete environment of a git or cc child: nothing else is inherited.
fn child_env(tool: Tool, staging: &Path) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    let mut set = |key: &str, value: &OsStr| env.push((key.into(), value.to_owned()));
    let path = std::env::join_paths(absolute_path_entries()).unwrap_or_default();
    set("PATH", &path);
    if let Some(home) = config::home() {
        set("HOME", home.as_os_str());
    }
    set("LANG", OsStr::new("C"));
    set("LC_ALL", OsStr::new("C"));
    for key in ["TMPDIR", "TMP", "TEMP", "TEMPDIR"] {
        set(key, staging.as_os_str());
    }
    // macOS /usr/bin/git and /usr/bin/cc are xcrun shims that honour these; relative values
    // would resolve against the staging directory, so only absolute ones pass.
    for key in ["DEVELOPER_DIR", "SDKROOT"] {
        if let Some(value) = absolute_var(key) {
            set(key, value.as_os_str());
        }
    }
    match tool {
        Tool::Cc => {
            set("CLANG_CRASH_DIAGNOSTICS_DIR", staging.as_os_str());
            if let Some(value) = std::env::var_os("MACOSX_DEPLOYMENT_TARGET") {
                set("MACOSX_DEPLOYMENT_TARGET", &value);
            }
        }
        Tool::Git => {
            set("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null"));
            set("GIT_CONFIG_NOSYSTEM", OsStr::new("1"));
            set("GIT_TERMINAL_PROMPT", OsStr::new("0"));
            set("GIT_LFS_SKIP_SMUDGE", OsStr::new("1"));
            for key in GIT_NETWORK_VARS {
                if let Some(value) = std::env::var_os(key) {
                    set(key, &value);
                }
            }
        }
    }
    env
}

/// Tools and the staging directory of one install.
struct Build {
    staging: PathBuf,
    git: Option<PathBuf>,
    cc: PathBuf,
}

impl Build {
    fn run(&self, tool: Tool, args: &[&OsStr]) -> Result<Vec<u8>, String> {
        let program = match tool {
            Tool::Git => self.git.as_ref().ok_or("git is not available")?,
            Tool::Cc => &self.cc,
        };
        let output = Command::new(program)
            .env_clear()
            .envs(child_env(tool, &self.staging))
            .current_dir(&self.staging)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(format!(
                "{} {} failed: {}",
                tool.name(),
                args.iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn src(&self) -> PathBuf {
        self.staging.join("src")
    }

    /// `git -C <staging>/src -c … <args>`.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let src = self.src();
        let mut all: Vec<OsString> = vec!["-C".into(), src.into_os_string()];
        for (key, value) in GIT_FLAGS {
            all.push("-c".into());
            all.push(format!("{key}={value}").into());
        }
        all.extend(args.iter().map(OsString::from));
        let all: Vec<&OsStr> = all.iter().map(OsString::as_os_str).collect();
        self.run(Tool::Git, &all)
    }

    fn git_text(&self, args: &[&str]) -> Result<String, String> {
        let output = self.git(args)?;
        String::from_utf8(output)
            .map(|text| text.trim_end_matches('\n').to_owned())
            .map_err(|_| "git printed non-UTF-8 output".to_owned())
    }

    /// Fetches exactly `revision` into `<staging>/src` and checks it out.
    fn fetch(&self, url: &str, revision: &str) -> Result<(), String> {
        make_private_dir(&self.src())?;
        // An empty template: nothing from an ambient init.templateDir (e.g. `commondir`).
        self.git(&["init", "-q", "--template="])?;
        let git_dir = check_git_dirs(&self.staging, &self.src(), |flag| {
            self.git_text(&["rev-parse", flag])
        })?;
        for (key, value) in GIT_FLAGS {
            self.git(&["config", key, value])?;
        }
        // No filter drivers, text conversion or diff drivers run at checkout.
        make_private_dir(&git_dir.join("info"))?;
        write_new(&git_dir.join("info/attributes"), b"* -filter -text -diff\n")?;
        self.git(&["fetch", "-q", "--depth", "1", "--", url, revision])?;
        check_tree(&self.git(&["ls-tree", "-r", "-z", "FETCH_HEAD"])?)?;
        self.git(&["checkout", "-q", "--detach", "FETCH_HEAD"])?;
        let head = self.git_text(&["rev-parse", "HEAD"])?;
        if head != revision {
            return Err(format!("checked out {head}, expected {revision}"));
        }
        Ok(())
    }

    /// Compiles each C file separately into `<staging>/obj`, then links `<staging>/lib.tmp`.
    fn compile(&self, dir: &Path, sources: &[&str]) -> Result<PathBuf, String> {
        let obj = self.staging.join("obj");
        make_private_dir(&obj)?;
        let mut objects = Vec::new();
        for source in sources {
            let object = obj.join(Path::new(source).with_extension("o"));
            let input = dir.join(source);
            let mut args: Vec<&OsStr> = CFLAGS.iter().map(OsStr::new).collect();
            args.extend([
                OsStr::new("-I"),
                dir.as_os_str(),
                OsStr::new("-c"),
                input.as_os_str(),
                OsStr::new("-o"),
                object.as_os_str(),
            ]);
            self.run(Tool::Cc, &args)?;
            objects.push(object);
        }
        let library = self.staging.join("lib.tmp");
        let mut args: Vec<&OsStr> = vec![OsStr::new("-shared")];
        args.extend(objects.iter().map(|object| object.as_os_str()));
        args.extend([OsStr::new("-o"), library.as_os_str()]);
        self.run(Tool::Cc, &args)?;
        Ok(library)
    }
}

/// Removes the staging directory however the install ends.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates one directory (never its parents) with mode 0700.
fn make_private_dir(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

/// Creates a new file (an existing entry or symlink is an error).
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Checks `rev-parse --git-common-dir` and `--git-dir` right after init: both, resolved against
/// `src` and canonicalised, must lie inside `staging` (compared by components); a path that
/// cannot be canonicalised is refused. Returns the canonical git dir.
fn check_git_dirs(
    staging: &Path,
    src: &Path,
    rev_parse: impl Fn(&str) -> Result<String, String>,
) -> Result<PathBuf, String> {
    let mut git_dir = PathBuf::new();
    for flag in ["--git-common-dir", "--git-dir"] {
        let printed = rev_parse(flag)?;
        let canonical = fs::canonicalize(src.join(&printed)).map_err(|error| {
            format!("refusing to install: {flag} is {printed:?}, which cannot be resolved: {error}")
        })?;
        if !canonical.starts_with(staging) {
            return Err(format!(
                "refusing to install: {flag} is {} outside the staging directory {}",
                canonical.display(),
                staging.display()
            ));
        }
        git_dir = canonical;
    }
    Ok(git_dir)
}

/// `ls-tree -r -z` output must contain no symlink (120000) or submodule (160000) entry.
fn check_tree(listing: &[u8]) -> Result<(), String> {
    for entry in listing
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (info, path) = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .map(|tab| (&entry[..tab], &entry[tab + 1..]))
            .ok_or("unexpected git ls-tree output")?;
        let mode = info.split(|byte| *byte == b' ').next().unwrap_or_default();
        if mode == b"120000" || mode == b"160000" {
            return Err(format!(
                "refusing to install: the grammar tree contains a {} at {}",
                if mode == b"120000" {
                    "symbolic link"
                } else {
                    "submodule"
                },
                String::from_utf8_lossy(path)
            ));
        }
    }
    Ok(())
}

/// Checks the grammar sources in `dir` (lstat: regular files, no symlinks; no C++ scanner) and
/// returns the C files to compile.
fn check_sources(dir: &Path) -> Result<Vec<&'static str>, String> {
    let regular = |path: &Path| match lstat(path) {
        Ok(meta) if meta.kind == Kind::File => Ok(true),
        Ok(_) => Err(format!("{} is not a regular file", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    };
    if lstat(&dir.join("scanner.cc")).is_ok() {
        return Err(format!("{}: C++ scanners are not supported", dir.display()));
    }
    if !regular(&dir.join("parser.c"))? {
        return Err(format!("{} has no parser.c", dir.display()));
    }
    let mut sources = vec!["parser.c"];
    if regular(&dir.join("scanner.c"))? {
        sources.push("scanner.c");
    }
    let headers = dir.join("tree_sitter");
    match lstat(&headers) {
        Ok(meta) if meta.kind == Kind::Dir => {
            for entry in fs::read_dir(&headers).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path.extension().is_some_and(|extension| extension == "h") {
                    regular(&path)?;
                }
            }
        }
        Ok(_) => return Err(format!("{} is not a directory", headers.display())),
        Err(_) => {}
    }
    Ok(sources)
}

/// Creates missing components of `base` (the cache base, the state directory) one at a time
/// with mode 0700 (never `create_dir_all`), below the deepest existing ancestor, which must pass
/// `resolve_trusted`.
pub(crate) fn create_base(base: &Path) -> Result<(), String> {
    let mut existing = base.to_owned();
    let mut missing = Vec::new();
    while lstat(&existing).is_err() {
        match existing.components().next_back() {
            Some(Component::Normal(name)) => missing.push(name.to_owned()),
            _ => {
                return Err(format!(
                    "cannot create {}: use a path of plain directory names",
                    base.display()
                ));
            }
        }
        existing.pop();
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut dir = match resolve_trusted(&existing, user(), &lstat) {
        Ok(dir) => dir,
        Err(Untrusted::Missing) => return Err(format!("{} vanished", existing.display())),
        Err(Untrusted::Refused(why)) => return Err(format!("{} refused: {why}", base.display())),
    };
    for name in missing.iter().rev() {
        dir.push(name);
        make_private_dir(&dir)?;
        private_dir(&dir)?;
    }
    Ok(())
}

/// Creates `path` with mode 0700 unless it exists, then requires [`private_dir`].
fn ensure_private_dir(path: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("cannot create {}: {error}", path.display())),
    }
    private_dir(path)
}

/// The canonical `<base>/rotter`, with `rotter/`, `build/` and `parsers/` in place and checked.
fn prepare_cache(base: Option<&Path>) -> Result<PathBuf, String> {
    let base = base.ok_or("no cache location: set HOME or an absolute XDG_CACHE_HOME")?;
    create_base(base)?;
    let rotter = trusted_cache_base(Some(base))?.join("rotter");
    for dir in [rotter.clone(), rotter.join("build"), rotter.join("parsers")] {
        ensure_private_dir(&dir)?;
    }
    Ok(rotter)
}

fn install_one(grammar: &Grammar) -> Result<PathBuf, String> {
    let (source, symbol) = grammar
        .external_source()
        .ok_or_else(|| format!("{} is a builtin language", grammar.name()))?;
    config::check_symbol(symbol)?;
    let rotter = prepare_cache(grammar.cache())?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    let staging =
        rotter
            .join("build")
            .join(format!("{}-{}-{nanos}", grammar.name(), std::process::id()));
    make_private_dir(&staging)?;
    let _cleanup = Staging(staging.clone());
    let build = Build {
        git: matches!(source, ExternalSource::Git { .. })
            .then(|| find_tool(Tool::Git))
            .transpose()?,
        cc: find_tool(Tool::Cc)?,
        staging,
    };
    let (dir, key) = match source {
        ExternalSource::Git {
            url,
            revision,
            location,
        } => {
            build.fetch(url, revision)?;
            let root = fs::canonicalize(build.src()).map_err(|error| error.to_string())?;
            let grammar_dir = root
                .join(location.as_deref().unwrap_or_default())
                .join("src");
            let dir = fs::canonicalize(&grammar_dir)
                .map_err(|error| format!("{}: {error}", grammar_dir.display()))?;
            if !dir.starts_with(&root) {
                return Err(format!("{} is outside the checkout", dir.display()));
            }
            let key = git_key(revision, location.as_deref().unwrap_or_default(), symbol);
            (dir, key)
        }
        ExternalSource::Path(path) => {
            let inputs = read_inputs(path)?;
            let src = build.src();
            make_private_dir(&src)?;
            make_private_dir(&src.join("tree_sitter"))?;
            for (name, bytes) in &inputs {
                write_new(&src.join(name), bytes)?;
            }
            (src, inputs_key(&inputs))
        }
    };
    let sources = check_sources(&dir)?;
    let library = build.compile(&dir, &sources)?;
    fs::set_permissions(&library, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot chmod {}: {error}", library.display()))?;
    let target = rotter
        .join("parsers")
        .join(library_file(grammar.name(), &key));
    fs::rename(&library, &target)
        .map_err(|error| format!("cannot publish {}: {error}", target.display()))?;
    Ok(target)
}

/// One grammar of `rotter parser install`: `installed`, `failed` or `skipped`.
#[derive(Debug)]
pub struct ParserResult {
    pub name: String,
    pub result: &'static str,
    pub path: Option<String>,
    pub detail: Option<String>,
}

/// `rotter parser install` (`rotter.parser_install/1`). Installing stops at the first failure,
/// which is `error` (the later grammars are `skipped`); the document is printed either way.
#[derive(Debug)]
pub struct ParserInstall {
    pub parsers: Vec<ParserResult>,
    /// The code-execution notice, whenever anything was installed.
    pub notes: Vec<String>,
    pub error: Option<String>,
}

impl ParserInstall {
    pub fn json(&self) -> Json {
        let parsers = self
            .parsers
            .iter()
            .map(|parser| {
                Json::Obj(vec![
                    ("name", parser.name.as_str().into()),
                    ("result", parser.result.into()),
                    ("path", parser.path.clone().into()),
                    ("detail", parser.detail.clone().into()),
                ])
            })
            .collect();
        Json::Obj(vec![
            ("tool", "rotter".into()),
            ("schema", "rotter.parser_install/1".into()),
            ("parsers", Json::Arr(parsers)),
            ("notes", self.notes.clone().into()),
        ])
    }
}

/// `rotter parser install [<name>...]`: every enabled external grammar, or the named ones.
pub fn install(config: &Config, names: &[String]) -> Result<ParserInstall, String> {
    let chosen: Vec<&Arc<Grammar>> = if names.is_empty() {
        config.externals.iter().collect()
    } else {
        names
            .iter()
            .map(|name| {
                config
                    .externals
                    .iter()
                    .find(|grammar| grammar.name() == name)
                    .ok_or_else(|| {
                        format!(
                            "{name:?} is not enabled; add it to languages = [..] (registry: {}) \
                             or define [language.{name}] in config.toml",
                            config::registry_names().join(", ")
                        )
                    })
            })
            .collect::<Result<_, _>>()?
    };
    if chosen.is_empty() {
        return Err("no external languages are enabled in config.toml".to_owned());
    }
    let mut done = ParserInstall {
        parsers: Vec::new(),
        notes: Vec::new(),
        error: None,
    };
    for grammar in chosen {
        let name = grammar.name().to_owned();
        let (result, path, detail) = if done.error.is_some() {
            ("skipped", None, None)
        } else {
            match install_one(grammar) {
                Ok(path) => ("installed", Some(path.display().to_string()), None),
                Err(error) => {
                    done.error = Some(format!("{name}: {error}"));
                    ("failed", None, Some(error))
                }
            }
        };
        done.parsers.push(ParserResult {
            name,
            result,
            path,
            detail,
        });
    }
    if done
        .parsers
        .iter()
        .any(|parser| parser.result == "installed")
    {
        done.notes.push(TRUST.to_owned());
    }
    Ok(done)
}

/// One grammar of `rotter parser list`: an enabled one, or a registry entry not enabled.
#[derive(Debug)]
pub struct ParserEntry {
    pub name: String,
    pub enabled: bool,
    pub installed: bool,
    pub path: Option<String>,
    pub source: Option<String>,
    pub detail: Option<String>,
}

/// `rotter.parsers/1`.
pub fn list_json(parsers: &[ParserEntry]) -> Json {
    let parsers = parsers
        .iter()
        .map(|parser| {
            Json::Obj(vec![
                ("name", parser.name.as_str().into()),
                ("enabled", parser.enabled.into()),
                ("installed", parser.installed.into()),
                ("path", parser.path.clone().into()),
                ("source", parser.source.clone().into()),
                ("detail", parser.detail.clone().into()),
            ])
        })
        .collect();
    Json::Obj(vec![
        ("tool", "rotter".into()),
        ("schema", "rotter.parsers/1".into()),
        ("parsers", Json::Arr(parsers)),
    ])
}

/// `rotter parser list`: enabled grammars with their state, then registry entries not enabled.
pub fn list(config: &Config) -> Vec<ParserEntry> {
    let mut parsers = Vec::new();
    for grammar in &config.externals {
        let source = match grammar.external_source() {
            Some((ExternalSource::Git { url, revision, .. }, _)) => {
                Some(format!("{url} {revision}"))
            }
            Some((ExternalSource::Path(path), _)) => Some(path.display().to_string()),
            None => None,
        };
        let (path, detail) = match grammar.library() {
            Ok(path) => (Some(path.display().to_string()), None),
            Err(why) => (None, Some(why.to_string())),
        };
        parsers.push(ParserEntry {
            name: grammar.name().to_owned(),
            enabled: true,
            installed: path.is_some(),
            path,
            source,
            detail,
        });
    }
    for name in config::registry_names() {
        if !config
            .externals
            .iter()
            .any(|grammar| grammar.name() == name)
        {
            parsers.push(ParserEntry {
                name: name.to_string(),
                enabled: false,
                installed: false,
                path: None,
                source: None,
                detail: Some(format!("add {name:?} to languages in config.toml")),
            });
        }
    }
    parsers
}

#[cfg(test)]
mod tests {
    use super::{check_git_dirs, check_tree};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn temp(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("rotter-install-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        output.stdout
    }

    fn rev_parse(src: &Path) -> impl Fn(&str) -> Result<String, String> + '_ {
        move |flag| {
            Ok(String::from_utf8(git(src, &["rev-parse", flag]))
                .unwrap()
                .trim_end()
                .to_owned())
        }
    }

    #[test]
    fn common_dir_outside_staging_or_missing_is_refused() {
        let root = temp("commondir");
        let staging = root.join("staging");
        let src = staging.join("src");
        fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "--template="]);
        assert_eq!(
            check_git_dirs(&staging, &src, rev_parse(&src)).unwrap(),
            src.join(".git")
        );
        let canary = root.join("canary");
        fs::create_dir_all(&canary).unwrap();
        git(&canary, &["init", "-q"]);
        fs::write(
            src.join(".git/commondir"),
            format!("{}\n", canary.join(".git").display()),
        )
        .unwrap();
        let outside = check_git_dirs(&staging, &src, rev_parse(&src)).unwrap_err();
        assert!(outside.contains("outside the staging"), "{outside}");
        fs::write(
            src.join(".git/commondir"),
            format!("{}\n", root.join("gone").display()),
        )
        .unwrap();
        // git itself may reject the repository, or print the missing path: both refuse.
        let missing = check_git_dirs(&staging, &src, |flag| {
            Command::new("git")
                .arg("-C")
                .arg(&src)
                .args(["rev-parse", flag])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env_remove("GIT_DIR")
                .env_remove("GIT_COMMON_DIR")
                .output()
                .map_err(|error| error.to_string())
                .and_then(|output| {
                    if output.status.success() {
                        Ok(String::from_utf8_lossy(&output.stdout)
                            .trim_end()
                            .to_owned())
                    } else {
                        Err("rev-parse failed".to_owned())
                    }
                })
        })
        .unwrap_err();
        assert!(
            missing.contains("cannot be resolved") || missing.contains("rev-parse failed"),
            "{missing}"
        );
        // A printed path that does not exist is refused by canonicalisation.
        let printed = check_git_dirs(&staging, &src, |_| {
            Ok(root.join("gone").display().to_string())
        })
        .unwrap_err();
        assert!(printed.contains("cannot be resolved"), "{printed}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_with_symlink_or_submodule_is_refused() {
        let repo = temp("tree");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/parser.c"), "int x;\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "c"]);
        check_tree(&git(&repo, &["ls-tree", "-r", "-z", "HEAD"])).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", repo.join("src/scanner.c")).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "link"]);
        let error = check_tree(&git(&repo, &["ls-tree", "-r", "-z", "HEAD"])).unwrap_err();
        assert!(error.contains("symbolic link at src/scanner.c"), "{error}");
        let submodule = format!(
            "100644 blob {0}\ta\0160000 commit {0}\tvendor/x\0",
            "0".repeat(40)
        );
        assert!(
            check_tree(submodule.as_bytes())
                .unwrap_err()
                .contains("submodule")
        );
        fs::remove_dir_all(repo).unwrap();
    }
}
