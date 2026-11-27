use crate::json::Json;
use crate::{Grammar, Language};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// The node of `nodes` that holds byte `at`. `nodes` is in document order, none inside another.
fn holding<'t>(nodes: &[Node<'t>], at: usize) -> Option<Node<'t>> {
    let index = nodes.partition_point(|node| node.start_byte() <= at);
    nodes[..index]
        .last()
        .copied()
        .filter(|node| at < node.end_byte())
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
    attributes: Vec<Node<'t>>,
    attribute_rows: BTreeSet<usize>,
    comment_rows: BTreeSet<usize>,
    changed_rows: BTreeSet<usize>,
    identifiers: Vec<Node<'t>>,
    /// Byte ranges of ERROR and MISSING nodes in a partially parsed tree.
    errors: Vec<Range<usize>>,
    /// Full mode: unit text longer than this many lines is cut.
    text_limit: Option<usize>,
    /// covered() by (row, end, attributes): every comment or attribute on a row asks about it.
    covered_rows: RefCell<HashMap<(usize, usize, bool), bool>>,
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
    // Every comment that shares a row with code maps to that row's unit; found once per row.
    let mut at_row: HashMap<usize, Option<Node<'_>>> = HashMap::new();
    for comment in &file.comments {
        let unit = if file.standalone(*comment) {
            file.unit_for(*comment)
        } else {
            let row = comment.start_position().row;
            at_row
                .entry(row)
                .or_insert_with(|| file.node_at_row(row).map(|node| file.unit_for(node)))
                .unwrap_or(*comment)
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
    let mut kept: Vec<Selected<'_>> = Vec::new();
    for (_, item) in selected {
        match kept.iter_mut().find(|outer| {
            file.start_byte(outer.node) <= file.start_byte(item.node)
                && item.node.end_byte() <= outer.node.end_byte()
        }) {
            Some(outer) => {
                outer.changed.extend(item.changed);
                outer.gaps.extend(item.gaps);
            }
            None => kept.push(item),
        }
    }

    // Units elsewhere in the file that use a changed name; their comments may describe it.
    let overlaps = |a: Node<'_>, b: Node<'_>| {
        file.start_byte(a) < b.end_byte() && file.start_byte(b) < a.end_byte()
    };
    // Only a selection that contains a unit shows its text; one nested inside it does not.
    let contains = |outer: Node<'_>, inner: Node<'_>| {
        file.start_byte(outer) <= file.start_byte(inner) && inner.end_byte() <= outer.end_byte()
    };
    let mut references: Vec<Selected<'_>> = Vec::new();
    // Per changed unit, the units past its cap; settled once every changed unit has picked its
    // references, since another changed unit may select one of them, or a unit containing it,
    // after all.
    let mut omitted_by: Vec<(usize, Vec<Node<'_>>)> = Vec::new();
    for index in 0..kept.len() {
        let item = &kept[index];
        let Some(name) = file.name_text(item.node) else {
            continue;
        };
        if name.chars().count() < 2 || !file.code_changed(item) {
            continue;
        }
        let mut added = 0;
        let mut omitted: Vec<Node<'_>> = Vec::new();
        for identifier in &file.identifiers {
            if file.source[identifier.byte_range()] != name || overlaps(item.node, *identifier) {
                continue;
            }
            let unit = file.unit_for(*identifier);
            let mut selected = kept.iter().chain(&references);
            if selected.clone().any(|other| contains(other.node, unit)) {
                continue;
            }
            // It holds a unit already selected (say, a method of this impl), so selecting it
            // too would repeat that text; it stays out of the report, and is counted as such.
            if added == MAX_REFERENCES || selected.any(|other| overlaps(other.node, unit)) {
                // The same unit only once; a nested or enclosing one is a different unit, and the
                // final count decides which of them the report shows.
                if !omitted.contains(&unit) {
                    omitted.push(unit);
                }
                continue;
            }
            added += 1;
            references.push(Selected::new(unit, Some(name.clone())));
        }
        omitted_by.push((index, omitted));
    }
    for (index, omitted) in omitted_by {
        kept[index].omitted_references = omitted
            .iter()
            .filter(|unit| !references.iter().any(|other| contains(other.node, **unit)))
            .count();
    }
    kept.extend(references);
    Json::Arr(kept.iter().map(|item| file.unit_json(item)).collect())
}

impl<'t> File<'t> {
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
            attributes: Vec::new(),
            attribute_rows: BTreeSet::new(),
            comment_rows: BTreeSet::new(),
            changed_rows: BTreeSet::new(),
            identifiers: Vec::new(),
            errors: Vec::new(),
            text_limit: None,
            covered_rows: RefCell::new(HashMap::new()),
        };
        preorder(root, |node| file.scan(node));
        // Only attributes on lines of their own sit between a leading comment and its unit; one
        // on a line of code (`#[inline] fn f() {}`) belongs to that line.
        for attribute in file.attributes.clone() {
            let rows = attribute.start_position().row..=last_row(attribute);
            let alone = |row| file.covered(row, usize::MAX, true);
            if alone(*rows.start()) && alone(*rows.end()) {
                file.attribute_rows.extend(rows);
            }
        }
        for comment in file.comments.clone() {
            let first = comment.start_position().row;
            if file.standalone(comment) {
                file.by_start_row.entry(first).or_insert(comment);
                file.by_last_row.insert(last_row(comment), comment);
                file.comment_rows.extend(first..=last_row(comment));
            } else if file.opens_row(comment) {
                // Code follows it on its last row (`/* a\n b */ fn f`): the rows above hold none.
                file.comment_rows.extend(first..last_row(comment));
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
        // `attributes` holds no attribute inside another, so holding() can search it.
        if has(&self.grammar.attribute_kinds, node.kind())
            && self
                .attributes
                .last()
                .is_none_or(|last| last.end_byte() <= node.start_byte())
        {
            self.attributes.push(node);
        }
        true
    }

    /// Whether the text of `row` before byte column `end` lies only in comments (and with
    /// `attributes`, attributes), apart from whitespace.
    fn covered(&self, row: usize, end: usize, attributes: bool) -> bool {
        let key = (row, end, attributes);
        if let Some(known) = self.covered_rows.borrow().get(&key) {
            return *known;
        }
        let known = self.scan_covered(row, end, attributes);
        self.covered_rows.borrow_mut().insert(key, known);
        known
    }

    fn scan_covered(&self, row: usize, end: usize, attributes: bool) -> bool {
        let line = self.line(row);
        let base = self.line_starts[row];
        let end = end.min(line.len());
        let mut column = 0;
        loop {
            let Some(offset) = line[column..end].find(|character: char| !character.is_whitespace())
            else {
                return true;
            };
            column += offset;
            let at = base + column;
            let cover = holding(&self.comments, at)
                .or_else(|| attributes.then(|| holding(&self.attributes, at)).flatten());
            let Some(cover) = cover else {
                return false;
            };
            column = cover.end_byte() - base;
            if column >= end {
                return true;
            }
        }
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

    /// Nothing but whitespace before `node` on its first row.
    fn opens_row(&self, node: Node<'_>) -> bool {
        let start = node.start_position();
        self.line(start.row)[..start.column].trim().is_empty()
    }

    /// A comment alone on its lines: nothing but whitespace before it, and nothing but comments
    /// after it. One with code after it (`/* c */ fn f`) belongs to that code.
    fn standalone(&self, node: Node<'_>) -> bool {
        self.opens_row(node) && self.covered(last_row(node), usize::MAX, false)
    }

    fn is_unit(&self, node: Node<'_>) -> bool {
        has(&self.grammar.unit_kinds, node.kind())
    }

    /// Row where a unit begins: the topmost of the attribute lines directly above it, with any
    /// comment lines between them and the unit.
    fn start_row(&self, node: Node<'_>) -> usize {
        let mut row = node.start_position().row;
        let mut top = row;
        while row > 0
            && (self.attribute_rows.contains(&(row - 1)) || self.comment_rows.contains(&(row - 1)))
        {
            row -= 1;
            if self.attribute_rows.contains(&row) {
                top = row;
            }
        }
        top
    }

    /// Comments that end on a byte in `(from, to]`.
    fn comments_ending(&self, from: usize, to: usize) -> &[Node<'t>] {
        let first = self
            .comments
            .partition_point(|comment| comment.end_byte() <= from);
        let last = self
            .comments
            .partition_point(|comment| comment.end_byte() <= to);
        &self.comments[first..last.max(first)]
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
        let at = |column| {
            let point = Point::new(row, column);
            self.root.descendant_for_point_range(point, point)
        };
        let mut column = line.len() - line.trim_start().len();
        let node = at(column)?;
        if node == self.root && line.trim().is_empty() {
            return None;
        }
        // The row belongs to code that follows comments on it, not to those comments. A comment
        // that goes on past the row, or with nothing after it, keeps the row.
        let base = self.line_starts[row];
        let first = column;
        while let Some(comment) = holding(&self.comments, base + column) {
            let after = comment.end_byte() - base;
            let rest = line.get(after..).unwrap_or_default();
            let Some(offset) = rest.find(|character: char| !character.is_whitespace()) else {
                return Some(node);
            };
            column = after + offset;
        }
        if column == first {
            return Some(node);
        }
        at(column)
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

    /// The item that an attribute holding `node` decorates: its next sibling past further
    /// attributes and comments.
    fn decorated(&self, node: Node<'t>) -> Option<Node<'t>> {
        let attribute = holding(&self.attributes, node.start_byte())?;
        let mut next = attribute.next_named_sibling();
        while let Some(sibling) = next {
            let kind = sibling.kind();
            if !has(&self.grammar.attribute_kinds, kind) && !has(&self.grammar.comment_kinds, kind)
            {
                return Some(sibling);
            }
            next = sibling.next_named_sibling();
        }
        None
    }

    fn unit_or_top_level(&self, node: Node<'t>) -> Node<'t> {
        let node = self.decorated(node).unwrap_or(node);
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
                // One that opens its row with code after it on its last row (`/* a\n b */ fn f`)
                // belongs to that code.
                if self.opens_row(candidate)
                    && !self.file_level(candidate)
                    && let Some(code) = self.node_at_row(last_row(candidate))
                    && holding(&self.comments, code.start_byte()).is_none()
                {
                    return self.unit_or_top_level(code);
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
        // An inner doc or a shebang before the code on that row ends the block, as in leading().
        let code = node.start_byte();
        let held = holding(&self.comments, code);
        if self
            .comments_ending(self.line_starts[row], code)
            .iter()
            .chain(held.iter())
            .any(|comment| self.file_level(*comment))
        {
            return None;
        }
        Some(self.unit_for(node))
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
            || directive(self.grammar, text, comment.start_position().row) == Some("shebang")
    }

    fn leading(&self, node: Node<'t>) -> Vec<Node<'t>> {
        // Nearest first until the end. Comments before the unit's code on its own row
        // (`/* c */ fn f`) lead it too, and so do those before a comment that ends there, while
        // only comments and attributes precede them. After an inner doc or a shebang, only the
        // comments after it lead; one that starts after code is that code's. Neither case looks
        // further up.
        let mut found = Vec::new();
        let mut row = node.start_position().row;
        let mut begin = node.start_byte();
        loop {
            let line_start = self.line_starts[row];
            if !self.covered(row, begin - line_start, true) {
                break;
            }
            let before = self.comments_ending(line_start, begin);
            if let Some(index) = before.iter().rposition(|comment| self.file_level(*comment)) {
                found.extend(before[index + 1..].iter().rev());
                found.reverse();
                return found;
            }
            found.extend(before.iter().rev());
            let Some(first) = before.first() else {
                break;
            };
            let start = first.start_position();
            if start.row == row {
                break;
            }
            if !self.covered(start.row, start.column, true) {
                found.pop();
                found.reverse();
                return found;
            }
            row = start.row;
            begin = first.start_byte();
        }
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
            (
                "directive",
                directive(self.grammar, text, first.start_position().row).into(),
            ),
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
        let plain = |node: Node<'_>| {
            self.standalone(node)
                && directive(self.grammar, text(node), node.start_position().row).is_none()
        };
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
        // From the unit's first attribute line: a comment after an attribute is inside too.
        let start = self.start_byte(node);
        for comment in self.comments.iter().copied().filter(|comment| {
            comment.start_byte() >= start && comment.end_byte() <= node.end_byte()
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

/// Comments that tools read as instructions; they must not be treated as prose. A shebang is one
/// only on the first line of the file (`row` 0).
fn directive<'g>(grammar: &'g Grammar, text: &str, row: usize) -> Option<&'g str> {
    let text = text.trim_end();
    let Some(language) = grammar.builtin else {
        return grammar
            .directives
            .iter()
            .find(|(prefix, label)| {
                text.starts_with(prefix.as_str()) && (row == 0 || label != "shebang")
            })
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
        Language::Bash if row == 0 && text.starts_with("#!") => Some("shebang"),
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

    /// (kind, name, first line, last line, comment texts) of one unit.
    type Summary = (String, Option<String>, u64, u64, Vec<String>);

    fn picked(language: Language, source: &str, rows: std::ops::Range<usize>) -> Vec<Summary> {
        let tree = parse_partial(language, source).unwrap();
        let grammar = Grammar::builtin(language);
        let report = units(&grammar, source, &tree, &[Change::Rows(rows)]);
        report.as_arr().iter().map(summary).collect()
    }

    fn summary(unit: &Json) -> Summary {
        let lines = unit.get("range").get("lines").as_arr();
        (
            unit.get("kind").as_str().unwrap().to_owned(),
            unit.get("name").as_str().map(str::to_owned),
            lines[0].as_u64().unwrap(),
            lines[1].as_u64().unwrap(),
            unit.get("comments")
                .as_arr()
                .iter()
                .map(|comment| comment.get("text").as_str().unwrap().to_owned())
                .collect(),
        )
    }

    fn unit(kind: &str, name: &str, lines: (u64, u64), comments: &[&str]) -> Summary {
        (
            kind.to_owned(),
            Some(name.to_owned()),
            lines.0,
            lines.1,
            comments.iter().map(|text| (*text).to_owned()).collect(),
        )
    }

    #[test]
    fn code_after_a_block_comment_on_its_row_is_that_rows_unit() {
        let source = "/* c */ fn f() { 2; }\nfn g() {}\n";
        assert_eq!(
            picked(Language::Rust, source, 0..1),
            [unit("function_item", "f", (1, 1), &["/* c */"])]
        );
        // An inner doc describes the module, so it is not the function's comment.
        let source = "/*! i */ fn f() { 2; }\nfn g() {}\n";
        assert_eq!(
            picked(Language::Rust, source, 0..1),
            [unit("function_item", "f", (1, 1), &[])]
        );
        // A comment block above still leads through the comment on the unit's own row. The row
        // above is longer, so a column taken from the file's start instead of the row's is past
        // the row's end.
        let source = "// a comment longer than the row below\n/* c */ fn f() { 2; }\n";
        assert_eq!(
            picked(Language::Rust, source, 1..2),
            [unit(
                "function_item",
                "f",
                (2, 2),
                &["// a comment longer than the row below", "/* c */"]
            )]
        );
        // An inner doc above ends that block; it ends at column 0 of the unit's row.
        let source = "//! m\n/* c */ fn f() { 2; }\n";
        assert_eq!(
            picked(Language::Rust, source, 1..2),
            [unit("function_item", "f", (2, 2), &["/* c */"])]
        );
        // After code on the same row, the comment trails that code instead. `a` is a reference
        // too, so the row is asked about up to two different columns.
        let source = "fn foo() { 2; }\nfn a() { foo(); } /* c */ fn f() { foo(); }\n";
        let found = picked(Language::Rust, source, 0..1);
        let reference = found.iter().find(|unit| unit.1.as_deref() == Some("f"));
        assert_eq!(
            reference,
            Some(&unit("function_item", "f", (2, 2), &[])),
            "{found:?}"
        );
    }

    #[test]
    fn a_changed_row_that_starts_with_a_line_comment_in_a_crlf_file() {
        // A line comment holds the `\r` that line() trims off.
        let source = "// lead\r\nfn f() { 2; }\r\n";
        let found = picked(Language::Rust, source, 0..1);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1.as_deref(), Some("f"));
        let source = "fn f() {\r\n    // c\r\n    2;\r\n}\r\n";
        let found = picked(Language::Rust, source, 1..2);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1.as_deref(), Some("f"));
        let source = "# lead\r\nf() {\r\n  echo b\r\n}\r\n";
        let found = picked(Language::Bash, source, 0..1);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1.as_deref(), Some("f"));
    }

    #[test]
    fn a_block_comment_that_ends_on_a_code_row_belongs_to_that_code() {
        let source = "/* a\n   b */ fn f() { 1; }\nfn g() {}\n";
        assert_eq!(
            picked(Language::Rust, source, 0..1),
            [unit("function_item", "f", (2, 2), &["/* a\n   b */"])]
        );
        let tree = parse_partial(Language::Rust, source).unwrap();
        let grammar = Grammar::builtin(Language::Rust);
        let report = full_units(&grammar, source, &tree);
        let found: Vec<_> = report.as_arr().iter().map(summary).collect();
        assert_eq!(
            found,
            [unit("function_item", "f", (2, 2), &["/* a\n   b */"])]
        );
        // Only its first row changed: a comment row, so `foo` looks for no users.
        let source = "/* a\n   b */ fn foo() { 1; }\nfn u() { foo(); }\n";
        assert_eq!(picked(Language::Rust, source, 0..1).len(), 1);
        assert_eq!(picked(Language::Rust, source, 1..2).len(), 2);
    }

    #[test]
    fn comments_before_a_unit_on_its_row_lead_it_past_attributes_and_inner_docs() {
        let source = "/* c */ #[inline] fn f() { 2; }\n";
        assert_eq!(
            picked(Language::Rust, source, 0..1),
            [unit("function_item", "f", (1, 1), &["/* c */"])]
        );
        // After an inner doc on the row, only the comments after it lead, and none above.
        let source = "// above\n/*! i */ /* c */ fn f() { 2; }\n";
        assert_eq!(
            picked(Language::Rust, source, 1..2),
            [unit("function_item", "f", (2, 2), &["/* c */"])]
        );
    }

    #[test]
    fn a_changed_attribute_row_shows_in_the_unit_it_decorates() {
        // A comment after an attribute is inside the unit, whose range starts at the attribute.
        let source = "#[test]\n#[cfg_attr(miri, ignore)] // slow\nfn t() { 1; }\n";
        assert_eq!(
            picked(Language::Rust, source, 1..2),
            [unit("function_item", "t", (1, 3), &["// slow"])]
        );
        // The range reaches the attribute above a doc comment, which still leads.
        let source = "#[cfg(feature = \"b\")]\n/// Only with b.\npub fn f() {}\n";
        assert_eq!(
            picked(Language::Rust, source, 0..1),
            [unit("function_item", "f", (1, 3), &["/// Only with b.\n"])]
        );
    }

    #[test]
    fn leading_comments_on_the_units_row_after_attributes_above_it() {
        let source = "// r\n#[test]\n/* c */ fn f() { 2; }\n";
        assert_eq!(
            picked(Language::Rust, source, 2..3),
            [unit("function_item", "f", (2, 3), &["// r", "/* c */"])]
        );
        // A comment that starts after code stays that code's: neither it nor what is above it
        // leads the next unit.
        let source = "// about a\nfn a() {} /* x\n   y */ fn b() { 2; }\n";
        let found = picked(Language::Rust, source, 2..3);
        assert_eq!(
            found,
            [unit("function_item", "b", (3, 3), &[])],
            "{found:?}"
        );
        // Comments that open the row before a comment that ends on the unit's row lead too.
        let source = "/* a */ /* b\n c */ fn f() { 2; }\n";
        assert_eq!(
            picked(Language::Rust, source, 1..2),
            [unit(
                "function_item",
                "f",
                (2, 2),
                &["/* a */", "/* b\n c */"]
            )]
        );
    }

    #[test]
    fn a_comment_above_follows_through_to_the_code_it_leads() {
        // Through a block comment that ends on the code row.
        let source = "// lead\n/* a\n   b */ fn f() { 1; }\nfn g() {}\n";
        assert_eq!(
            picked(Language::Rust, source, 0..1),
            [unit(
                "function_item",
                "f",
                (3, 3),
                &["// lead", "/* a\n   b */"]
            )]
        );
        // Not past an inner doc on the code row, where leading() stops too.
        let source = "// x\n/*! i */ fn f() { 1; }\n";
        let found = picked(Language::Rust, source, 0..1);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].4, ["// x"], "{found:?}");
    }

    #[test]
    fn full_mode_gives_a_comment_before_code_on_its_row_to_that_code() {
        let source = "/* c */ fn f() { 2; }\nfn g() {}\n";
        let tree = parse_partial(Language::Rust, source).unwrap();
        let grammar = Grammar::builtin(Language::Rust);
        let report = full_units(&grammar, source, &tree);
        let found: Vec<_> = report.as_arr().iter().map(summary).collect();
        assert_eq!(found, [unit("function_item", "f", (1, 1), &["/* c */"])]);
    }

    #[test]
    fn an_attribute_on_a_line_of_code_does_not_extend_the_next_unit() {
        let source = "// r\n#[inline] fn f() {}\nfn g() { 1; }\n";
        assert_eq!(
            picked(Language::Rust, source, 2..3),
            [unit("function_item", "g", (3, 3), &[])]
        );
        // Attributes on their own line still belong to the item below, with its comment.
        let source = "// r\n#[a] #[b]\nfn g() { 1; }\n";
        assert_eq!(
            picked(Language::Rust, source, 2..3),
            [unit("function_item", "g", (2, 3), &["// r"])]
        );
    }

    #[test]
    fn an_identifier_in_an_attribute_selects_the_item_it_decorates() {
        let source = "pub fn foo() -> u8 { 2 }\n#[foo]\nfn g() {\n    foo();\n}\n";
        let found = picked(Language::Rust, source, 0..1);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[1].0, "function_item");
        assert_eq!(found[1].1.as_deref(), Some("g"));
        assert_eq!((found[1].2, found[1].3), (2, 5));
        // Past further attributes and comments between the attribute and the item.
        let source = "pub fn foo() -> u8 { 2 }\n#[foo]\n// c\n#[inline]\nfn g() {}\n";
        let found = picked(Language::Rust, source, 0..1);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[1].1.as_deref(), Some("g"));
    }

    #[test]
    fn only_the_first_line_holds_a_shebang() {
        let source =
            "#!/bin/bash\nf() {\n  echo a\n}\n# lead\n#!important\n# more\ng() {\n  echo b\n}\n";
        let found = picked(Language::Bash, source, 8..9);
        assert_eq!(
            found,
            [unit(
                "function_definition",
                "g",
                (8, 10),
                &["# lead\n#!important\n# more"]
            )]
        );
        let tree = parse_partial(Language::Bash, source).unwrap();
        let grammar = Grammar::builtin(Language::Bash);
        let report = units(&grammar, source, &tree, &[Change::Rows(8..9)]);
        let comment = &report.as_arr()[0].get("comments").as_arr()[0];
        assert_eq!(comment.get("directive"), &Json::Null);
        // The first line is still one.
        let report = full_units(&grammar, source, &tree);
        let shebangs = report
            .as_arr()
            .iter()
            .flat_map(|unit| unit.get("comments").as_arr())
            .filter(|comment| comment.get("directive").as_str() == Some("shebang"))
            .count();
        assert_eq!(shebangs, 1);
    }

    #[test]
    fn an_external_shebang_label_holds_only_on_the_first_line() {
        let mut grammar = Grammar::builtin(Language::Bash);
        grammar.builtin = None;
        grammar.directives = vec![
            ("#!".to_owned(), "shebang".to_owned()),
            ("# noqa".to_owned(), "noqa".to_owned()),
        ];
        let source = "#!/bin/sh\n# x\n#!mid\n# noqa\nf() {\n  echo\n}\n";
        let tree = parse_partial(Language::Bash, source).unwrap();
        let report = full_units(&grammar, source, &tree);
        let mut found: Vec<(String, Option<String>)> = report
            .as_arr()
            .iter()
            .flat_map(|unit| unit.get("comments").as_arr())
            .map(|comment| {
                (
                    comment.get("text").as_str().unwrap().to_owned(),
                    comment.get("directive").as_str().map(str::to_owned),
                )
            })
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("# noqa".to_owned(), Some("noqa".to_owned())),
                ("# x\n#!mid".to_owned(), None),
                ("#!/bin/sh".to_owned(), Some("shebang".to_owned())),
            ]
        );
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
