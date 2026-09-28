//! `$XDG_CONFIG_HOME/rotter/config.toml`: parse limit, external languages and overrides.
//!
//! The file is only read from the user's own config directory, never from a repository or the
//! working directory, and only when every directory and symlink leading to it is trusted.

use crate::grammar::{Definition, ExternalSource, Grammar};
use crate::{Language, Languages};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PARSE_TIMEOUT: u64 = 60;
const MAX_PARSE_TIMEOUT: u64 = 3600;
const MAX_SYMLINK_HOPS: usize = 40;
const BUILTIN_NAMES: [&str; 8] = ["go", "lua", "nix", "bash", "sh", "yaml", "toml", "rust"];
const BUILTIN_EXTENSIONS: [&str; 15] = [
    "go", "lua", "nix", "rs", "yaml", "yml", "toml", "bash", "sh", "zsh", "ksh", "dash", "fish",
    "csh", "tcsh",
];

/// An environment variable that holds an absolute path; relative values are ignored.
pub(crate) fn absolute_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

pub(crate) fn home() -> Option<PathBuf> {
    absolute_var("HOME")
}

/// `$XDG_CONFIG_HOME/rotter/config.toml`, else `$HOME/.config/...`; None without either.
pub fn location() -> Option<PathBuf> {
    absolute_var("XDG_CONFIG_HOME")
        .or_else(|| home().map(|home| home.join(".config")))
        .map(|base| base.join("rotter").join("config.toml"))
}

#[derive(Clone, Debug)]
pub struct Config {
    pub parse_timeout_seconds: u64,
    /// Enabled external grammars in precedence order.
    pub externals: Vec<Arc<Grammar>>,
    /// `[overrides]` glob and language name, in file order.
    pub overrides: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parse_timeout_seconds: DEFAULT_PARSE_TIMEOUT,
            externals: Vec::new(),
            overrides: Vec::new(),
        }
    }
}

impl Config {
    pub fn parse_timeout(&self) -> Duration {
        Duration::from_secs(self.parse_timeout_seconds)
    }

    pub fn languages(&self) -> Languages {
        Languages::new(self.externals.clone())
    }

    /// `[overrides]` as `Options.languages` entries; names were checked when parsing.
    pub fn override_languages(&self, grammars: &Languages) -> Vec<(String, Arc<Grammar>, String)> {
        self.overrides
            .iter()
            .filter_map(|(pattern, name)| {
                let (grammar, dialect) = grammars.by_name(name)?;
                Some((pattern.clone(), grammar, dialect))
            })
            .collect()
    }
}

/// A usable config (the default when there is none) and why the file was not used, if it wasn't.
#[derive(Debug, Default)]
pub struct Loaded {
    pub config: Config,
    pub note: Option<String>,
}

impl Loaded {
    fn off(note: String) -> Self {
        Self {
            config: Config::default(),
            note: Some(format!("{note}; external languages disabled")),
        }
    }
}

/// Loads the config for a run in `repo` (None: no repository comparison). `Err` is an invalid
/// config; a missing file gives the defaults and an untrusted one the defaults with a note.
pub fn load(repo: Option<&Path>) -> Result<Loaded, String> {
    let Some(path) = location() else {
        return Ok(Loaded::off(
            "no config location (HOME and XDG_CONFIG_HOME are unset or relative)".to_owned(),
        ));
    };
    let resolved = match trusted_file(&path, user(), &lstat) {
        Ok(resolved) => resolved,
        Err(Untrusted::Missing) => return Ok(Loaded::default()),
        Err(Untrusted::Refused(why)) => {
            return Ok(Loaded::off(format!(
                "config {} refused: {why}",
                path.display()
            )));
        }
    };
    if let Some(repo) = repo {
        let canonical = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_owned());
        if path.starts_with(repo)
            || path.starts_with(&canonical)
            || resolved.starts_with(&canonical)
        {
            return Ok(Loaded::off(format!(
                "config {} is inside the repository {}; it is off for this repository",
                path.display(),
                repo.display()
            )));
        }
    }
    let text = fs::read_to_string(&resolved)
        .map_err(|error| format!("cannot read {}: {error}", resolved.display()))?;
    let base = resolved.parent().unwrap_or(Path::new("/"));
    let config = parse(&text, base).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Loaded { config, note: None })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    parse_timeout_seconds: Option<u64>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    language: BTreeMap<String, toml::Spanned<RawLanguage>>,
    #[serde(default)]
    overrides: BTreeMap<String, toml::Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanguage {
    url: Option<String>,
    revision: Option<String>,
    location: Option<String>,
    path: Option<String>,
    symbol: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
    units: Vec<String>,
    #[serde(default)]
    functions: Vec<String>,
    #[serde(default)]
    function_values: Vec<String>,
    #[serde(default)]
    attributes: Vec<String>,
    #[serde(default = "default_comments")]
    comments: Vec<String>,
    /// Comment prefix → directive label.
    #[serde(default)]
    directives: BTreeMap<String, String>,
    #[serde(default = "default_references")]
    references: bool,
}

