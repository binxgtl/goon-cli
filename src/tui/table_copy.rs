//! Semantic copying of table cells in plain, rendered-row byte coordinates.
//!
//! This deliberately follows the small inline dialect of `text::format_inline_markdown_default`,
//! not a full CommonMark parser. Grid borders and padding have no source representation here.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug)]
pub(super) struct Table {
    pub alignments: Vec<String>,
}

#[derive(Debug)]
pub(super) struct Cell {
    /// Content only, excluding the grid's padding and borders, in rendered-row bytes.
    pub range: Range<usize>,
    /// Inline Markdown after the table parser has unescaped its pipe delimiters.
    pub markdown: String,
}

#[derive(Debug)]
pub(super) struct TableRow {
    pub table: Arc<Table>,
    /// Plain rendered grid row, retained for selections containing only borders.
    pub text: Arc<str>,
    /// Source row number; zero is the header. A grid separator has no cells.
    pub row: usize,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    strike: bool,
    code: bool,
    link: Option<usize>,
    link_suffix: bool,
}

#[derive(Clone)]
struct Atom {
    ch: char,
    style: Style,
}

struct Projection {
    atoms: Vec<Atom>,
    links: Vec<String>,
}

impl Projection {
    fn new(markdown: &str) -> Self {
        static PATTERNS: OnceLock<[Regex; 4]> = OnceLock::new();
        let patterns = PATTERNS.get_or_init(|| [r"`([^`\n]+)`", r"\[([^\]]+)\]\(([^)]+)\)", r"\*\*([^*]+)\*\*", r"~~([^~]+)~~"].map(|pattern| Regex::new(pattern).expect("constant inline pattern")));
        let mut result = Self { atoms: markdown.chars().map(|ch| Atom { ch, style: Style::default() }).collect(), links: vec![] };
        // The painter applies these replacements sequentially, including inside code.
        for (pass, pattern) in patterns.iter().enumerate() {
            let text: String = result.atoms.iter().map(|atom| atom.ch).collect();
            let mut offsets: Vec<usize> = text.char_indices().map(|(offset, _)| offset).collect();
            offsets.push(text.len());
            let index = |byte| offsets.binary_search(&byte).expect("regex character boundary");
            let mut output = vec![];
            let mut previous = 0;
            for captures in pattern.captures_iter(&text) {
                let whole = captures.get(0).unwrap();
                let body = captures.get(1).unwrap();
                output.extend_from_slice(&result.atoms[previous..index(whole.start())]);
                let mut content = result.atoms[index(body.start())..index(body.end())].to_vec();
                let link = if pass == 1 {
                    let target = captures.get(2).unwrap();
                    let id = result.links.len();
                    result.links.push(target.as_str().to_owned());
                    Some((id, target))
                } else {
                    None
                };
                for atom in &mut content {
                    match pass {
                        0 => atom.style.code = true,
                        1 => atom.style.link = link.as_ref().map(|(id, _)| *id),
                        2 => atom.style.bold = true,
                        3 => atom.style.strike = true,
                        _ => unreachable!(),
                    }
                }
                output.extend(content);
                if let Some((id, target)) = link {
                    // Links in this renderer show their destination as `label (target)`.
                    let style = Style { link: Some(id), link_suffix: true, ..Style::default() };
                    output.extend(" (".chars().map(|ch| Atom { ch, style: style.clone() }));
                    output.extend(result.atoms[index(target.start())..index(target.end())].iter().cloned().map(|mut atom| {
                        atom.style.link = Some(id);
                        atom.style.link_suffix = true;
                        atom
                    }));
                    output.push(Atom { ch: ')', style });
                }
                previous = index(whole.end());
            }
            output.extend_from_slice(&result.atoms[previous..]);
            result.atoms = output;
        }
        result
    }

    fn selected(&self, ranges: &[Range<usize>]) -> Vec<Atom> {
        let plain: String = self.atoms.iter().map(|atom| atom.ch).collect();
        let mut selected = vec![];
        let mut atom_index = 0;
        // Keep complete graphemes only: a byte endpoint inside an emoji or combining
        // sequence must not copy bytes outside the selected content (or panic).
        for (start, grapheme) in plain.grapheme_indices(true) {
            let end = start + grapheme.len();
            let count = grapheme.chars().count();
            if ranges.iter().any(|range| range.start <= start && end <= range.end) {
                selected.extend_from_slice(&self.atoms[atom_index..atom_index + count]);
            }
            atom_index += count;
        }
        selected
    }
}

