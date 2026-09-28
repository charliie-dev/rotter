mod comments;
pub mod config;
mod git;
mod grammar;
pub mod hosts;
pub mod install;
pub mod integration;
pub mod json;

pub use comments::{Change, error_lines, full_units, units};
pub use git::{Git, Mode, Options, Report, extract, extract_with, toplevel};
pub use grammar::{ExternalSource, Grammar, Languages};

use std::fmt;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Go,
    Lua,
    Nix,
    Bash,
    Yaml,
    Toml,
    Rust,
}

impl Language {
    /// Parses a `--lang` value; `sh` means a POSIX script parsed with the Bash grammar.
    pub fn from_name(name: &str) -> Option<(Self, &'static str)> {
        Some(match name {
            "go" => (Self::Go, "go-by-override"),
            "lua" => (Self::Lua, "lua-by-override"),
            "nix" => (Self::Nix, "nix-by-override"),
            "bash" => (Self::Bash, "bash-by-override"),
            "sh" => (Self::Bash, "sh-parsed-as-bash-by-override"),
            "yaml" => (Self::Yaml, "yaml-by-override"),
            "toml" => (Self::Toml, "toml-by-override"),
            "rust" => (Self::Rust, "rust-by-override"),
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Go => "go",
            Self::Lua => "lua",
            Self::Nix => "nix",
            Self::Bash => "bash",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Rust => "rust",
        }
    }

    pub(crate) fn ts(self) -> tree_sitter::Language {
        match self {
            Self::Go => tree_sitter_go::LANGUAGE,
            Self::Lua => tree_sitter_lua::LANGUAGE,
            Self::Nix => tree_sitter_nix::LANGUAGE,
            Self::Bash => tree_sitter_bash::LANGUAGE,
            Self::Yaml => tree_sitter_yaml::LANGUAGE,
            Self::Toml => tree_sitter_toml_ng::LANGUAGE,
            Self::Rust => tree_sitter_rust::LANGUAGE,
        }
        .into()
    }

    /// Node kinds treated as a unit that comments can belong to.
    pub(crate) fn unit_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Go => &[
                "function_declaration",
                "method_declaration",
                "type_declaration",
                "type_spec",
                "type_alias",
                "field_declaration",
                "method_elem",
                "const_declaration",
                "const_spec",
                "var_declaration",
                "var_spec",
                "import_declaration",
            ],
            Self::Lua => &[
                "function_declaration",
                "variable_declaration",
                "assignment_statement",
                "field",
            ],
            Self::Nix => &["binding", "inherit", "inherit_from"],
            Self::Bash => &["function_definition"],
            Self::Yaml => &["block_mapping_pair", "block_sequence_item", "flow_pair"],
            Self::Toml => &["pair", "table", "table_array_element"],
            Self::Rust => &[
                "function_item",
                "function_signature_item",
                "struct_item",
                "enum_item",
                "union_item",
                "enum_variant",
                "field_declaration",
                "impl_item",
                "trait_item",
                "mod_item",
                "const_item",
                "static_item",
                "type_item",
                "macro_definition",
                "use_declaration",
            ],
        }
    }

    /// Unit kinds whose bodies are one unit: smaller units nested inside them are ignored.
    pub(crate) fn function_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Go => &["function_declaration", "method_declaration"],
            Self::Lua => &["function_declaration"],
            Self::Bash => &["function_definition"],
            Self::Rust => &["function_item"],
            Self::Nix | Self::Yaml | Self::Toml => &[],
        }
    }

    /// Function values (closures, lambdas); inside a unit they make that unit the function.
    pub(crate) fn function_value_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Go => &["func_literal"],
            Self::Lua => &["function_definition"],
            Self::Nix => &["function_expression"],
            Self::Rust => &["closure_expression"],
            Self::Bash | Self::Yaml | Self::Toml => &[],
        }
    }

    /// Lines of these kinds may sit between a leading comment and its unit.
    pub(crate) fn attribute_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["attribute_item"],
            _ => &[],
        }
    }
}

/// Result of mapping a path (and optionally its first line) to a grammar.
#[derive(Clone, Debug)]
pub enum Detected {
    Supported(Arc<Grammar>, String),
    UnsupportedDialect(String),
    NotInScope,
    /// The extension alone is not enough; call [`Languages::detect_content`] with the first line.
    NeedsContent,
}

impl Languages {
    fn supported(&self, language: Language, dialect: &str) -> Detected {
        Detected::Supported(self.builtin(language), dialect.to_owned())
    }

