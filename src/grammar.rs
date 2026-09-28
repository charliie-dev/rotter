use crate::Language;
use std::fmt;
use std::path::PathBuf;
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
                // ponytail: no loader yet, so every external is reported as not installed.
                Source::External { .. } => Err(format!(
                    "ask the user to run `rotter parser install {}`; installing needs user approval",
                    self.name
                )),
            })
            .as_ref()
            .map_err(Clone::clone)
    }
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