struct SelectedCell {
    projection: Projection,
    ranges: Vec<Range<usize>>,
}

/// Serialize only intersected cell content, never the unselected source of a cell.
/// Empty cells have a zero-width content range and are retained when the selection
/// covers that point. Nonempty selections of rows without cells denote separators.
pub(super) fn copy_selected(rows: &[(Arc<TableRow>, Range<usize>)], entire_selection: bool) -> String {
    let Some((first, _)) = rows.first() else { return String::new() };
    let columns = first.table.alignments.len();
    if columns == 0 {
        return String::new();
    }
    let mut cells: BTreeMap<(usize, usize), SelectedCell> = BTreeMap::new();
    let mut separator = false;
    for (row, selected) in rows {
        if selected.start >= selected.end {
            continue; // A half-open selection ending at the next row does not select its empty cells.
        }
        if row.cells.is_empty() {
            separator |= !selected.is_empty();
        }
        for (column, cell) in row.cells.iter().take(columns).enumerate() {
            let start = selected.start.max(cell.range.start);
            let end = selected.end.min(cell.range.end);
            if start > end || start == end && !cell.range.is_empty() {
                continue;
            }
            cells
                .entry((row.row, column))
                .or_insert_with(|| SelectedCell { projection: Projection::new(&cell.markdown), ranges: vec![] })
                .ranges
                .push(start.saturating_sub(cell.range.start)..end.saturating_sub(cell.range.start));
        }
    }
    let standalone = cells.len() == 1 && !separator;
    let mut output: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for ((row, column), mut cell) in cells {
        // Union overlapping selections, including duplicates of a retained row.
        cell.ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = vec![];
        let plain: String = cell.projection.atoms.iter().map(|atom| atom.ch).collect();
        for range in cell.ranges {
            if let Some(previous) = merged.last_mut() {
                if range.start <= previous.end || plain.get(previous.end..range.start).is_some_and(|gap| gap.trim().is_empty()) {
                    previous.end = previous.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        let atoms = cell.projection.selected(&merged);
        let plain = atoms.iter().all(|atom| !atom.style.bold && !atom.style.strike && atom.style.link.is_none());
        let content = if standalone && entire_selection && plain { atoms.iter().map(|atom| atom.ch).collect() } else { serialize(&atoms, &cell.projection.links, !standalone, 0) };
        if standalone {
            return content;
        }
        output.entry(row).or_insert_with(|| vec![String::new(); columns])[column] = content;
    }
    if output.is_empty() {
        return rows
            .iter()
            .map(|(row, range)| {
                let selected: String = row.text.grapheme_indices(true).filter(|(start, grapheme)| range.start <= *start && start + grapheme.len() <= range.end).map(|(_, grapheme)| grapheme).collect();
                escape(&selected)
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    output.entry(0).or_insert_with(|| vec![String::new(); columns]);
    let mut markdown = String::new();
    for (row, values) in output {
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        markdown.push_str(&format!("| {} |", values.join(" | ")));
        if row == 0 {
            markdown.push_str("\n|");
            for alignment in &first.table.alignments {
                markdown.push_str(alignment);
                markdown.push('|');
            }
        }
    }
    markdown
}

// Serialize a fixed wrapper order, sharing surrounding styles across adjacent runs.
fn serialize(atoms: &[Atom], links: &[String], table: bool, depth: usize) -> String {
    if depth == 4 {
        return escape(&atoms.iter().map(|atom| atom.ch).collect::<String>());
    }
    let key = |atom: &Atom| match depth {
        0 => atom.style.link.map_or(0, |id| id + 1),
        1 => usize::from(atom.style.bold),
        2 => usize::from(atom.style.strike),
        3 => usize::from(atom.style.code),
        _ => unreachable!(),
    };
    let mut result = String::new();
    let mut start = 0;
    while start < atoms.len() {
        let value = key(&atoms[start]);
        let mut end = start + 1;
        while end < atoms.len() && key(&atoms[end]) == value {
            end += 1;
        }
        let group = &atoms[start..end];
        let content = if value == 0 {
            serialize(group, links, table, depth + 1)
        } else {
            match depth {
                0 => {
                    let label: Vec<Atom> = group.iter().filter(|atom| !atom.style.link_suffix).cloned().collect();
                    if label.is_empty() {
                        // A destination-only selection is visible prose, not a link label.
                        serialize(group, links, table, depth + 1)
                    } else {
                        let target = escape_destination(&links[value - 1]);
                        format!("[{}](<{target}>)", serialize(group, links, table, depth + 1))
                    }
                }
                1 => format!("**{}**", serialize(group, links, table, depth + 1)),
                2 => format!("~~{}~~", serialize(group, links, table, depth + 1)),
                3 => {
                    let text: String = group.iter().map(|atom| atom.ch).collect();
                    let fence = "`".repeat(text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0) + 1);
                    let padding = text.starts_with('`') || text.ends_with('`') || text.starts_with(' ') && text.ends_with(' ');
                    let text = if table { text.replace('|', "\\|") } else { text };
                    let pad = if padding { " " } else { "" };
                    format!("{fence}{pad}{text}{pad}{fence}")
                }
                _ => unreachable!(),
            }
        };
        result.push_str(&content);
        start = end;
    }
    result
}

fn escape_destination(target: &str) -> String {
    target.replace('\\', "\\\\").replace('&', "&amp;").replace('<', "%3C").replace('>', "%3E").replace('|', "%7C").replace('\n', "%0A").replace('\r', "%0D")
}

fn escape(text: &str) -> String {
    let mut output = String::new();
    let mut whitespace_prefix = true;
    let mut numeric_prefix = false;
    for ch in text.chars() {
        let block_marker = "=-+".contains(ch) && whitespace_prefix || matches!(ch, '.' | ')') && numeric_prefix;
        if "\\`*_[]<>~#|&!".contains(ch) || block_marker {
            output.push('\\');
        }
        output.push(ch);
        numeric_prefix = ch.is_ascii_digit() && (whitespace_prefix || numeric_prefix);
        whitespace_prefix &= ch.is_whitespace();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy_selected(rows: &[(Arc<TableRow>, Range<usize>)]) -> String {
        super::copy_selected(rows, true)
    }

    fn table() -> Arc<Table> {
        Arc::new(Table { alignments: vec!["---".into(), ":---:".into(), "---:".into()] })
    }

    fn row(table: &Arc<Table>, number: usize, contents: &[&str]) -> Arc<TableRow> {
        let mut offset = 2;
        let cells = contents
            .iter()
            .map(|markdown| {
                let length: usize = Projection::new(markdown).atoms.iter().map(|atom| atom.ch.len_utf8()).sum();
                let range = offset..offset + length;
                offset += length + 3;
                Cell { range, markdown: (*markdown).into() }
            })
            .collect();
        Arc::new(TableRow { table: Arc::clone(table), row: number, cells, text: Arc::from("") })
    }

    #[test]
    fn full_grid_has_semantic_cells_and_alignment_not_grid_padding() {
        let table = table();
        let header = row(&table, 0, &["Name", "Value", "Note"]);
        let body = row(&table, 1, &["**alpha**", "`a|b`", "~~old~~"]);
        let separator = Arc::new(TableRow { table, row: 0, cells: vec![], text: Arc::from("") });
        assert_eq!(copy_selected(&[(header, 0..100), (separator, 0..100), (body, 0..100)]), "| Name | Value | Note |\n|---|:---:|---:|\n| **alpha** | `a\\|b` | ~~old~~ |");
    }

    #[test]
    fn partial_bold_does_not_copy_the_original_cell() {
        let body = row(&table(), 1, &["**alpha**", "secret", "secret"]);
        assert_eq!(copy_selected(&[(body, 3..6)]), "**lph**");
    }

    #[test]
    fn standalone_plain_and_code_keep_literal_pipes_and_backslashes() {
        for source in ["a|b", "`a|b`", r"a\|b", r"`a\|b`"] {
            let body = row(&table(), 1, &[source, "secret", "secret"]);
            let range = body.cells[0].range.clone();
            assert_eq!(copy_selected(&[(body, range)]), source.trim_matches('`'));
        }
        let body = row(&table(), 1, &["`a|b`", "secret", "secret"]);
        assert_eq!(copy_selected(&[(body, 3..5)]), "|b");
    }

    #[test]
    fn partial_multicell_selection_blanks_unselected_header_and_secrets() {
        let body = row(&table(), 8, &["secret", "**alpha**", "secret"]);
        let start = body.cells[1].range.start + 1;
        let next = row(&body.table, 9, &["beta", "secret", "secret"]);
        let end = next.cells[0].range.start + 2;
        assert_eq!(copy_selected(&[(body, start..start + 3), (next, 2..end)]), "|  |  |  |\n|---|:---:|---:|\n|  | **lph** |  |\n| be |  |  |");
    }

    #[test]
    fn selected_empty_cells_keep_their_columns_and_rows() {
        let body = row(&table(), 1, &["", "", "secret"]);
        assert_eq!(copy_selected(&[(body, 0..6)]), "|  |  |  |\n|---|:---:|---:|\n|  |  |  |");
        let body = row(&table(), 1, &["secret", "", "secret"]);
        let point = body.cells[1].range.start;
        assert_eq!(copy_selected(&[(body, point..point)]), "");
    }

    #[test]
    fn a_selected_separator_disables_the_standalone_shortcut() {
        let table = table();
        let body = row(&table, 1, &["secret", "value", "secret"]);
        let range = body.cells[1].range.clone();
        let separator = Arc::new(TableRow { table, row: 0, cells: vec![], text: Arc::from("") });
        assert_eq!(copy_selected(&[(separator, 1..2), (body, range)]), "|  |  |  |\n|---|:---:|---:|\n|  | value |  |");
    }

    #[test]
    fn unicode_uses_bytes_and_never_splits_graphemes() {
        let body = row(&table(), 1, &["**a界e\u{301}👩‍💻z**", "secret", "secret"]);
        let start = body.cells[0].range.start + 1;
        let end = start + "界e\u{301}👩‍💻".len();
        assert_eq!(copy_selected(&[(Arc::clone(&body), start..end)]), "**界e\u{301}👩‍💻**");
        assert_eq!(copy_selected(&[(body, start + 1..start + 3)]), "");
    }

    #[test]
    fn table_plain_punctuation_is_escaped_without_losing_backslashes() {
        let body = row(&table(), 1, &[r"a\|b * _ [x]", "ok", "secret"]);
        let end = body.cells[1].range.end;
        assert_eq!(copy_selected(&[(body, 0..end)]), "|  |  |  |\n|---|:---:|---:|\n| a\\\\\\|b \\* \\_ \\[x\\] | ok |  |");
    }

    #[test]
    fn partial_strike_and_link_labels_preserve_supported_formatting() {
        let body = row(&table(), 1, &["~~alpha~~", "secret", "secret"]);
        assert_eq!(copy_selected(&[(body, 3..6)]), "~~lph~~");
        let body = row(&table(), 1, &["[alpha](https://example.test)", "secret", "secret"]);
        assert_eq!(copy_selected(&[(Arc::clone(&body), 3..6)]), "[lph](<https://example.test>)");
        let range = body.cells[0].range.clone();
        assert_eq!(copy_selected(&[(body, range)]), "[alpha (https://example.test)](<https://example.test>)");
    }

    #[test]
    fn repeated_ranges_are_unioned_without_copying_the_gap() {
        let body = row(&table(), 1, &["**alphaSECRETomega**", "secret", "secret"]);
        assert_eq!(copy_selected(&[(Arc::clone(&body), 2..7), (Arc::clone(&body), 3..7), (body, 13..18)]), "**alphaomega**");
    }

    #[test]
    fn plain_escaping_keeps_ordinary_punctuation_and_protects_block_prefixes() {
        assert_eq!(escape("my_long_file_name!.txt"), r"my\_long\_file\_name\!.txt");
        assert_eq!(escape("word - text v1.2 (ok)"), "word - text v1.2 (ok)");
        assert_eq!(escape("- item"), r"\- item");
        assert_eq!(escape("12. text"), r"12\. text");
        assert_eq!(escape("12 3."), "12 3.");
        assert_eq!(escape_destination("https://example.test/?a=1&b=2|x>"), "https://example.test/?a=1&amp;b=2%7Cx%3E");
    }

    #[test]
    fn projection_tracks_the_painters_sequential_inline_dialect() {
        let projection = Projection::new("**a `b`** ~~c~~ [d](url) `**e**` *literal*");
        assert_eq!(projection.atoms.iter().map(|atom| atom.ch).collect::<String>(), "a b c d (url) e *literal*");
    }

    #[test]
    fn code_only_cell_with_surrounding_selection_keeps_markdown() {
        let body = row(&table(), 1, &["`a|b`", "secret", "secret"]);
        let range = body.cells[0].range.clone();
        assert_eq!(super::copy_selected(&[(body, range)], false), "`a|b`");
    }

    #[test]
    fn border_only_selection_uses_only_selected_rendered_text() {
        let separator = Arc::new(TableRow { table: table(), row: 0, cells: vec![], text: Arc::from("├──secret──┤") });
        assert_eq!(copy_selected(&[(separator, 0..9)]), "├──");
    }
}
