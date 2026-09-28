mod comments;
mod git;
pub mod json;

pub use comments::{Change, error_lines, full_units, units};
pub use git::{Mode, Options, Report, extract};

use std::fmt;
use std::path::Path;

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

    /// Node kinds treated as a unit that comments can belong to.
    fn unit_kinds(self) -> &'static [&'static str] {
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
    fn function_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Go => &["function_declaration", "method_declaration"],
            Self::Lua => &["function_declaration"],
            Self::Bash => &["function_definition"],
            Self::Rust => &["function_item"],
            Self::Nix | Self::Yaml | Self::Toml => &[],
        }
    }

    /// Function values (closures, lambdas); inside a unit they make that unit the function.
    fn function_value_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Go => &["func_literal"],
            Self::Lua => &["function_definition"],
            Self::Nix => &["function_expression"],
            Self::Rust => &["closure_expression"],
            Self::Bash | Self::Yaml | Self::Toml => &[],
        }
    }

    /// Lines of these kinds may sit between a leading comment and its unit.
    fn attribute_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["attribute_item"],
            _ => &[],
        }
    }
}

/// Result of mapping a path (and optionally its first line) to a checked language.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Detected {
    Supported(Language, &'static str),
    UnsupportedDialect(String),
    NotInScope,
    /// The extension alone is not enough; call [`detect_content`] with the first line.
    NeedsContent,
}

pub fn detect_path(path: &Path) -> Detected {
    let Some(extension) = path.extension() else {
        return Detected::NeedsContent;
    };
    match extension.to_str().unwrap_or_default() {
        "go" => Detected::Supported(Language::Go, "go"),
        "lua" => Detected::Supported(Language::Lua, "lua"),
        "nix" => Detected::Supported(Language::Nix, "nix"),
        "rs" => Detected::Supported(Language::Rust, "rust"),
        "yaml" | "yml" => Detected::Supported(Language::Yaml, "yaml"),
        "toml" => Detected::Supported(Language::Toml, "toml"),
        "bash" => Detected::Supported(Language::Bash, "bash"),
        "sh" => Detected::NeedsContent,
        "dash" => Detected::Supported(Language::Bash, "dash-parsed-as-bash"),
        dialect @ ("zsh" | "ksh" | "fish" | "csh" | "tcsh") => {
            Detected::UnsupportedDialect(dialect.to_owned())
        }
        _ => Detected::NotInScope,
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

/// Decides shell scripts by shebang. POSIX sh family scripts are parsed with the Bash grammar and
/// labelled so; Bash-only syntax in them is not reported.
pub fn detect_content(path: &Path, first_line: &str) -> Detected {
    let is_sh = path.extension().is_some_and(|extension| extension == "sh");
    let Some(command) = first_line.strip_prefix("#!") else {
        return if is_sh {
            Detected::Supported(Language::Bash, "bash-assumed")
        } else {
            Detected::NotInScope
        };
    };
    let mut words = command.split_whitespace();
    let mut interpreter = words.next().unwrap_or_default().rsplit('/').next();
    if interpreter == Some("env") {
        interpreter = words.find(|word| !word.starts_with('-') && !word.contains('='));
    }
    match interpreter.unwrap_or_default() {
        "bash" => Detected::Supported(Language::Bash, "bash"),
        "sh" => Detected::Supported(Language::Bash, "sh-parsed-as-bash"),
        "dash" => Detected::Supported(Language::Bash, "dash-parsed-as-bash"),
        "ash" | "busybox" => Detected::Supported(Language::Bash, "ash-parsed-as-bash"),
        dialect @ ("zsh" | "ksh" | "mksh") => Detected::UnsupportedDialect(dialect.to_owned()),
        _ => Detected::NotInScope,
    }
}

#[derive(Debug)]
pub enum ParseError {
    GrammarLoad(tree_sitter::LanguageError),
    NoTree,
    Syntax,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GrammarLoad(error) => write!(formatter, "failed to load grammar: {error}"),
            Self::NoTree => formatter.write_str("parser returned no tree"),
            Self::Syntax => formatter.write_str("source contains syntax errors"),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::GrammarLoad(error) => Some(error),
            Self::NoTree | Self::Syntax => None,
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
    let grammar = match language {
        Language::Go => tree_sitter_go::LANGUAGE,
        Language::Lua => tree_sitter_lua::LANGUAGE,
        Language::Nix => tree_sitter_nix::LANGUAGE,
        Language::Bash => tree_sitter_bash::LANGUAGE,
        Language::Yaml => tree_sitter_yaml::LANGUAGE,
        Language::Toml => tree_sitter_toml_ng::LANGUAGE,
        Language::Rust => tree_sitter_rust::LANGUAGE,
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&grammar.into())
        .map_err(ParseError::GrammarLoad)?;
    parser.parse(source, None).ok_or(ParseError::NoTree)
}

#[cfg(test)]
mod tests {
    use super::glob_match;

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
