use crate::json::Json;
use crate::{Grammar, Language};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use tree_sitter::{Node, Point, Tree};

/// A changed region on one side of a diff, in 0-based rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    Rows(Range<usize>),
    /// Lines exist only on the other side, between rows `n - 1` and `n` of this side.
    Gap(usize),
}

fn has(kinds: &[String], kind: &str) -> bool {
    kinds.iter().any(|known| known == kind)
}

/// Visits `root` and its descendants in document order; `visit` returns whether to go into the
/// node's children. A tree cursor instead of recursion, so a deeply nested file cannot exhaust
/// the stack (an overflow aborts, which no panic guard catches).
fn preorder<'t>(root: Node<'t>, mut visit: impl FnMut(Node<'t>) -> bool) {
    let mut cursor = root.walk();
    loop {
        if visit(cursor.node()) && cursor.goto_first_child() {
            continue;
        }
        // The cursor never leaves `root`: at it, both moves below fail.
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

/// Whether a syntax-error byte range touches a unit's; both are end-exclusive. An ERROR must share
/// a byte, while a zero-width MISSING node counts inside the unit or on either edge.
fn touches(error: &Range<usize>, unit: Range<usize>) -> bool {
    if error.is_empty() {
        unit.start <= error.start && error.start <= unit.end
    } else {
        error.start < unit.end && unit.start < error.end
    }
}

/// A name as reported in JSON: at most 80 characters, so one long identifier cannot bloat the
/// report. Matching always uses the full name.
fn shown(name: &str) -> String {
    name.chars().take(80).collect()
}

/// Last row that holds text of `node`; Rust doc comments end at column 0 of the next row.
fn last_row(node: Node<'_>) -> usize {
    last_row_of(node.start_position(), node.end_position())
}

fn last_row_of(start: Point, end: Point) -> usize {
    if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    }
}

/// At most this many units are added per changed name; the rest are counted.
const MAX_REFERENCES: usize = 20;

struct Selected<'t> {
    node: Node<'t>,
    changed: BTreeSet<usize>,
    gaps: BTreeSet<usize>,
    /// Set when the unit was selected because it uses the name of a changed unit.
    reference: Option<String>,
    omitted_references: usize,
    /// Full mode: only these comments (by start byte) are reported with the unit.
    only: Option<BTreeSet<usize>>,
}

impl<'t> Selected<'t> {
    fn new(node: Node<'t>, reference: Option<String>) -> Self {
        Self {
            node,
            changed: BTreeSet::new(),
            gaps: BTreeSet::new(),
            reference,
            omitted_references: 0,
            only: None,
        }
    }
}

struct File<'t> {
    grammar: &'t Grammar,
    source: &'t str,
    root: Node<'t>,
    line_starts: Vec<usize>,
    comments: Vec<Node<'t>>,
    /// Standalone comments.
    by_start_row: HashMap<usize, Node<'t>>,
    by_last_row: HashMap<usize, Node<'t>>,
    attribute_rows: BTreeSet<usize>,
    comment_rows: BTreeSet<usize>,
    changed_rows: BTreeSet<usize>,
    identifiers: Vec<Node<'t>>,
    /// Byte ranges of ERROR and MISSING nodes in a partially parsed tree.
    errors: Vec<Range<usize>>,
    /// Full mode: unit text longer than this many lines is cut.
    text_limit: Option<usize>,
}

/// Unit text in full mode is cut after this many lines; the range still covers the whole unit.
const FULL_TEXT_LINES: usize = 80;

/// 1-based lines of syntax errors, for reporting a partial parse.
pub fn error_lines(tree: &Tree) -> Vec<usize> {
    let mut rows = BTreeSet::new();
    preorder(tree.root_node(), |node| {
        if node.is_error() || node.is_missing() {
            rows.insert(node.start_position().row + 1);
            return false;
        }
        node.has_error()
    });
    rows.into_iter().collect()
}