fn default_comments() -> Vec<String> {
    vec!["comment".to_owned()]
}

fn default_references() -> bool {
    true
}

/// `^[a-z][a-z0-9_]*$`
fn is_identifier(text: &str) -> bool {
    text.starts_with(|first: char| first.is_ascii_lowercase())
        && text
            .chars()
            .all(|next| next.is_ascii_lowercase() || next.is_ascii_digit() || next == '_')
}

pub(crate) fn check_name(name: &str) -> Result<(), String> {
    if !is_identifier(name) {
        return Err(format!(
            "language name {name:?} must match ^[a-z][a-z0-9_]*$"
        ));
    }
    if BUILTIN_NAMES.contains(&name) {
        return Err(format!("language name {name:?} is a builtin language"));
    }
    Ok(())
}

pub(crate) fn check_url(url: &str) -> Result<(), String> {
    if url.starts_with('-')
        || !url.starts_with("https://")
        || url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(format!(
            "url {url:?} must start with https:// and contain no whitespace or control characters"
        ));
    }
    Ok(())
}

pub(crate) fn check_revision(revision: &str) -> Result<(), String> {
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "revision {revision:?} must be a full 40-character lowercase commit id"
        ));
    }
    Ok(())
}

pub(crate) fn check_symbol(symbol: &str) -> Result<(), String> {
    let rest = symbol.strip_prefix("tree_sitter_").unwrap_or_default();
    if rest.is_empty()
        || !rest
            .chars()
            .all(|next| next.is_ascii_lowercase() || next.is_ascii_digit() || next == '_')
        || symbol.contains("_external_scanner_")
    {
        return Err(format!(
            "symbol {symbol:?} must match ^tree_sitter_[a-z0-9_]+$ and not name a scanner function"
        ));
    }
    Ok(())
}

pub(crate) fn check_location(location: &str) -> Result<(), String> {
    if location.contains('\0')
        || location
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!(
            "location {location:?} must be a relative path of plain directory names"
        ));
    }
    Ok(())
}

fn check_extension(extension: &str) -> Result<(), String> {
    if extension.is_empty() || extension.contains(['/', '\0']) {
        return Err(format!(
            "extension {extension:?} must be non-empty without / or NUL"
        ));
    }
    if BUILTIN_EXTENSIONS.contains(&extension) {
        return Err(format!(
            "extension {extension:?} belongs to a builtin language"
        ));
    }
    Ok(())
}

fn check_filename(pattern: &str) -> Result<(), String> {
    if pattern.contains(['/', '\0'])
        || pattern.contains("**")
        || pattern.chars().all(|character| "*?".contains(character))
    {
        return Err(format!(
            "filename {pattern:?} must be a base name or base-name glob with a literal character, \
             without /, NUL or **"
        ));
    }
    Ok(())
}

