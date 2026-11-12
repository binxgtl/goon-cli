//! Width-adaptive table presentation backed by width-independent logical grid coordinates.
use super::selection::{Fragment, Origin};
use super::table_copy::{Cell, Table, TableRow};
use super::table_layout::{choose, Mode};
use super::text::{format_inline_markdown_with, styled_range_with, visible_length, wrap_ranges, Palette};
use super::{Row, RowKind};
use std::sync::Arc;

struct Logical {
    row: Arc<TableRow>,
    painted: Vec<String>,
}

fn logical(table: &Arc<Table>, number: usize, source: &[String], widths: &[usize], p: &Palette) -> Logical {
    let mut text = String::from("│ ");
    let mut cells = Vec::new();
    let mut painted = Vec::new();
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            text.push_str(" │ ");
        }
        let markdown = source.get(column).cloned().unwrap_or_default();
        let style = format_inline_markdown_with(&markdown, p.reset, p);
        let plain = crate::term::strip_vt(&style);
        let start = text.len();
        text.push_str(&plain);
        cells.push(Cell { range: start..text.len(), markdown });
        text.push_str(&" ".repeat(width.saturating_sub(visible_length(&plain))));
        painted.push(style);
    }
    text.push_str(" │");
    Logical { row: Arc::new(TableRow { table: table.clone(), row: number, text: Arc::from(text), cells }), painted }
}

fn push(rows: &mut Vec<Row>, text: String, pre: &str, logical: &Arc<TableRow>, fragments: Vec<Fragment>) {
    rows.push(Row {
        kind: RowKind::Table,
        pre: pre.into(),
        text,
        glyph: None,
        sign: None,
        level: 0,
        origin: Some(Origin { text: logical.text.clone(), range: 0..logical.text.len(), projected: true, table: Some(logical.clone()), fragments: Some(fragments) }),
    });
}

#[allow(clippy::too_many_arguments)]
fn field(rows: &mut Vec<Row>, logical: &Logical, column: usize, width: usize, first: &str, cont: &str, pre: &str, occurrence: Option<usize>, p: &Palette) {
    let cell = &logical.row.cells[column];
    let plain = &logical.row.text[cell.range.clone()];
    for (index, range) in wrap_ranges(plain, width, width, true).into_iter().enumerate() {
        let prefix = if index == 0 { first } else { cont };
        let start = visible_length(prefix);
        let end = start + visible_length(&plain[range.clone()]);
        let fragment = Fragment { columns: start..end, range: cell.range.start + range.start..cell.range.start + range.end, table: None, occurrence };
        push(rows, format!("{prefix}{}", styled_range_with(&logical.painted[column], range, p)), pre, &logical.row, vec![fragment]);
    }
}