/// Full mode: every unit that has a comment, each with only the comments that belong to it.
pub fn full_units(grammar: &Grammar, source: &str, tree: &Tree) -> Json {
    let mut file = File::new(grammar, source, tree);
    file.text_limit = Some(FULL_TEXT_LINES);
    let mut selected: BTreeMap<(usize, usize), Selected<'_>> = BTreeMap::new();
    for comment in &file.comments {
        let unit = if file.standalone(*comment) {
            file.unit_for(*comment)
        } else {
            file.node_at_row(comment.start_position().row)
                .map_or(*comment, |node| file.unit_for(node))
        };
        selected
            .entry((file.start_byte(unit), unit.end_byte()))
            .or_insert_with(|| Selected::new(unit, None))
            .only
            .get_or_insert_with(BTreeSet::new)
            .insert(comment.start_byte());
    }
    Json::Arr(selected.values().map(|item| file.unit_json(item)).collect())
}

/// Maps the changes on one side of a file to units and their related comments.
pub fn units(grammar: &Grammar, source: &str, tree: &Tree, changes: &[Change]) -> Json {
    let mut file = File::new(grammar, source, tree);
    for change in changes {
        if let Change::Rows(rows) = change {
            file.changed_rows.extend(rows.clone());
        }
    }

    let mut selected: BTreeMap<(usize, usize), Selected<'_>> = BTreeMap::new();
    let mut select = |node, row: Option<usize>, gap: Option<usize>| {
        let entry = selected
            .entry((file.start_byte(node), node.end_byte()))
            .or_insert_with(|| Selected::new(node, None));
        entry.changed.extend(row);
        entry.gaps.extend(gap);
    };
    for change in changes {
        match change {
            Change::Rows(rows) => {
                for row in rows.clone() {
                    if let Some(node) = file.node_at_row(row) {
                        select(file.unit_for(node), Some(row), None);
                    }
                }
            }
            Change::Gap(row) => {
                if let Some(node) = file.unit_across(*row) {
                    select(node, None, Some(*row));
                }
            }
        }
    }

    // Keep only outermost selections; nested ones are already inside their container's text.
    // A nested one still names what changed, so its name looks for users too.
    let mut kept: Vec<Selected<'_>> = Vec::new();
    let mut nested: Vec<(usize, Node<'_>)> = Vec::new();
    for (_, item) in selected {
        match kept
            .iter()
            .position(|outer| file.contains(outer.node, item.node))
        {
            Some(index) => {
                if file.code_changed(&item) {
                    nested.push((index, item.node));
                }
                let outer = &mut kept[index];
                outer.changed.extend(item.changed);
                outer.gaps.extend(item.gaps);
            }
            None => kept.push(item),
        }
    }
    let mut owners: Vec<(usize, Node<'_>)> = (0..kept.len())
        .filter(|index| file.code_changed(&kept[*index]))
        .map(|index| (index, kept[index].node))
        .collect();
    owners.extend(nested);
    let references = file.references(&mut kept, &owners);
    kept.extend(references);
    Json::Arr(kept.iter().map(|item| file.unit_json(item)).collect())
}

impl<'t> File<'t> {
    /// Only a selection that contains a unit shows its text; one nested inside it does not.
    fn contains(&self, outer: Node<'_>, inner: Node<'_>) -> bool {
        self.start_byte(outer) <= self.start_byte(inner) && inner.end_byte() <= outer.end_byte()
    }

    /// Units elsewhere in the file that use a changed name; their comments may describe it.
    /// `owners` pairs each changed unit whose name is searched with the index of the kept unit
    /// that shows it. Every name's users are collected first and taken in document order,
    /// enclosing before nested, so the result does not depend on which name is declared first.
    fn references(
        &self,
        kept: &mut [Selected<'t>],
        owners: &[(usize, Node<'t>)],
    ) -> Vec<Selected<'t>> {
        let mut names = Vec::new();
        let mut candidates: Vec<(usize, Node<'t>)> = Vec::new();
        let mut seen = HashSet::new();
        for &(index, node) in owners {
            let Some(name) = self
                .name_text(node)
                .filter(|name| name.chars().count() >= 2)
            else {
                continue;
            };
            let owner = names.len();
            for identifier in &self.identifiers {
                if self.source[identifier.byte_range()] != name
                    || self.contains(kept[index].node, *identifier)
                {
                    continue;
                }
                let unit = self.unit_for(*identifier);
                if seen.insert((owner, unit.id())) {
                    candidates.push((owner, unit));
                }
            }
            names.push((index, name));
        }
        candidates
            .sort_by_key(|&(owner, unit)| (self.start_byte(unit), Reverse(unit.end_byte()), owner));
        let mut added = vec![0; names.len()];
        // Per kept unit, the units past a cap; counted once every name has picked its users,
        // since another name may select one of them, or a unit containing it, after all.
        let mut omitted: Vec<Vec<Node<'t>>> = vec![Vec::new(); kept.len()];
        let mut references: Vec<Selected<'t>> = Vec::new();
        for (owner, unit) in candidates {
            let mut selected = kept.iter().chain(&references);
            if selected
                .clone()
                .any(|other| self.contains(other.node, unit))
            {
                continue;
            }
            // It holds a unit already selected (say, a method of this impl), so selecting it
            // too would repeat that text; it stays out of the report, and is counted as such.
            // Units nest or stay apart, so holding is the only way to overlap one.
            if added[owner] == MAX_REFERENCES
                || selected.any(|other| self.contains(unit, other.node))
            {
                let list = &mut omitted[names[owner].0];
                if !list.contains(&unit) {
                    list.push(unit);
                }
                continue;
            }
            added[owner] += 1;
            references.push(Selected::new(unit, Some(names[owner].1.clone())));
        }
        for (item, omitted) in kept.iter_mut().zip(omitted) {
            item.omitted_references = omitted
                .iter()
                .filter(|unit| {
                    !references
                        .iter()
                        .any(|other| self.contains(other.node, **unit))
                })
                .count();
        }
        references
    }

    fn new(grammar: &'t Grammar, source: &'t str, tree: &'t Tree) -> Self {
        let root = tree.root_node();
        let mut file = File {
            grammar,
            source,
            root,
            line_starts: std::iter::once(0)
                .chain(source.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            comments: Vec::new(),
            by_start_row: HashMap::new(),
            by_last_row: HashMap::new(),
            attribute_rows: BTreeSet::new(),
            comment_rows: BTreeSet::new(),
            changed_rows: BTreeSet::new(),
            identifiers: Vec::new(),
            errors: Vec::new(),
            text_limit: None,
        };
        preorder(root, |node| file.scan(node));
        for comment in file.comments.clone() {
            if file.standalone(comment) {
                file.by_start_row
                    .entry(comment.start_position().row)
                    .or_insert(comment);
                file.by_last_row.insert(last_row(comment), comment);
                file.comment_rows
                    .extend(comment.start_position().row..=last_row(comment));
            }
        }
        file
    }

    /// Returns whether its children are scanned (not a comment's).
    fn scan(&mut self, node: Node<'t>) -> bool {
        if node.is_error() || node.is_missing() {
            self.errors.push(node.byte_range());
        }
        if has(&self.grammar.comment_kinds, node.kind()) {
            self.comments.push(node);
            return false;
        }
        let kind = node.kind();
        if self.grammar.references
            && node.child_count() == 0
            && (kind.ends_with("identifier") || kind == "variable_name" || kind == "word")
        {
            self.identifiers.push(node);
        }
        if has(&self.grammar.attribute_kinds, node.kind()) {
            self.attribute_rows
                .extend(node.start_position().row..=last_row(node));
        }
        true
    }

    fn line(&self, row: usize) -> &'t str {
        let start = self.line_starts[row];
        let end = self
            .line_starts
            .get(row + 1)
            .copied()
            .unwrap_or(self.source.len());
        self.source[start..end].trim_end_matches(['\n', '\r'])
    }

    fn standalone(&self, node: Node<'_>) -> bool {
        let start = node.start_position();
        self.line(start.row)[..start.column].trim().is_empty()
    }

    fn is_unit(&self, node: Node<'_>) -> bool {
        has(&self.grammar.unit_kinds, node.kind())
    }

    /// Row where a unit begins, including attribute lines directly above it.
    fn start_row(&self, node: Node<'_>) -> usize {
        let mut row = node.start_position().row;
        while row > 0 && self.attribute_rows.contains(&(row - 1)) {
            row -= 1;
        }
        row
    }

    fn start_byte(&self, node: Node<'_>) -> usize {
        let row = self.start_row(node);
        if row == node.start_position().row {
            return node.start_byte();
        }
        let line = self.line(row);
        self.line_starts[row] + line.len() - line.trim_start().len()
    }

    fn node_at_row(&self, row: usize) -> Option<Node<'t>> {
        let line = self.line(row);
        let column = line.len() - line.trim_start().len();
        let point = Point::new(row, column);
        let node = self.root.descendant_for_point_range(point, point)?;
        (node != self.root || !line.trim().is_empty()).then_some(node)
    }

    /// Enclosing units, innermost first; units inside a function body collapse into it.
    fn chain(&self, node: Node<'t>) -> Vec<Node<'t>> {
        let mut chain = Vec::new();
        let mut value_at = None;
        let mut current = Some(node);
        while let Some(node) = current {
            if self.is_unit(node) {
                chain.push(node);
            } else if value_at.is_none() && has(&self.grammar.value_kinds, node.kind()) {
                value_at = Some(chain.len());
            }
            current = node.parent();
        }
        let declared = chain
            .iter()
            .position(|node| has(&self.grammar.function_kinds, node.kind()));
        // A function value only counts when a unit (binding, var, field) holds it.
        let value = value_at.filter(|index| *index < chain.len());
        if let Some(index) = [declared, value].into_iter().flatten().min() {
            chain.drain(..index);
        }
        chain
    }

    fn top_level(&self, node: Node<'t>) -> Node<'t> {
        let mut node = node;
        while let Some(parent) = node.parent() {
            if parent == self.root {
                break;
            }
            node = parent;
        }
        node
    }

    fn unit_or_top_level(&self, node: Node<'t>) -> Node<'t> {
        self.chain(node)
            .first()
            .copied()
            .unwrap_or_else(|| self.top_level(node))
    }

    fn unit_for(&self, node: Node<'t>) -> Node<'t> {
        let mut current = Some(node);
        while let Some(candidate) = current {
            if has(&self.grammar.comment_kinds, candidate.kind()) {
                // A file-level comment is its own unit even right above a declaration, since
                // leading() never attaches it there.
                if self.standalone(candidate)
                    && !self.file_level(candidate)
                    && let Some(unit) = self.follow(candidate)
                {
                    return unit;
                }
                return self.chain(candidate).first().copied().unwrap_or(candidate);
            }
            current = candidate.parent();
        }
        self.unit_or_top_level(node)
    }

    /// The unit that a standalone comment block sits directly above, if any.
    fn follow(&self, comment: Node<'t>) -> Option<Node<'t>> {
        let mut row = last_row(comment) + 1;
        loop {
            if row >= self.line_starts.len() {
                return None;
            }
            if let Some(next) = self.by_start_row.get(&row) {
                // A file-level comment ends the block: nothing above it attaches to the code
                // below, since leading() stops there too.
                if self.file_level(*next) {
                    return None;
                }
                row = last_row(*next) + 1;
            } else if self.attribute_rows.contains(&row) {
                row += 1;
            } else {
                break;
            }
        }
        let node = self.node_at_row(row)?;
        if self.line(row).trim().is_empty() {
            return None;
        }
        Some(self.unit_or_top_level(node))
    }

    /// Innermost unit spanning both sides of a gap between rows `row - 1` and `row`.
    fn unit_across(&self, row: usize) -> Option<Node<'t>> {
        if row == 0 || row >= self.line_starts.len() {
            return None;
        }
        let scope = |node: Node<'t>| {
            let mut chain = self.chain(node);
            chain.push(self.top_level(node));
            chain
        };
        let (above, below) = (self.node_at_row(row - 1)?, self.node_at_row(row)?);
        // Both rows may belong to one unit only through a leading comment block.
        let unit = self.unit_for(above);
        if unit == self.unit_for(below) {
            return Some(unit);
        }
        let below = scope(below);
        scope(above).into_iter().find(|node| below.contains(node))
    }

    /// Inner docs and shebangs describe the enclosing file or module, not the next item.
    fn file_level(&self, comment: Node<'t>) -> bool {
        let text = &self.source[comment.byte_range()];
        comment_style(comment, text) == "doc_inner"
            || directive(self.grammar, text) == Some("shebang")
    }

    fn leading(&self, node: Node<'t>) -> Vec<Node<'t>> {
        let mut found = Vec::new();
        let mut row = self.start_row(node);
        loop {
            while row > 0 && self.attribute_rows.contains(&(row - 1)) {
                row -= 1;
            }
            let Some(comment) = row
                .checked_sub(1)
                .and_then(|above| self.by_last_row.get(&above))
            else {
                break;
            };
            if comment.start_byte() >= node.start_byte() || self.file_level(*comment) {
                break;
            }
            found.push(*comment);
            row = comment.start_position().row;
        }
        found.reverse();
        found
    }

    fn trailing(&self, node: Node<'t>) -> Option<Node<'t>> {
        let end = node.end_byte();
        self.comments.iter().copied().find(|comment| {
            comment.start_byte() >= end
                && comment.start_position().row == last_row(node)
                && self.source[end..comment.start_byte()]
                    .trim_matches(|character: char| {
                        character.is_whitespace() || ",;".contains(character)
                    })
                    .is_empty()
        })
    }

    fn enclosing(&self, node: Node<'t>) -> Vec<Node<'t>> {
        let mut found = Vec::new();
        let mut current = node.parent();
        while let Some(parent) = current {
            if self.is_unit(parent) {
                found.push(parent);
            }
            current = parent.parent();
        }
        found
    }

    fn range(&self, start: usize, end: usize, first: usize, last: usize) -> Json {
        Json::Obj(vec![
            ("lines", vec![first + 1, last + 1].into()),
            ("bytes", vec![start, end].into()),
        ])
    }

    fn node_range(&self, node: Node<'_>) -> Json {
        self.range(
            self.start_byte(node),
            node.end_byte(),
            self.start_row(node),
            last_row(node),
        )
    }

    fn code_changed(&self, item: &Selected<'_>) -> bool {
        !item.gaps.is_empty()
            || item
                .changed
                .iter()
                .any(|row| !self.comment_rows.contains(row))
    }

    fn name(&self, node: Node<'_>) -> Json {
        self.name_text(node).map(|name| shown(&name)).into()
    }

    fn name_text(&self, node: Node<'_>) -> Option<String> {
        let name = ["name", "key", "attrpath"]
            .iter()
            .find_map(|field| node.child_by_field_name(field))
            .or_else(|| {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind().ends_with("key") || child.kind().ends_with("_spec"))
            })?;
        if name.kind().ends_with("_spec") {
            return self.name_text(name);
        }
        // The full first line: units() matches identifiers against it exactly, so only the
        // JSON fields are shortened (shown).
        let text = &self.source[name.byte_range()];
        Some(text.lines().next().unwrap_or_default().to_owned())
    }

    /// One JSON entry for a run of adjacent comments that share a relation and style.
    fn comment_json(&self, group: &[Node<'_>], relation: &str) -> Json {
        let (first, last) = (group[0], group[group.len() - 1]);
        let text = &self.source[first.start_byte()..last.end_byte()];
        let rows = first.start_position().row..=last_row(last);
        Json::Obj(vec![
            ("relation", relation.into()),
            ("style", comment_style(first, text).into()),
            ("directive", directive(self.grammar, text).into()),
            (
                "changed",
                rows.into_iter()
                    .any(|row| self.changed_rows.contains(&row))
                    .into(),
            ),
            (
                "range",
                self.range(
                    first.start_byte(),
                    last.end_byte(),
                    first.start_position().row,
                    last_row(last),
                ),
            ),
            ("text", text.into()),
        ])
    }

    /// Consecutive standalone line comments read as one block; directives stay separate.
    fn joins(&self, previous: Node<'_>, next: Node<'_>) -> bool {
        let text = |node: Node<'_>| &self.source[node.byte_range()];
        let plain =
            |node: Node<'_>| self.standalone(node) && directive(self.grammar, text(node)).is_none();
        plain(previous)
            && plain(next)
            && comment_style(previous, text(previous)) == comment_style(next, text(next))
            && comment_style(next, text(next)) != "block"
            && next.start_position().row == last_row(previous) + 1
    }

    fn overlaps_error(&self, node: Node<'_>) -> bool {
        let (start, end) = (self.start_byte(node), node.end_byte());
        self.errors.iter().any(|error| touches(error, start..end))
    }

    fn unit_json(&self, item: &Selected<'t>) -> Json {
        let node = item.node;
        let enclosing = self.enclosing(node);
        let mut related: BTreeMap<usize, (Node<'t>, &str)> = BTreeMap::new();
        let mut add = |comment: Node<'t>, relation: &'static str| {
            related
                .entry(comment.start_byte())
                .or_insert((comment, relation));
        };
        for comment in self.leading(node) {
            add(comment, "leading");
        }
        for comment in self.comments.iter().copied().filter(|comment| {
            comment.start_byte() >= node.start_byte() && comment.end_byte() <= node.end_byte()
        }) {
            add(comment, "inside");
        }
        if let Some(comment) = self.trailing(node) {
            add(comment, "trailing");
        }
        for outer in &enclosing {
            for comment in self.leading(*outer) {
                add(comment, "enclosing_leading");
            }
        }
        if let Some(only) = &item.only {
            for comment in self.comments.iter().copied() {
                if only.contains(&comment.start_byte()) {
                    add(comment, "nearby");
                }
            }
            related.retain(|start, _| only.contains(start));
        }
        let mut groups: Vec<(Vec<Node<'t>>, &str)> = Vec::new();
        for (comment, relation) in related.into_values() {
            match groups.last_mut() {
                Some((group, previous))
                    if *previous == relation && self.joins(group[group.len() - 1], comment) =>
                {
                    group.push(comment);
                }
                _ => groups.push((vec![comment], relation)),
            }
        }
        let comments: Vec<Json> = groups
            .iter()
            .map(|(group, relation)| self.comment_json(group, relation))
            .collect();
        let mut text = &self.source[self.start_byte(node)..node.end_byte()];
        let cut = self
            .text_limit
            .and_then(|limit| text.match_indices('\n').nth(limit - 1))
            .map(|(index, _)| index);
        let truncated = cut.is_some();
        if let Some(index) = cut {
            text = &text[..index];
        }
        Json::Obj(vec![
            ("kind", node.kind().into()),
            ("name", self.name(node)),
            ("range", self.node_range(node)),
            (
                "selected_by",
                (if item.only.is_some() {
                    "full"
                } else if item.reference.is_some() {
                    "reference"
                } else {
                    "change"
                })
                .into(),
            ),
            (
                "referenced_name",
                item.reference.as_deref().map(shown).into(),
            ),
            ("omitted_reference_units", item.omitted_references.into()),
            (
                "changed_lines",
                item.changed
                    .iter()
                    .map(|row| row + 1)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            (
                "gaps_between_lines",
                Json::Arr(
                    item.gaps
                        .iter()
                        .map(|row| vec![*row, row + 1].into())
                        .collect(),
                ),
            ),
            (
                "enclosing",
                Json::Arr(
                    enclosing
                        .iter()
                        .map(|outer| {
                            Json::Obj(vec![
                                ("kind", outer.kind().into()),
                                ("name", self.name(*outer)),
                                ("range", self.node_range(*outer)),
                                ("header", self.line(outer.start_position().row).into()),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("overlaps_syntax_error", self.overlaps_error(node).into()),
            ("text", text.into()),
            ("text_truncated", truncated.into()),
            ("comments", Json::Arr(comments)),
        ])
    }
}

fn comment_style(comment: Node<'_>, text: &str) -> &'static str {
    let mut cursor = comment.walk();
    for child in comment.children(&mut cursor) {
        match child.kind() {
            "outer_doc_comment_marker" => return "doc_outer",
            "inner_doc_comment_marker" => return "doc_inner",
            _ => {}
        }
    }
    let lua_long = text
        .strip_prefix("--[")
        .is_some_and(|rest| rest.trim_start_matches('=').starts_with('['));
    if text.starts_with("/*") || lua_long {
        "block"
    } else {
        "line"
    }
}

/// Comments that tools read as instructions; they must not be treated as prose.
fn directive<'g>(grammar: &'g Grammar, text: &str) -> Option<&'g str> {
    let text = text.trim_end();
    let Some(language) = grammar.builtin else {
        return grammar
            .directives
            .iter()
            .find(|(prefix, _)| text.starts_with(prefix.as_str()))
            .map(|(_, label)| label.as_str());
    };
    match language {
        Language::Go => [
            "//go:",
            "//line ",
            "//export ",
            "// +build",
            "//nolint",
            "//lint:",
        ]
        .iter()
        .any(|prefix| text.starts_with(prefix))
        .then_some("go_directive"),
        Language::Lua => text.starts_with("---@").then_some("lua_annotation"),
        Language::Bash if text.starts_with("#!") => Some("shebang"),
        Language::Bash => text
            .strip_prefix('#')
            .is_some_and(|rest| rest.trim_start().starts_with("shellcheck "))
            .then_some("shellcheck_directive"),
        Language::Yaml => text
            .starts_with("# yaml-language-server:")
            .then_some("yaml_language_server"),
        Language::Toml => text.starts_with("#:schema").then_some("toml_schema"),
        Language::Nix | Language::Rust => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, Json, error_lines, full_units, last_row_of, touches, units};
    use crate::{Grammar, Language, parse_partial};
    use tree_sitter::Point;

    #[test]
    fn a_grammar_without_references_never_pulls_in_users() {
        let source = "greet() {\n  echo hi\n}\ncaller() {\n  greet\n}\n";
        let tree = parse_partial(Language::Bash, source).unwrap();
        let count = |grammar: &Grammar| match units(grammar, source, &tree, &[Change::Rows(1..2)]) {
            Json::Arr(units) => units.len(),
            other => panic!("{other}"),
        };
        let mut grammar = Grammar::builtin(Language::Bash);
        assert_eq!(count(&grammar), 2, "greet and its caller");
        grammar.references = false;
        assert_eq!(count(&grammar), 1, "greet alone");
    }

    #[test]
    fn a_node_ending_at_column_zero_of_a_later_row_ends_on_the_row_before() {
        let at = |row, column| Point { row, column };
        assert_eq!(last_row_of(at(2, 4), at(3, 0)), 2);
        assert_eq!(last_row_of(at(2, 4), at(5, 0)), 4);
        assert_eq!(last_row_of(at(2, 4), at(3, 1)), 3);
        assert_eq!(last_row_of(at(2, 0), at(2, 0)), 2);
    }

    #[test]
    fn deeply_nested_files_do_not_exhaust_the_stack() {
        // Test threads get a 2 MiB stack; a recursive walk overflows here and aborts the process.
        let depth = 100_000;
        let nested = format!(
            "fn f() -> i32 {{ {}1{} }}\n",
            "(".repeat(depth),
            ")".repeat(depth)
        );
        let grammar = Grammar::builtin(Language::Rust);
        let tree = parse_partial(Language::Rust, &nested).unwrap();
        assert!(error_lines(&tree).is_empty());
        // No comments, so no units are selected; the point is that the walk finishes.
        assert!(
            matches!(full_units(&grammar, &nested, &tree), Json::Arr(units) if units.is_empty())
        );
        // One parenthesis short: the error is found without recursing down the nesting.
        let broken = format!(
            "fn f() -> i32 {{ {}1{} }}\n",
            "(".repeat(depth),
            ")".repeat(depth - 1)
        );
        let tree = parse_partial(Language::Rust, &broken).unwrap();
        assert_eq!(error_lines(&tree), [1]);
    }

    #[test]
    fn errors_touch_a_unit_only_when_they_share_a_byte() {
        assert!(!touches(&(20..21), 10..20), "an ERROR right after the unit");
        assert!(!touches(&(9..10), 10..20), "an ERROR right before the unit");
        assert!(touches(&(19..21), 10..20));
        assert!(touches(&(5..11), 10..20));
        assert!(touches(&(20..20), 10..20));
        assert!(touches(&(10..10), 10..20));
        assert!(touches(&(15..15), 10..20));
        assert!(!touches(&(21..21), 10..20));
    }
}
