//! Grammars and the loader for installed external grammars. Loading only reads and verifies the
//! cache and dlopens a library; fetching and building live in `install.rs` alone.

use crate::Language;
use crate::config::{
    Kind, Meta, Untrusted, absolute_var, check_symbol, home, lstat, resolve_trusted, user,
};
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Where an external grammar's C sources come from; only `rotter parser install` reads them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalSource {
    Git {
        url: String,
        revision: String,
        /// Grammar directory inside the checkout; None is the checkout root.
        location: Option<String>,
    },
    /// Absolute directory holding parser.c (relative config values are already resolved).
    Path(PathBuf),
}

enum Source {
    Builtin(tree_sitter::Language),
    External {
        source: ExternalSource,
        symbol: String,
    },
}

/// A language rotter can analyse: a builtin grammar or an enabled external one.
///
/// Constructing a Grammar never loads a library; [`Grammar::language`] does so on first use.
pub struct Grammar {
    pub(crate) name: String,
    /// Set for builtins so language-specific rules (shebangs, Rust doc styles) keep working.
    pub(crate) builtin: Option<Language>,
    source: Source,
    ts: OnceLock<Result<tree_sitter::Language, String>>,
    pub(crate) unit_kinds: Vec<String>,
    pub(crate) function_kinds: Vec<String>,
    pub(crate) value_kinds: Vec<String>,
    pub(crate) attribute_kinds: Vec<String>,
    pub(crate) comment_kinds: Vec<String>,
    /// (prefix, label) pairs for externals; builtins use their own rules.
    pub(crate) directives: Vec<(String, String)>,
    /// Whether units using a changed unit's name are added.
    pub(crate) references: bool,
    pub(crate) extensions: Vec<String>,
    pub(crate) filenames: Vec<String>,
    /// Canonical top level of the analysed repository; a cache inside it is refused.
    repo: Option<PathBuf>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Everything an external definition declares, already validated by the config loader.
pub(crate) struct Definition {
    pub name: String,
    pub source: ExternalSource,
    pub symbol: String,
    pub unit_kinds: Vec<String>,
    pub function_kinds: Vec<String>,
    pub value_kinds: Vec<String>,
    pub attribute_kinds: Vec<String>,
    pub comment_kinds: Vec<String>,
    pub directives: Vec<(String, String)>,
    pub references: bool,
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
    pub repo: Option<PathBuf>,
}

impl Grammar {
    pub fn builtin(language: Language) -> Self {
        Self {
            name: language.name().to_owned(),
            builtin: Some(language),
            source: Source::Builtin(language.ts()),
            ts: OnceLock::new(),
            unit_kinds: strings(language.unit_kinds()),
            function_kinds: strings(language.function_kinds()),
            value_kinds: strings(language.function_value_kinds()),
            attribute_kinds: strings(language.attribute_kinds()),
            comment_kinds: strings(&["comment", "line_comment", "block_comment"]),
            directives: Vec::new(),
            references: !matches!(language, Language::Yaml | Language::Toml),
            extensions: Vec::new(),
            filenames: Vec::new(),
            repo: None,
        }
    }