/// Parses and validates config text; relative `path` values resolve against `base`.
pub(crate) fn parse(text: &str, base: &Path) -> Result<Config, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|error| error.to_string())?;
    let parse_timeout_seconds = raw.parse_timeout_seconds.unwrap_or(DEFAULT_PARSE_TIMEOUT);
    if !(1..=MAX_PARSE_TIMEOUT).contains(&parse_timeout_seconds) {
        return Err(format!(
            "parse_timeout_seconds must be between 1 and {MAX_PARSE_TIMEOUT}, not {parse_timeout_seconds}"
        ));
    }
    // ponytail: no registry is bundled yet, so every `languages` entry is unknown.
    if let Some(name) = raw.languages.first() {
        return Err(format!("languages: {name:?} is not in the registry"));
    }
    let mut tables: Vec<_> = raw.language.into_iter().collect();
    tables.sort_by_key(|(_, table)| table.span().start);
    let mut externals: Vec<Arc<Grammar>> = Vec::new();
    for (name, table) in tables {
        let grammar = definition(&name, table.into_inner(), base)
            .map_err(|error| format!("[language.{name}]: {error}"))?;
        for other in &externals {
            if let Some(extension) = grammar
                .extensions
                .iter()
                .find(|extension| other.extensions.contains(extension))
            {
                return Err(format!(
                    "extension {extension:?} is claimed by both {} and {name}",
                    other.name
                ));
            }
            if let Some(pattern) = grammar
                .filenames
                .iter()
                .find(|pattern| other.filenames.contains(pattern))
            {
                return Err(format!(
                    "filename {pattern:?} is claimed by both {} and {name}",
                    other.name
                ));
            }
        }
        externals.push(Arc::new(Grammar::external(grammar)));
    }
    let mut overrides: Vec<_> = raw.overrides.into_iter().collect();
    overrides.sort_by_key(|(_, name)| name.span().start);
    let overrides = overrides
        .into_iter()
        .map(|(pattern, name)| {
            let name = name.into_inner();
            let known = Language::from_name(&name).is_some()
                || externals.iter().any(|grammar| grammar.name == name);
            if pattern.is_empty() || !known {
                return Err(format!(
                    "[overrides]: {pattern:?} = {name:?} needs a glob and a builtin or enabled language"
                ));
            }
            Ok((pattern, name))
        })
        .collect::<Result<_, _>>()?;
    Ok(Config {
        parse_timeout_seconds,
        externals,
        overrides,
    })
}

