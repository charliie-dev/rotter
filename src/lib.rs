use std::fmt;

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
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;
    if tree.root_node().has_error() {
        return Err(ParseError::Syntax);
    }
    Ok(tree)
}