    /// Builtin extensions win; external file names and then extensions come after them.
    pub fn detect_path(&self, path: &Path) -> Detected {
        let Some(extension) = path.extension() else {
            return Detected::NeedsContent;
        };
        match extension.to_str().unwrap_or_default() {
            "go" => self.supported(Language::Go, "go"),
            "lua" => self.supported(Language::Lua, "lua"),
            "nix" => self.supported(Language::Nix, "nix"),
            "rs" => self.supported(Language::Rust, "rust"),
            "yaml" | "yml" => self.supported(Language::Yaml, "yaml"),
            "toml" => self.supported(Language::Toml, "toml"),
            "bash" => self.supported(Language::Bash, "bash"),
            "sh" => Detected::NeedsContent,
            "dash" => self.supported(Language::Bash, "dash-parsed-as-bash"),
            dialect @ ("zsh" | "ksh" | "fish" | "csh" | "tcsh") => {
                Detected::UnsupportedDialect(dialect.to_owned())
            }
            _ => self.detect_external(path),
        }
    }

    /// Decides shell scripts by shebang. POSIX sh family scripts are parsed with the Bash grammar
    /// and labelled so; Bash-only syntax in them is not reported. Without a builtin match the
    /// external file names and extensions decide.
    pub fn detect_content(&self, path: &Path, first_line: &str) -> Detected {
        let is_sh = path.extension().is_some_and(|extension| extension == "sh");
        let Some(command) = first_line.strip_prefix("#!") else {
            return if is_sh {
                self.supported(Language::Bash, "bash-assumed")
            } else {
                self.detect_external(path)
            };
        };
        let mut words = command.split_whitespace();
        let mut interpreter = words.next().unwrap_or_default().rsplit('/').next();
        if interpreter == Some("env") {
            interpreter = words.find(|word| !word.starts_with('-') && !word.contains('='));
        }
        match interpreter.unwrap_or_default() {
            "bash" => self.supported(Language::Bash, "bash"),
            "sh" => self.supported(Language::Bash, "sh-parsed-as-bash"),
            "dash" => self.supported(Language::Bash, "dash-parsed-as-bash"),
            "ash" | "busybox" => self.supported(Language::Bash, "ash-parsed-as-bash"),
            dialect @ ("zsh" | "ksh" | "mksh") => Detected::UnsupportedDialect(dialect.to_owned()),
            _ => self.detect_external(path),
        }
    }

    /// First enabled external whose `filenames` match the base name, else whose extension matches.
    fn detect_external(&self, path: &Path) -> Detected {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let extension = path.extension().and_then(|extension| extension.to_str());
        self.externals
            .iter()
            .find(|grammar| {
                grammar
                    .filenames
                    .iter()
                    .any(|pattern| glob_match(pattern, name))
            })
            .or_else(|| {
                self.externals.iter().find(|grammar| {
                    extension.is_some_and(|extension| {
                        grammar.extensions.iter().any(|known| known == extension)
                    })
                })
            })
            .map_or(Detected::NotInScope, |grammar| {
                Detected::Supported(Arc::clone(grammar), grammar.name.clone())
            })
    }
}

/// Matches a repository-relative path: `*` and `?` stay within one path component, `**`
/// crosses components (`**/` also matches no directory).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn matches(pattern: &[u8], path: &[u8]) -> bool {
        match pattern {
            [] => path.is_empty(),
            [b'*', b'*', b'/', rest @ ..] => (0..=path.len())
                .filter(|&index| index == 0 || path[index - 1] == b'/')
                .any(|index| matches(rest, &path[index..])),
            [b'*', b'*', rest @ ..] => (0..=path.len()).any(|index| matches(rest, &path[index..])),
            [b'*', rest @ ..] => (0..=path.len())
                .take_while(|&index| index == 0 || path[index - 1] != b'/')
                .any(|index| matches(rest, &path[index..])),
            [b'?', rest @ ..] => {
                matches!(path, [first, ..] if *first != b'/') && matches(rest, &path[1..])
            }
            [first, rest @ ..] => path.first() == Some(first) && matches(rest, &path[1..]),
        }
    }
    matches(pattern.as_bytes(), path.as_bytes())
}