    pub(crate) fn external(definition: Definition) -> Self {
        Self {
            name: definition.name,
            builtin: None,
            source: Source::External {
                source: definition.source,
                symbol: definition.symbol,
            },
            ts: OnceLock::new(),
            unit_kinds: definition.unit_kinds,
            function_kinds: definition.function_kinds,
            value_kinds: definition.value_kinds,
            attribute_kinds: definition.attribute_kinds,
            comment_kinds: definition.comment_kinds,
            directives: definition.directives,
            references: definition.references,
            extensions: definition.extensions,
            filenames: definition.filenames,
            repo: definition.repo,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where an external grammar is built from and its exported function; None for builtins.
    pub fn external_source(&self) -> Option<(&ExternalSource, &str)> {
        match &self.source {
            Source::Builtin(_) => None,
            Source::External { source, symbol } => Some((source, symbol)),
        }
    }

    /// The tree-sitter language, loaded on first call; the error explains what the user can do.
    pub fn language(&self) -> Result<&tree_sitter::Language, String> {
        self.ts
            .get_or_init(|| match &self.source {
                Source::Builtin(language) => Ok(language.clone()),
                Source::External { symbol, .. } => self.load(symbol).map_err(|why| {
                    format!(
                        "{why}; ask the user to run `rotter parser install {}`; installing needs \
                         user approval",
                        self.name
                    )
                }),
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The verified path of this external grammar's installed library (nothing is loaded).
    pub(crate) fn library(&self) -> Result<PathBuf, String> {
        let Source::External { source, symbol } = &self.source else {
            return Err(format!("{} is a builtin language", self.name));
        };
        let file = library_file(&self.name, &key(source, symbol)?);
        let parsers = parsers_dir(self.repo.as_deref())?;
        let path = parsers.join(file);
        let meta = lstat(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if meta.kind != Kind::File || meta.uid != user() || meta.mode & 0o022 != 0 {
            return Err(format!(
                "{} is not a regular file owned by you and unwritable by others",
                path.display()
            ));
        }
        Ok(path)
    }

    /// dlopens the verified library path and asks it for the grammar.
    fn load(&self, symbol: &str) -> Result<tree_sitter::Language, String> {
        let path = self.library()?;
        check_symbol(symbol)?;
        // SAFETY: the library was built by `rotter parser install` from sources the user enabled,
        // and every directory from / to it is checked; its constructors run here (documented).
        let library = unsafe { libloading::Library::new(&path) }
            .map_err(|error| format!("cannot load {}: {error}", path.display()))?;
        // Leaked so the returned Language, which points into it, can never outlive it.
        let library: &'static libloading::Library = Box::leak(Box::new(library));
        // SAFETY: tree-sitter grammars export `const TSLanguage *tree_sitter_<name>(void)`.
        let raw = unsafe {
            let function = library
                .get::<unsafe extern "C" fn() -> *const tree_sitter::ffi::TSLanguage>(symbol)
                .map_err(|error| format!("{} has no {symbol}: {error}", path.display()))?;
            function()
        };
        if raw.is_null() {
            return Err(format!(
                "{symbol} in {} returned no grammar",
                path.display()
            ));
        }
        // SAFETY: a non-null TSLanguage from the grammar; its ABI is checked by set_language.
        Ok(unsafe { tree_sitter::Language::from_raw(raw) })
    }
}

#[cfg(target_os = "macos")]
const LIBRARY_EXTENSION: &str = "dylib";
#[cfg(not(target_os = "macos"))]
const LIBRARY_EXTENSION: &str = "so";

/// `<name>-<key>.<ext>`: the file name carries the grammar's identity.
pub(crate) fn library_file(name: &str, key: &str) -> String {
    format!("{name}-{key}.{LIBRARY_EXTENSION}")
}

/// FNV-1a 64.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    parts
        .iter()
        .flat_map(|part| part.iter())
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// `<revision>-<h8>` for git grammars (h8 covers location and symbol); for local grammars the
/// hash of every compile input, read now.
fn key(source: &ExternalSource, symbol: &str) -> Result<String, String> {
    match source {
        ExternalSource::Git {
            revision, location, ..
        } => Ok(git_key(
            revision,
            location.as_deref().unwrap_or_default(),
            symbol,
        )),
        ExternalSource::Path(dir) => read_inputs(dir).map(|inputs| inputs_key(&inputs)),
    }
}

/// `<revision>-<h8>`: h8 is the first 8 hex digits of FNV-1a 64 over location and symbol.
pub(crate) fn git_key(revision: &str, location: &str, symbol: &str) -> String {
    let hash = fnv1a(&[location.as_bytes(), b"\0", symbol.as_bytes()]);
    format!("{revision}-{:08x}", hash >> 32)
}

/// Hash of the compile inputs in their (sorted) order, names and lengths included.
pub(crate) fn inputs_key(inputs: &[(String, Vec<u8>)]) -> String {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for (name, bytes) in inputs {
        parts.push(name.as_bytes().to_vec());
        parts.push((bytes.len() as u64).to_le_bytes().to_vec());
        parts.push(bytes.clone());
    }
    let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    format!("{:016x}", fnv1a(&parts))
}

/// Absolute `$XDG_CACHE_HOME`, else `$HOME/.cache`.
pub(crate) fn cache_base() -> Option<PathBuf> {
    absolute_var("XDG_CACHE_HOME").or_else(|| home().map(|home| home.join(".cache")))
}

/// A real directory (not a symlink) owned by the user with mode exactly 0700.
pub(crate) fn private_dir(path: &Path) -> Result<(), String> {
    let meta = lstat(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if meta.kind != Kind::Dir || meta.uid != user() || meta.mode & 0o7777 != 0o700 {
        return Err(format!(
            "{} must be a directory owned by you with mode 0700",
            path.display()
        ));
    }
    Ok(())
}

/// The canonical cache base, through [`resolve_trusted`].
pub(crate) fn trusted_cache_base() -> Result<PathBuf, String> {
    let base =
        cache_base().ok_or("no cache location (HOME and XDG_CACHE_HOME are unset or relative)")?;
    match resolve_trusted(&base, user(), &lstat) {
        Ok(resolved) => Ok(resolved),
        Err(Untrusted::Missing) => Err(format!("{} does not exist", base.display())),
        Err(Untrusted::Refused(why)) => Err(format!("cache {} refused: {why}", base.display())),
    }
}

/// `<canonical base>/rotter/parsers`, with `rotter/` and `parsers/` checked by [`private_dir`].
fn parsers_dir(repo: Option<&Path>) -> Result<PathBuf, String> {
    let base = trusted_cache_base()?;
    if let Some(repo) = repo
        && base.starts_with(repo)
    {
        return Err(format!(
            "cache {} is inside the repository {}",
            base.display(),
            repo.display()
        ));
    }
    let rotter = base.join("rotter");
    private_dir(&rotter)?;
    let parsers = rotter.join("parsers");
    private_dir(&parsers)?;
    Ok(parsers)
}

#[cfg(target_os = "macos")]
const OPEN_FLAGS: i32 = 0x0100 | 0x0004; // O_NOFOLLOW | O_NONBLOCK
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const OPEN_FLAGS: i32 = 0o400000 | 0o4000;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const OPEN_FLAGS: i32 = 0o100000 | 0o4000;
// ponytail: flag values per target without a libc dependency; add a target's values to port.
#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
)))]
compile_error!("O_NOFOLLOW/O_NONBLOCK values are not known for this target");

/// A compile input may be used when it is a regular file owned by the user or root that others
/// cannot write.
pub(crate) fn trusted_input(meta: Meta, user: u32) -> bool {
    meta.kind == Kind::File && (meta.uid == user || meta.uid == 0) && meta.mode & 0o022 == 0
}

/// Reads `name` in `dir`: the directory goes through [`resolve_trusted`], the file is opened by
/// the returned path without following a symlink or blocking on a FIFO, and the open descriptor
/// must pass [`trusted_input`]; the bytes come from that descriptor.
fn read_input(dir: &Path, name: &str) -> Result<Vec<u8>, String> {
    let dir = match resolve_trusted(dir, user(), &lstat) {
        Ok(dir) => dir,
        Err(Untrusted::Missing) => return Err(format!("{} does not exist", dir.display())),
        Err(Untrusted::Refused(why)) => return Err(format!("source refused: {why}")),
    };
    let path = dir.join(name);
    let fail = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(&path)
        .map_err(fail)?;
    let meta = file.metadata().map_err(fail)?;
    let kind = if meta.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    if !trusted_input(
        Meta {
            kind,
            uid: meta.uid(),
            mode: meta.mode(),
        },
        user(),
    ) {
        return Err(format!(
            "{} must be a regular file owned by you or root and not writable by others",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(fail)?;
    Ok(bytes)
}

/// A local grammar's closed list of compile inputs, in sorted order: `parser.c`, `scanner.c`
/// when present and `tree_sitter/*.h`. A C++ scanner is an error.
pub(crate) fn read_inputs(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    if lstat(&dir.join("scanner.cc")).is_ok() {
        return Err(format!("{}: C++ scanners are not supported", dir.display()));
    }
    let mut names = vec!["parser.c".to_owned()];
    if lstat(&dir.join("scanner.c")).is_ok() {
        names.push("scanner.c".to_owned());
    }
    let headers = dir.join("tree_sitter");
    let listed = match resolve_trusted(&headers, user(), &lstat) {
        Ok(resolved) => fs::read_dir(&resolved)
            .map_err(|error| format!("{}: {error}", resolved.display()))?
            .map(|entry| {
                let name = entry.map_err(|error| error.to_string())?.file_name();
                name.into_string()
                    .map_err(|_| format!("{}: non-UTF-8 file name", headers.display()))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Err(Untrusted::Missing) => Vec::new(),
        Err(Untrusted::Refused(why)) => return Err(format!("source refused: {why}")),
    };
    let mut listed: Vec<String> = listed
        .into_iter()
        .filter(|name| name.ends_with(".h"))
        .map(|name| format!("tree_sitter/{name}"))
        .collect();
    listed.sort();
    names.extend(listed);
    names
        .into_iter()
        .map(|name| {
            let (parent, file) = match name.split_once('/') {
                Some((sub, file)) => (dir.join(sub), file),
                None => (dir.to_owned(), name.as_str()),
            };
            let bytes = read_input(&parent, file)?;
            Ok((name, bytes))
        })
        .collect()
}

impl fmt::Debug for Grammar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Grammar")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Builtins in `Language` declaration order, so a `Language` indexes them.
const BUILTINS: [Language; 7] = [
    Language::Go,
    Language::Lua,
    Language::Nix,
    Language::Bash,
    Language::Yaml,
    Language::Toml,
    Language::Rust,
];

/// The grammars one run may use: the builtins plus enabled externals in precedence order.
#[derive(Clone, Debug)]
pub struct Languages {
    builtins: Vec<Arc<Grammar>>,
    pub(crate) externals: Vec<Arc<Grammar>>,
}

impl Default for Languages {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Languages {
    pub fn new(externals: Vec<Arc<Grammar>>) -> Self {
        Self {
            builtins: BUILTINS
                .iter()
                .map(|language| Arc::new(Grammar::builtin(*language)))
                .collect(),
            externals,
        }
    }

    pub(crate) fn builtin(&self, language: Language) -> Arc<Grammar> {
        Arc::clone(&self.builtins[language as usize])
    }

    /// Resolves a `--lang` or `[overrides]` name to a grammar and its dialect label.
    pub fn by_name(&self, name: &str) -> Option<(Arc<Grammar>, String)> {
        if let Some((language, dialect)) = Language::from_name(name) {
            return Some((self.builtin(language), dialect.to_owned()));
        }
        self.externals
            .iter()
            .find(|grammar| grammar.name == name)
            .map(|grammar| (Arc::clone(grammar), format!("{name}-by-override")))
    }
}

#[cfg(test)]
mod tests {
    use super::{git_key, inputs_key, trusted_input};
    use crate::config::{Kind, Meta, user};

    #[test]
    fn inputs_must_be_regular_owned_and_not_writable_by_others() {
        let meta = |kind, uid, mode| Meta { kind, uid, mode };
        assert!(trusted_input(meta(Kind::File, user(), 0o100644), user()));
        assert!(
            trusted_input(meta(Kind::File, 0, 0o100444), user()),
            "root-owned"
        );
        // Metadata injection: another user's file, group/world writable, not a regular file.
        assert!(!trusted_input(meta(Kind::File, 4242, 0o100644), user()));
        assert!(!trusted_input(meta(Kind::File, user(), 0o100664), user()));
        assert!(!trusted_input(meta(Kind::File, user(), 0o100646), user()));
        assert!(!trusted_input(meta(Kind::Other, user(), 0o010644), user()));
    }

    #[test]
    fn keys_cover_location_symbol_and_every_input() {
        let revision = "a".repeat(40);
        let base = git_key(&revision, "", "tree_sitter_x");
        assert!(
            base.starts_with(&format!("{revision}-")) && base.len() == 49,
            "{base}"
        );
        assert_ne!(base, git_key(&revision, "x", "tree_sitter_x"));
        assert_ne!(base, git_key(&revision, "", "tree_sitter_y"));
        // The separator keeps (location, symbol) pairs apart.
        assert_ne!(git_key(&revision, "a", "b"), git_key(&revision, "ab", ""));
        let inputs = |header: &[u8]| {
            vec![
                ("parser.c".to_owned(), b"int x;".to_vec()),
                ("tree_sitter/parser.h".to_owned(), header.to_vec()),
            ]
        };
        assert_eq!(inputs_key(&inputs(b"h")), inputs_key(&inputs(b"h")));
        assert_ne!(inputs_key(&inputs(b"h")), inputs_key(&inputs(b"h2")));
        assert_eq!(inputs_key(&inputs(b"h")).len(), 16);
    }
}