fn definition(name: &str, raw: RawLanguage, base: &Path) -> Result<Definition, String> {
    check_name(name)?;
    let source = match (raw.path, raw.url) {
        (Some(path), None) if raw.revision.is_none() && raw.location.is_none() => {
            if path.is_empty() || path.contains('\0') {
                return Err(format!("path {path:?} must be a non-empty path"));
            }
            ExternalSource::Path(base.join(path))
        }
        (None, Some(url)) => {
            check_url(&url)?;
            let revision = raw.revision.ok_or("url needs a revision")?;
            check_revision(&revision)?;
            if let Some(location) = &raw.location {
                check_location(location)?;
            }
            ExternalSource::Git {
                url,
                revision,
                location: raw.location,
            }
        }
        _ => {
            return Err(
                "set either path, or url with revision (and optionally location)".to_owned(),
            );
        }
    };
    check_symbol(&raw.symbol)?;
    for extension in &raw.extensions {
        check_extension(extension)?;
    }
    for pattern in &raw.filenames {
        check_filename(pattern)?;
    }
    if raw.units.is_empty() {
        return Err("units must list at least one node kind".to_owned());
    }
    let kinds = [
        &raw.units,
        &raw.functions,
        &raw.function_values,
        &raw.attributes,
        &raw.comments,
    ];
    if kinds
        .iter()
        .flat_map(|kinds| kinds.iter())
        .any(String::is_empty)
    {
        return Err("node kinds must not be empty".to_owned());
    }
    if let Some((prefix, label)) = raw
        .directives
        .iter()
        .find(|(prefix, label)| prefix.is_empty() || !is_identifier(label))
    {
        return Err(format!(
            "directive {prefix:?} = {label:?} needs a prefix and a label matching ^[a-z][a-z0-9_]*$"
        ));
    }
    Ok(Definition {
        name: name.to_owned(),
        source,
        symbol: raw.symbol,
        unit_kinds: raw.units,
        function_kinds: raw.functions,
        value_kinds: raw.function_values,
        attribute_kinds: raw.attributes,
        comment_kinds: raw.comments,
        directives: raw.directives.into_iter().collect(),
        references: raw.references,
        extensions: raw.extensions,
        filenames: raw.filenames,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What the trust checks read from `lstat`; tests inject owners and modes through this.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Meta {
    pub kind: Kind,
    pub uid: u32,
    pub mode: u32,
}

pub(crate) fn lstat(path: &Path) -> io::Result<Meta> {
    let meta = fs::symlink_metadata(path)?;
    let kind = meta.file_type();
    Ok(Meta {
        kind: if kind.is_symlink() {
            Kind::Symlink
        } else if kind.is_dir() {
            Kind::Dir
        } else if kind.is_file() {
            Kind::File
        } else {
            Kind::Other
        },
        uid: meta.uid(),
        mode: meta.mode(),
    })
}

unsafe extern "C" {
    safe fn geteuid() -> u32;
}

pub(crate) fn user() -> u32 {
    geteuid()
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Untrusted {
    /// A component does not exist (every component before it was trusted).
    Missing,
    Refused(String),
}

/// StrictModes: owned by the user or root, and not writable by others unless root-owned sticky.
fn safe_dir(meta: Meta, user: u32) -> bool {
    (meta.uid == user || meta.uid == 0)
        && (meta.mode & 0o022 == 0 || (meta.uid == 0 && meta.mode & 0o1000 != 0))
}

/// Resolves an absolute path one component at a time from `/`, following symlinks itself. Every
/// directory traversed (including symlink targets and the final component when it is a directory)
/// must pass [`safe_dir`], and every symlink followed must be owned by the user or root. The
/// result contains no symlinks; callers use only it afterwards. `user` is normally [`user()`]
/// and `lstat` [`lstat()`]; tests inject owners and modes.
pub(crate) fn resolve_trusted(
    path: &Path,
    user: u32,
    lstat: &dyn Fn(&Path) -> io::Result<Meta>,
) -> Result<PathBuf, Untrusted> {
    let refuse = |at: &Path, why: &str| Untrusted::Refused(format!("{}: {why}", at.display()));
    if !path.is_absolute() {
        return Err(refuse(path, "not an absolute path"));
    }
    let stat = |at: &Path| {
        lstat(at).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => Untrusted::Missing,
            _ => refuse(at, &error.to_string()),
        })
    };
    let check_dir = |at: &Path, meta: Meta| {
        if safe_dir(meta, user) {
            Ok(())
        } else {
            Err(refuse(
                at,
                "directory is owned by another user or writable by other users",
            ))
        }
    };
    // Components still to resolve, next one last; ".." (never a normal name) means the parent.
    let mut pending: Vec<OsString> = Vec::new();
    let mut resolved = PathBuf::from("/");
    let push = |pending: &mut Vec<OsString>, target: &Path| {
        for component in target.components().rev() {
            match component {
                Component::Normal(name) => pending.push(name.to_owned()),
                Component::ParentDir => pending.push("..".into()),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
    };
    check_dir(&resolved, stat(&resolved)?)?;
    push(&mut pending, path);
    let mut hops = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            // The prefix is already resolved and checked, so popping it is exact.
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        let meta = stat(&next)?;
        match meta.kind {
            Kind::Symlink => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(refuse(path, "too many symbolic links"));
                }
                if meta.uid != user && meta.uid != 0 {
                    return Err(refuse(&next, "symbolic link owned by another user"));
                }
                let target =
                    fs::read_link(&next).map_err(|error| refuse(&next, &error.to_string()))?;
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                // A relative target resolves against the directory holding the link.
                push(&mut pending, &target);
            }
            Kind::Dir => {
                check_dir(&next, meta)?;
                resolved = next;
            }
            _ if pending.is_empty() => resolved = next,
            _ => return Err(refuse(&next, "not a directory")),
        }
    }
    Ok(resolved)
}

/// [`resolve_trusted`], then the file itself: regular, owned by the user or root, not writable by
/// group or others.
pub(crate) fn trusted_file(
    path: &Path,
    user: u32,
    lstat: &dyn Fn(&Path) -> io::Result<Meta>,
) -> Result<PathBuf, Untrusted> {
    let resolved = resolve_trusted(path, user, lstat)?;
    let meta = lstat(&resolved)
        .map_err(|error| Untrusted::Refused(format!("{}: {error}", resolved.display())))?;
    if meta.kind != Kind::File {
        return Err(Untrusted::Refused(format!(
            "{} is not a regular file",
            resolved.display()
        )));
    }
    if (meta.uid != user && meta.uid != 0) || meta.mode & 0o022 != 0 {
        return Err(Untrusted::Refused(format!(
            "{} is owned by another user or writable by other users",
            resolved.display()
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::{Kind, Meta, Untrusted, lstat, parse, resolve_trusted, trusted_file, user};
    use crate::Detected;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const LUA2: &str = r#"
[language.lua2]
path = "lua-src"
symbol = "tree_sitter_lua"
extensions = ["lua2"]
units = ["function_declaration"]
"#;

    fn temp() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rotter-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn language(body: &str) -> String {
        format!("[language.x]\nsymbol = \"tree_sitter_x\"\nunits = [\"a\"]\n{body}\n")
    }

    #[test]
    fn valid_config_keeps_file_order_and_resolves_paths() {
        let config = parse(
            &format!(
                "parse_timeout_seconds = 5\n{LUA2}\n[language.alpha]\nurl = \"https://example.invalid/g.git\"\n\
                 revision = \"{}\"\nlocation = \"sub/dir\"\nsymbol = \"tree_sitter_alpha\"\n\
                 units = [\"x\"]\n[overrides]\n\"z/*\" = \"lua2\"\n\"a/*\" = \"sh\"\n",
                "0".repeat(40)
            ),
            Path::new("/cfg"),
        )
        .unwrap();
        assert_eq!(config.parse_timeout_seconds, 5);
        let names: Vec<&str> = config.externals.iter().map(|g| g.name()).collect();
        assert_eq!(names, ["lua2", "alpha"]);
        assert_eq!(
            config.overrides,
            [("z/*".into(), "lua2".into()), ("a/*".into(), "sh".into())]
        );
        match config.externals[0].external_source() {
            Some((crate::ExternalSource::Path(path), "tree_sitter_lua")) => {
                assert_eq!(path, Path::new("/cfg/lua-src"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse("", Path::new("/")).unwrap().parse_timeout_seconds, 60);
    }

    #[test]
    fn invalid_values_and_unknown_keys_are_errors() {
        let revision = format!("revision = \"{}\"", "a".repeat(40));
        let git = |url: &str| language(&format!("url = \"{url}\"\n{revision}"));
        let cases = [
            "parse_timeout_seconds = 0".to_owned(),
            "parse_timeout_seconds = -1".to_owned(),
            "parse_timeout_seconds = 3601".to_owned(),
            "parse_timeout_seconds = \"60\"".to_owned(),
            "parse_timeout_seconds = 1.5".to_owned(),
            "unknown = 1".to_owned(),
            language("path = \"p\"\nunknown = 1"),
            "languages = [\"python\"]".to_owned(),
            LUA2.replace("lua2]", "\"../x\"]"),
            LUA2.replace("lua2]", "-x]"),
            LUA2.replace("lua2]", "go]"),
            LUA2.replace("lua2]", "sh]"),
            git("-x"),
            git("--upload-pack=x"),
            git("ext::sh -c x"),
            git("http://example.invalid/g"),
            git("https://example.invalid/a b"),
            language(&format!(
                "url = \"https://e.invalid\"\nrevision = \"{}\"",
                "a".repeat(39)
            )),
            language("url = \"https://e.invalid\""),
            language(&format!(
                "url = \"https://e.invalid\"\n{revision}\nlocation = \"/abs\""
            )),
            language(&format!(
                "url = \"https://e.invalid\"\n{revision}\nlocation = \"a/../b\""
            )),
            language(&format!(
                "url = \"https://e.invalid\"\n{revision}\npath = \"p\""
            )),
            language("path = \"p\"\nrevision = \"x\""),
            LUA2.replace("tree_sitter_lua", "tree_sitter_a b"),
            LUA2.replace("tree_sitter_lua", "tree_sitter_a\\u0000"),
            LUA2.replace("tree_sitter_lua", "tree_sitter_lua_external_scanner_create"),
            LUA2.replace("tree_sitter_lua", "lua"),
            LUA2.replace("\"lua2\"", "\"lua\""),
            LUA2.replace("\"lua2\"", "\"yml\""),
            LUA2.replace("\"lua2\"", "\"a/b\""),
            format!("{LUA2}\n{}", LUA2.replace("lua2]", "lua3]")),
            LUA2.replace("extensions", "filenames = [\"*\"]\nextensions"),
            LUA2.replace("extensions", "filenames = [\"**\"]\nextensions"),
            LUA2.replace("extensions", "filenames = [\"a/Dockerfile\"]\nextensions"),
            format!(
                "{}\n{}",
                language("path = \"p\"\nfilenames = [\"Dockerfile.*\"]"),
                language("path = \"p\"\nfilenames = [\"Dockerfile.*\"]").replace("x]", "y]")
            ),
            LUA2.replace("units = [\"function_declaration\"]", "units = []"),
            "[overrides]\n\"*.x\" = \"python\"".to_owned(),
        ];
        for case in cases {
            assert!(parse(&case, Path::new("/")).is_err(), "accepted: {case}");
        }
    }

    #[test]
    fn detection_order_builtin_then_filenames_then_extensions() {
        let config = parse(
            r#"
[language.first]
path = "/p"
symbol = "tree_sitter_first"
filenames = ["Dockerfile.*", "Containerfile"]
extensions = ["dockerfile"]
units = ["x"]
[language.second]
path = "/p"
symbol = "tree_sitter_second"
filenames = ["*.dockerfile", "Dockerfile"]
units = ["x"]
"#,
            Path::new("/"),
        )
        .unwrap();
        let languages = config.languages();
        let name = |detected: Detected| match detected {
            Detected::Supported(grammar, _) => grammar.name().to_owned(),
            other => format!("{other:?}"),
        };
        let path = |path: &str| name(languages.detect_path(Path::new(path)));
        assert_eq!(path("Dockerfile.dockerfile"), "first");
        assert_eq!(path("x/Dockerfile.prod"), "first");
        assert_eq!(
            path("a.dockerfile"),
            "second",
            "file names before extensions"
        );
        assert_eq!(path("a.go"), "go");
        assert_eq!(path("Dockerfile"), "NeedsContent");
        let content =
            |path: &str, line: &str| name(languages.detect_content(Path::new(path), line));
        assert_eq!(content("Dockerfile", "FROM x"), "second");
        assert_eq!(content("Containerfile", "FROM x"), "first");
        assert_eq!(
            content("Dockerfile", "#!/bin/bash"),
            "bash",
            "builtin shebang wins"
        );
        assert_eq!(content("script", "#!/bin/bash"), "bash");
        assert_eq!(content("script", "plain"), "NotInScope");
        let (grammar, dialect) = languages.by_name("second").unwrap();
        assert_eq!(
            (grammar.name(), dialect.as_str()),
            ("second", "second-by-override")
        );
        assert!(languages.by_name("third").is_none());
        assert!(
            grammar
                .language()
                .unwrap_err()
                .contains("rotter parser install second")
        );
    }

    fn real() -> impl Fn(&Path) -> std::io::Result<Meta> {
        |path| lstat(path)
    }

    /// The real lstat with owner and mode replaced for the given paths.
    fn with(overrides: Vec<(PathBuf, u32, u32)>) -> impl Fn(&Path) -> std::io::Result<Meta> {
        move |path| {
            let mut meta = lstat(path)?;
            if let Some((_, uid, mode)) = overrides.iter().find(|(at, _, _)| at == path) {
                meta.uid = *uid;
                meta.mode = *mode;
            }
            Ok(meta)
        }
    }

    #[test]
    fn resolver_follows_relative_dot_dot_links_physically() {
        let root = temp();
        fs::create_dir_all(root.join("cfg")).unwrap();
        fs::create_dir_all(root.join("x")).unwrap();
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/config.toml"), "").unwrap();
        symlink("../x/../real/config.toml", root.join("cfg/config.toml")).unwrap();
        // Through the original temp path, which on macOS itself starts with the /tmp symlink.
        let resolved = trusted_file(&root.join("cfg/config.toml"), user(), &real()).unwrap();
        assert_eq!(resolved, root.join("real/config.toml"));
        let dotted = root.join("cfg/./../real/config.toml");
        assert_eq!(
            resolve_trusted(&dotted, user(), &real()).unwrap(),
            root.join("real/config.toml")
        );
        assert_eq!(
            resolve_trusted(&root.join("cfg/missing"), user(), &real()),
            Err(Untrusted::Missing)
        );
        symlink("loop", root.join("loop")).unwrap();
        assert!(matches!(
            resolve_trusted(&root.join("loop"), user(), &real()),
            Err(Untrusted::Refused(why)) if why.contains("too many")
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolver_refuses_other_users_link_in_sticky_directory() {
        let root = temp();
        fs::create_dir_all(root.join("sticky")).unwrap();
        fs::create_dir_all(root.join("safe")).unwrap();
        fs::write(root.join("safe/payload.toml"), "").unwrap();
        symlink(root.join("safe/payload.toml"), root.join("sticky/link")).unwrap();
        symlink(root.join("sticky/link"), root.join("config.toml")).unwrap();
        let sticky = (root.join("sticky"), 0, 0o41777);
        let refused = with(vec![
            sticky.clone(),
            (root.join("sticky/link"), 4242, 0o120777),
        ]);
        assert!(matches!(
            trusted_file(&root.join("config.toml"), user(), &refused),
            Err(Untrusted::Refused(why)) if why.contains("symbolic link owned by another user")
        ));
        let accepted = with(vec![sticky, (root.join("sticky/link"), user(), 0o120777)]);
        assert_eq!(
            trusted_file(&root.join("config.toml"), user(), &accepted).unwrap(),
            root.join("safe/payload.toml")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolver_checks_every_directory_on_a_multi_hop_chain() {
        let root = temp();
        fs::create_dir_all(root.join("x/shared")).unwrap();
        fs::create_dir_all(root.join("safe")).unwrap();
        fs::write(root.join("safe/payload.toml"), "").unwrap();
        symlink(
            root.join("safe/payload.toml"),
            root.join("x/shared/current.toml"),
        )
        .unwrap();
        symlink(root.join("x/shared/current.toml"), root.join("config.toml")).unwrap();
        let config = root.join("config.toml");
        fs::set_permissions(root.join("x/shared"), fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            trusted_file(&config, user(), &real()),
            Err(Untrusted::Refused(why)) if why.contains("shared")
        ));
        fs::set_permissions(root.join("x/shared"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            trusted_file(&config, user(), &real()).unwrap(),
            root.join("safe/payload.toml")
        );

        // A group-writable non-sticky directory holding a link to a root-owned file.
        let unsafe_dir = with(vec![
            (root.join("x/shared"), user(), 0o40775),
            (root.join("safe/payload.toml"), 0, 0o100444),
        ]);
        assert!(matches!(
            trusted_file(&config, user(), &unsafe_dir),
            Err(Untrusted::Refused(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_file_must_be_regular_and_not_writable_by_others() {
        let root = temp();
        fs::create_dir_all(root.join("store")).unwrap();
        fs::write(root.join("store/payload.toml"), "").unwrap();
        symlink(root.join("store/payload.toml"), root.join("config.toml")).unwrap();
        let config = root.join("config.toml");
        // Like home-manager: a root-owned 0444 file in a root-owned unwritable store directory.
        let store = with(vec![
            (root.join("store"), 0, 0o40555),
            (root.join("store/payload.toml"), 0, 0o100444),
        ]);
        assert!(trusted_file(&config, user(), &store).is_ok());
        let sticky_store = with(vec![
            (root.join("store"), 0, 0o41777),
            (root.join("store/payload.toml"), 0, 0o100444),
        ]);
        assert!(trusted_file(&config, user(), &sticky_store).is_ok());
        let foreign = with(vec![(root.join("store/payload.toml"), 4242, 0o100444)]);
        assert!(trusted_file(&config, user(), &foreign).is_err());

        let payload = root.join("store/payload.toml");
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o664)).unwrap();
        assert!(matches!(
            trusted_file(&config, user(), &real()),
            Err(Untrusted::Refused(why)) if why.contains("writable")
        ));
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(trusted_file(&config, user(), &real()).is_ok());
        assert!(matches!(
            trusted_file(&root.join("store"), user(), &real()),
            Err(Untrusted::Refused(why)) if why.contains("regular")
        ));
        assert!(
            real()(&root).is_ok_and(|meta| meta.kind == Kind::Dir),
            "sanity: lstat sees directories"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