#[derive(Debug)]
pub enum ParseError {
    GrammarLoad(tree_sitter::LanguageError),
    NoTree,
    Syntax,
    /// The parse was cancelled at its deadline.
    Timeout,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GrammarLoad(error) => write!(formatter, "failed to load grammar: {error}"),
            Self::NoTree => formatter.write_str("parser returned no tree"),
            Self::Syntax => formatter.write_str("source contains syntax errors"),
            Self::Timeout => formatter.write_str("parse did not finish before its deadline"),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::GrammarLoad(error) => Some(error),
            Self::NoTree | Self::Syntax | Self::Timeout => None,
        }
    }
}

pub fn parse(language: Language, source: &str) -> Result<tree_sitter::Tree, ParseError> {
    let tree = parse_partial(language, source)?;
    if tree.root_node().has_error() {
        return Err(ParseError::Syntax);
    }
    Ok(tree)
}

/// Like [`parse`], but keeps a tree that contains syntax errors so the rest can still be used.
pub fn parse_partial(language: Language, source: &str) -> Result<tree_sitter::Tree, ParseError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts())
        .map_err(ParseError::GrammarLoad)?;
    parser.parse(source, None).ok_or(ParseError::NoTree)
}

/// Parses with a fresh parser (a cancelled one would resume its old parse), stopping once
/// `should_stop` returns true; tree-sitter asks it about every 100 parse operations.
pub(crate) fn parse_with_stop(
    grammar: &Grammar,
    source: &str,
    mut should_stop: impl FnMut() -> bool,
) -> Result<tree_sitter::Tree, ParseError> {
    // Run::load reports grammars that fail to load before anything is parsed.
    let language = grammar.language().map_err(|_| ParseError::NoTree)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .map_err(ParseError::GrammarLoad)?;
    let bytes = source.as_bytes();
    let mut progress = |_: &tree_sitter::ParseState| {
        if should_stop() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    parser
        .parse_with_options(
            &mut |index, _| bytes.get(index..).unwrap_or_default(),
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
        )
        .ok_or(ParseError::NoTree)
}

/// Like [`parse_partial`] for any grammar, cancelled with [`ParseError::Timeout`] after `limit`.
pub fn parse_with_deadline(
    grammar: &Grammar,
    source: &str,
    limit: Duration,
) -> Result<tree_sitter::Tree, ParseError> {
    // An overflowing deadline means none.
    let deadline = Instant::now().checked_add(limit);
    let passed = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    // Progress is only checked every 100 operations, so a small file could outrun a zero limit.
    if passed() {
        return Err(ParseError::Timeout);
    }
    match parse_with_stop(grammar, source, passed) {
        Err(ParseError::NoTree) if passed() => Err(ParseError::Timeout),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::{Grammar, Language, ParseError, glob_match, parse_with_deadline, parse_with_stop};
    use std::time::Duration;

    #[test]
    fn zero_limit_times_out_before_parsing() {
        let go = Grammar::builtin(Language::Go);
        assert!(matches!(
            parse_with_deadline(&go, "package p\n", Duration::ZERO),
            Err(ParseError::Timeout)
        ));
    }

    #[test]
    fn cancelled_parse_leaves_the_next_parse_correct() {
        let go = Grammar::builtin(Language::Go);
        let long: String = (0..1000)
            .map(|index| format!("func f{index}() int {{ return {index} }}\n"))
            .collect();
        let long = format!("package p\n{long}");
        let mut asked = 0;
        let stopped = parse_with_stop(&go, &long, || {
            asked += 1;
            true
        });
        assert!(matches!(stopped, Err(ParseError::NoTree)));
        assert!(asked > 0, "the progress callback ran");
        let tree = parse_with_deadline(&go, "package q\n\nvar x = 1\n", Duration::MAX).unwrap();
        assert!(!tree.root_node().has_error());
        assert_eq!(tree.root_node().kind(), "source_file");
        assert_eq!(tree.root_node().named_child_count(), 2);
    }

    #[test]
    fn globs_respect_path_components() {
        assert!(glob_match(".mise/tasks/lib/*", ".mise/tasks/lib/render"));
        assert!(!glob_match(
            ".mise/tasks/lib/*",
            ".mise/tasks/lib/sub/render"
        ));
        assert!(glob_match("**/lib/*", ".mise/tasks/lib/render"));
        assert!(glob_match("**/lib/*", "lib/render"));
        assert!(!glob_match("**/lib/*", "xlib/render"));
        assert!(glob_match("scripts/**", "scripts/a/b.sh"));
        assert!(glob_match("a?c", "abc") && !glob_match("a?c", "a/c"));
        assert!(!glob_match("*.sh", "dir/a.sh"));
    }
}