/// Render a complete source table only once; canonical row text is identical at every width.
pub(super) fn render(sources: &[Vec<String>], table: &Arc<Table>, natural: &[usize], columns: usize, pre: &str, lead: &str, p: &Palette) -> Vec<Row> {
    if sources.is_empty() {
        return vec![];
    }
    let logical: Vec<Logical> = sources.iter().enumerate().map(|(i, source)| logical(table, i, source, natural, p)).collect();
    let plain: Vec<Vec<String>> = logical.iter().map(|row| row.row.cells.iter().map(|cell| row.row.text[cell.range.clone()].to_string()).collect()).collect();
    let available = columns.saturating_sub(visible_length(pre).max(visible_length(lead))).max(1);
    let mode = choose(&plain[0], &plain[1..], available);
    let mut rows = Vec::new();
    match mode {
        Mode::Grid(widths) => {
            for (number, row) in logical.iter().enumerate() {
                if number == 1 || logical.len() == 1 && number == 0 {
                    let text = format!("├{}┤", widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┼"));
                    // Separator bytes are stable across widths too.
                    let canonical = format!("├{}┤", natural.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┼"));
                    let separator = Arc::new(TableRow { table: table.clone(), row: 0, text: Arc::from(canonical), cells: vec![] });
                    push(&mut rows, format!("{}{text}{}", p.dim, p.reset), pre, &separator, vec![]);
                }
                let wrapped: Vec<_> = row.row.cells.iter().zip(&widths).map(|(cell, width)| wrap_ranges(&row.row.text[cell.range.clone()], *width, *width, true)).collect();
                let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
                for y in 0..height {
                    let mut text = format!("{}│{} ", p.dim, p.reset);
                    let mut col = 2;
                    let mut fragments = Vec::new();
                    for (column, width) in widths.iter().enumerate() {
                        if column > 0 {
                            text.push_str(&format!(" {}│{} ", p.dim, p.reset));
                            col += 3;
                        }
                        let cell = &row.row.cells[column];
                        let used = if let Some(range) = wrapped[column].get(y) {
                            let start = cell.range.start + range.start;
                            let end = cell.range.start + range.end;
                            let used = visible_length(&row.row.text[start..end]);
                            fragments.push(Fragment { columns: col..col + used, range: start..end, table: None, occurrence: None });
                            text.push_str(&styled_range_with(&row.painted[column], range.clone(), p));
                            used
                        } else {
                            0
                        };
                        text.push_str(&" ".repeat(width.saturating_sub(used)));
                        col += width;
                    }
                    text.push_str(&format!(" {}│{}", p.dim, p.reset));
                    push(&mut rows, text, pre, &row.row, fragments);
                }
            }
        }
        Mode::Records { aligned, label_width } => {
            let canonical = format!("├{}┤", natural.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┼"));
            let separator = Arc::new(TableRow { table: table.clone(), row: 0, text: Arc::from(canonical), cells: vec![] });
            push(&mut rows, format!("{}{}{}", p.dim, "─".repeat(available), p.reset), pre, &separator, vec![]);
            let first_body = if logical.len() > 1 { 1 } else { 0 };
            for (number, row) in logical.iter().enumerate().skip(first_body) {
                if number > first_body {
                    rows.push(Row { kind: RowKind::Rule, pre: pre.into(), text: format!("{}{}{}", p.dim, "─".repeat(available), p.reset), glyph: None, sign: None, level: 0, origin: None });
                }
                for column in 0..natural.len() {
                    if aligned {
                        let header = &logical[0];
                        let label = &header.row.cells[column];
                        let label_text = &header.row.text[label.range.clone()];
                        let indent = 1 + label_width + 2;
                        let cell = &row.row.cells[column];
                        let value = &row.row.text[cell.range.clone()];
                        let width = available.saturating_sub(indent).max(1);
                        for (index, range) in wrap_ranges(value, width, width, true).into_iter().enumerate() {
                            let mut fragments = vec![];
                            let prefix = if index == 0 {
                                fragments.push(Fragment { columns: 1..1 + visible_length(label_text), range: label.range.clone(), table: Some(header.row.clone()), occurrence: Some(number) });
                                format!(" {}{}{}{}", p.bold, header.painted[column], p.reset, " ".repeat(indent - 1 - visible_length(label_text)))
                            } else {
                                " ".repeat(indent)
                            };
                            let used = visible_length(&value[range.clone()]);
                            fragments.push(Fragment { columns: indent..indent + used, range: cell.range.start + range.start..cell.range.start + range.end, table: None, occurrence: None });
                            push(&mut rows, format!("{prefix}{}", styled_range_with(&row.painted[column], range, p)), pre, &row.row, fragments);
                        }
                    } else {
                        let label_indent = if available > 1 { " " } else { "" };
                        field(&mut rows, &logical[0], column, available.saturating_sub(label_indent.len()).max(1), label_indent, label_indent, pre, Some(number), p);
                        let indent = " ".repeat(available.min(2).saturating_sub(usize::from(available <= 2)));
                        field(&mut rows, row, column, available.saturating_sub(indent.len()).max(1), &indent, &indent, pre, None, p);
                    }
                }
            }
        }
    }
    if let Some(first) = rows.iter_mut().find(|row| !row.text.is_empty()) {
        first.pre = lead.into();
    }
    rows
}
