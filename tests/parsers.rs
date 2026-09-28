use rotter::{Language, ParseError, parse};
use std::ops::Range;
use tree_sitter::{Node, Point};

type ExpectedComment<'a> = (&'a str, Range<usize>, (usize, usize), (usize, usize));

fn collect_comments<'tree>(node: Node<'tree>, comments: &mut Vec<Node<'tree>>) {
    if matches!(node.kind(), "comment" | "line_comment" | "block_comment") {
        comments.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comments(child, comments);
    }
}

fn assert_comments(language: Language, source: &str, expected: &[ExpectedComment<'_>]) {
    let tree = parse(language, source).expect("valid source");
    let mut comments = Vec::new();
    collect_comments(tree.root_node(), &mut comments);
    assert_eq!(comments.len(), expected.len(), "{language:?}: {source:?}");
    for (comment, (text, bytes, start, end)) in comments.iter().zip(expected) {
        assert_eq!(&source[bytes.clone()], *text, "literal source range");
        for (offset, (row, column)) in [(bytes.start, *start), (bytes.end, *end)] {
            let prefix = &source[..offset];
            assert_eq!(prefix.bytes().filter(|byte| *byte == b'\n').count(), row);
            assert_eq!(
                prefix.rsplit('\n').next().expect("source line").len(),
                column
            );
        }
        assert_eq!(&source[comment.byte_range()], *text);
        assert_eq!(comment.byte_range(), *bytes);
        assert_eq!(comment.start_position(), Point::new(start.0, start.1));
        assert_eq!(comment.end_position(), Point::new(end.0, end.1));
    }
}

fn assert_fixture_pair(
    language: Language,
    before: &str,
    after: &str,
    comment: ExpectedComment<'_>,
) {
    assert_eq!(before.replacen('1', "2", 1), after);
    for source in [before, after] {
        assert_comments(language, source, std::slice::from_ref(&comment));
    }
}

#[test]
fn lua_keeps_an_unchanged_comment() {
    assert_fixture_pair(
        Language::Lua,
        include_str!("fixtures/lua/before/sample.lua"),
        include_str!("fixtures/lua/after/sample.lua"),
        ("-- Returns one.", 0..15, (0, 0), (0, 15)),
    );
}

#[test]
fn nix_keeps_an_unchanged_comment() {
    assert_fixture_pair(
        Language::Nix,
        include_str!("fixtures/nix/before/sample.nix"),
        include_str!("fixtures/nix/after/sample.nix"),
        ("# Returns one.", 0..14, (0, 0), (0, 14)),
    );
}

#[test]
fn bash_keeps_an_unchanged_comment() {
    assert_fixture_pair(
        Language::Bash,
        include_str!("fixtures/bash/before/sample.sh"),
        include_str!("fixtures/bash/after/sample.sh"),
        ("# Returns one.", 0..14, (0, 0), (0, 14)),
    );
}

#[test]
fn yaml_keeps_an_unchanged_comment() {
    assert_fixture_pair(
        Language::Yaml,
        include_str!("fixtures/yaml/before/sample.yaml"),
        include_str!("fixtures/yaml/after/sample.yaml"),
        ("# Returns one.", 0..14, (0, 0), (0, 14)),
    );
}

#[test]
fn toml_keeps_an_unchanged_comment() {
    assert_fixture_pair(
        Language::Toml,
        include_str!("fixtures/toml/before/sample.toml"),
        include_str!("fixtures/toml/after/sample.toml"),
        ("# Returns one.", 0..14, (0, 0), (0, 14)),
    );
}

#[test]
fn rust_keeps_an_unchanged_comment() {
    assert_fixture_pair(
        Language::Rust,
        include_str!("fixtures/rust/before/sample.rs"),
        include_str!("fixtures/rust/after/sample.rs"),
        ("// Returns one.", 0..15, (0, 0), (0, 15)),
    );
}

#[test]
fn go_string_markers_are_not_comments() {
    assert_comments(
        Language::Go,
        "// actual\npackage sample\nvar marker = \"// not /* comment */\"\n",
        &[("// actual", 0..9, (0, 0), (0, 9))],
    );
}

#[test]
fn bash_strings_and_heredocs_are_not_comments() {
    assert_comments(
        Language::Bash,
        "# actual\nvalue='# not a comment'\ncat <<'EOF'\n# not a comment\nEOF\n",
        &[("# actual", 0..8, (0, 0), (0, 8))],
    );
}

#[test]
fn lua_long_comments_are_counted_once_and_strings_are_not_comments() {
    assert_comments(
        Language::Lua,
        "--[=[actual\ncomment]=]\nlocal marker = [=[-- not a comment]=]\nlocal quoted = \"-- not a comment\"\n",
        &[("--[=[actual\ncomment]=]", 0..22, (0, 0), (1, 10))],
    );
}

#[test]
fn nix_quoted_and_indented_strings_are_not_comments() {
    assert_comments(
        Language::Nix,
        "# actual\n{ quoted = \"# not /* a comment */\"; indented = ''\n# not a comment\n''; }\n",
        &[("# actual", 0..8, (0, 0), (0, 8))],
    );
}

#[test]
fn yaml_strings_and_block_scalars_are_not_comments() {
    assert_comments(
        Language::Yaml,
        "# actual\nquoted: \"# not a comment\"\ntext: |\n  # not a comment\n",
        &[("# actual", 0..8, (0, 0), (0, 8))],
    );
}

#[test]
fn toml_string_forms_are_not_comments() {
    assert_comments(
        Language::Toml,
        "# actual\nquoted = \"# not a comment\"\nliteral = '# not a comment'\nmultiline = \"\"\"\n# not a comment\n\"\"\"\n",
        &[("# actual", 0..8, (0, 0), (0, 8))],
    );
}

// Rust doc line comments include LF; ordinary line comments exclude LF.
#[test]
fn rust_doc_and_nested_comments_are_counted_once_and_strings_are_not_comments() {
    assert_comments(
        Language::Rust,
        "//! module docs\n/// item docs\n/** block docs */\n/* outer /* inner */ end */\nfn f() { let _ = r#\"// not /* comment */\"#; let _ = \"// not a comment\"; }\n",
        &[
            ("//! module docs\n", 0..16, (0, 0), (1, 0)),
            ("/// item docs\n", 16..30, (1, 0), (2, 0)),
            ("/** block docs */", 30..47, (2, 0), (2, 17)),
            ("/* outer /* inner */ end */", 48..75, (3, 0), (3, 27)),
        ],
    );
}

#[test]
fn utf8_columns_and_crlf_ranges_are_exact() {
    // CR belongs to Go/Nix/Bash/Rust comments, but not Lua/YAML/TOML comments.
    let cases = [
        (
            Language::Go,
            "package sample\r\nvar café = 1 // café\r\n",
            ("// café\r", 30..39, (1, 14), (1, 23)),
        ),
        (
            Language::Lua,
            "local s = 'é'; -- café\r\n",
            ("-- café", 16..24, (0, 16), (0, 24)),
        ),
        (
            Language::Nix,
            "{ s = \"é\"; # café\r\n}\r\n",
            ("# café\r", 12..20, (0, 12), (0, 20)),
        ),
        (
            Language::Bash,
            "s='é' # café\r\n",
            ("# café\r", 7..15, (0, 7), (0, 15)),
        ),
        (
            Language::Yaml,
            "s: é # café\r\n",
            ("# café", 6..13, (0, 6), (0, 13)),
        ),
        (
            Language::Toml,
            "s = 'é' # café\r\n",
            ("# café", 9..16, (0, 9), (0, 16)),
        ),
        (
            Language::Rust,
            "const S: &str = \"é\"; // café\r\n",
            ("// café\r", 22..31, (0, 22), (0, 31)),
        ),
    ];
    for (language, source, expected) in cases {
        assert_comments(language, source, &[expected]);
    }
}

#[test]
fn malformed_input_is_a_syntax_error_for_every_language() {
    for (language, source) in [
        (Language::Go, "package sample\nfunc broken( {"),
        (Language::Lua, "local x = function("),
        (Language::Nix, "{ x = ; }"),
        (Language::Bash, "if true; then\n"),
        (Language::Yaml, "x: [1, 2\n"),
        (Language::Toml, "x = [1, 2\n"),
        (Language::Rust, "fn broken( {"),
    ] {
        assert!(
            matches!(parse(language, source), Err(ParseError::Syntax)),
            "{language:?} should reject malformed input"
        );
    }
}

#[test]
fn go_parses_a_comment_far_from_a_changed_return_value() {
    let before = format!(
        "package sample\n\n// Returns one.\nfunc count() int {{\n\tvalue := 0\n{}\treturn value + 1\n}}\n",
        "\tvalue += 0\n".repeat(64)
    );
    let after = before.replace("return value + 1", "return value + 2");
    assert_fixture_pair(
        Language::Go,
        &before,
        &after,
        ("// Returns one.", 16..31, (2, 0), (2, 15)),
    );
}

#[test]
fn go_keeps_an_unchanged_comment_when_the_function_body_changes() {
    assert_eq!(
        include_str!("fixtures/go/before/sample.go").replacen('1', "2", 1),
        include_str!("fixtures/go/after/sample.go")
    );
    for source in [
        include_str!("fixtures/go/before/sample.go"),
        include_str!("fixtures/go/after/sample.go"),
    ] {
        let tree = parse(Language::Go, source).expect("valid Go fixture");
        let root = tree.root_node();
        let comment = root.named_child(1).expect("leading comment");

        assert_eq!(comment.kind(), "comment");
        assert_eq!(comment.byte_range(), 16..31);
        assert_eq!(comment.start_position(), Point::new(2, 0));
        assert_eq!(comment.end_position(), Point::new(2, 15));
        assert_eq!(&source[comment.byte_range()], "// Returns one.");
        assert_eq!(
            root.named_child(2).expect("function").kind(),
            "function_declaration"
        );
    }
}
