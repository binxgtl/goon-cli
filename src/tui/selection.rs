//! Logical transcript selection with immutable revision ownership and wrapping provenance.
use super::*;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug)]
pub(super) struct Origin {
    pub text: Arc<str>,
    pub range: Range<usize>,
    pub projected: bool,
    pub table: Option<Arc<super::table_copy::TableRow>>,
    pub fragments: Option<Vec<Fragment>>,
}

/// A visible table fragment mapped back to width-independent logical-row bytes.
#[derive(Clone, Debug)]
pub(super) struct Fragment {
    pub columns: Range<usize>,
    pub range: Range<usize>,
    /// Aligned record labels belong to the header, not the value's logical row.
    pub table: Option<Arc<super::table_copy::TableRow>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Point {
    pub line: usize,
    pub offset: usize,
}

#[derive(Clone, Default)]
pub(super) struct Layout {
    pub text: Vec<Arc<str>>,
    tables: Vec<Option<Arc<super::table_copy::TableRow>>>,
    rows: Vec<LayoutRow>,
}

#[derive(Clone)]
struct LayoutRow {
    line: usize,
    range: Range<usize>,
    prefix: usize,
    visible: String,
    fragments: Option<Vec<Fragment>>,
}

impl Layout {
    pub(super) fn from_rows(rows: &[Row], styled: &[String], rich: bool) -> Self {
        let mut layout = Self::default();
        let mut previous: Option<&Arc<str>> = None;
        let mut table_lines = std::collections::HashMap::new();
        for (row, painted) in rows.iter().zip(styled) {
            if let Some(table) = row.origin.as_ref().and_then(|origin| origin.table.as_ref()) {
                let key = Arc::as_ptr(&table.text) as *const () as usize;
                if !table_lines.contains_key(&key) {
                    let mut logical: Vec<_> = rows
                        .iter()
                        .filter_map(|row| row.origin.as_ref())
                        .flat_map(|origin| origin.table.iter().chain(origin.fragments.iter().flatten().filter_map(|fragment| fragment.table.as_ref())))
                        .filter(|other| Arc::ptr_eq(&other.table, &table.table))
                        .collect();
                    logical.sort_by_key(|row| (row.row, usize::from(row.cells.is_empty())));
                    for row in logical {
                        let key = Arc::as_ptr(&row.text) as *const () as usize;
                        if table_lines.contains_key(&key) {
                            continue;
                        }
                        table_lines.insert(key, layout.text.len());
                        layout.text.push(row.text.clone());
                        layout.tables.push(Some(row.clone()));
                    }
                }
            }
            if (row.origin.is_none() && row.kind == RowKind::Blank) || matches!(row.kind, RowKind::Rule | RowKind::CodeHead | RowKind::ReasoningHead | RowKind::BannerTitle | RowKind::Banner) {
                layout.rows.push(LayoutRow { line: usize::MAX, range: 0..0, prefix: 0, visible: String::new(), fragments: None });
                previous = None;
                continue;
            }
            let (text, range) = if let Some(origin) = &row.origin {
                (origin.text.clone(), origin.range.clone())
            } else {
                let painted = if row.kind == RowKind::Table { crate::term::strip_vt(&super::text::format_inline_markdown_default(&row.text)).into_owned() } else { row.text.clone() };
                let text: Arc<str> = Arc::from(painted);
                let end = text.len();
                (text, 0..end)
            };
            let continuation = row.origin.as_ref().is_some_and(|origin| previous.is_some_and(|p| Arc::ptr_eq(p, &origin.text)));
            let table = row.origin.as_ref().and_then(|origin| origin.table.clone());
            let table_key = table.as_ref().map(|row| Arc::as_ptr(&row.text) as *const () as usize);
            let line = if let Some(line) = table_key.and_then(|key| table_lines.get(&key).copied()) {
                line
            } else if continuation {
                layout.text.len() - 1
            } else {
                let line = layout.text.len();
                layout.text.push(text.clone());
                layout.tables.push(table);
                if let Some(key) = table_key {
                    table_lines.insert(key, line);
                }
                line
            };
            let prefix = if rich && row.kind == RowKind::Code { 4 } else { visible_length(&row.pre) };
            let plain = crate::term::strip_vt(painted);
            let visible = plain
                .graphemes(true)
                .scan(0, |col, g| {
                    let left = *col;
                    *col += visible_length(g);
                    Some((left, g))
                })
                .filter_map(|(col, g)| (col >= prefix).then_some(g))
                .collect();
            layout.rows.push(LayoutRow { line, range, prefix, visible, fragments: row.origin.as_ref().and_then(|origin| origin.fragments.clone()) });
            previous = row.origin.as_ref().map(|o| &o.text);
        }
        layout
    }

    fn fragment_line(&self, row: &LayoutRow, fragment: &Fragment) -> usize {
        fragment.table.as_ref().and_then(|table| self.tables.iter().position(|other| other.as_ref().is_some_and(|other| Arc::ptr_eq(&other.text, &table.text)))).unwrap_or(row.line)
    }

    pub(super) fn hit(&self, row: usize, column: usize) -> Option<Point> {
        let row = self.rows.get(row)?;
        if row.line == usize::MAX {
            return None;
        }
        if let Some(fragments) = &row.fragments {
            let column = column.saturating_sub(row.prefix);
            let fragment = fragments.iter().find(|f| column <= f.columns.end).or_else(|| fragments.last())?;
            let line = self.fragment_line(row, fragment);
            let mut col = fragment.columns.start;
            let mut offset = fragment.range.start;
            for grapheme in self.text[line][fragment.range.clone()].graphemes(true) {
                let right = col + visible_length(grapheme);
                if right > column {
                    break;
                }
                col = right;
                offset += grapheme.len();
            }
            return Some(Point { line, offset });
        }
        let text = &row.visible;
        let mut col = row.prefix;
        let mut offset = row.range.start;
        for grapheme in text.graphemes(true) {
            let right = col + visible_length(grapheme);
            if right > column {
                break;
            }
            col = right;
            offset += grapheme.len();
        }
        if column >= row.prefix + visible_length(text) && offset < row.range.end {
            // A clipped row's right edge is the logical end, not an anchor that
            // silently loses its offscreen tail on a same-row copy.
            offset = row.range.end;
        }
        Some(Point { line: row.line, offset: offset.min(row.range.end) })
    }

    fn columns(&self, row: usize, selection: &Selection) -> Vec<(usize, usize)> {
        let Some(layout_row) = self.rows.get(row) else { return vec![] };
        if layout_row.line == usize::MAX {
            return vec![];
        }
        if let Some(fragments) = &layout_row.fragments {
            return fragments
                .iter()
                .flat_map(|fragment| {
                    let line = self.fragment_line(layout_row, fragment);
                    let text = &self.text[line];
                    selection.ranges_on(line).into_iter().filter_map(move |range| {
                        let from = range.start.max(fragment.range.start);
                        let to = range.end.min(fragment.range.end);
                        (from < to).then(|| {
                            (
                                layout_row.prefix + fragment.columns.start + visible_length(&text[fragment.range.start..from]),
                                layout_row.prefix + fragment.columns.start + visible_length(&text[fragment.range.start..to]),
                            )
                        })
                    })
                })
                .collect();
        }
        if selection.selected_fragments.is_some() {
            let text = &self.text[layout_row.line];
            return selection
                .ranges_on(layout_row.line)
                .into_iter()
                .filter_map(|range| {
                    let from = range.start.max(layout_row.range.start);
                    let to = range.end.min(layout_row.range.end);
                    (from < to).then(|| (layout_row.prefix + visible_length(&text[layout_row.range.start..from]), layout_row.prefix + visible_length(&text[layout_row.range.start..to])))
                })
                .collect();
        }
        self.plain_columns(row, selection).into_iter().collect()
    }

    fn plain_columns(&self, row: usize, selection: &Selection) -> Option<(usize, usize)> {
        let row = self.rows.get(row)?;
        let (start, end) = selection.range();
        if row.line < start.line || row.line > end.line || start == end {
            return None;
        }
        let text = &self.text[row.line];
        let hard_break = row.line < end.line && row.range.end == text.len() && !row.range.is_empty();
        let start = if row.line == start.line { start.offset.max(row.range.start) } else { row.range.start };
        let end = if row.line == end.line { end.offset.min(row.range.end) } else { row.range.end };
        if start > end || (start == end && !hard_break) {
            return None;
        }
        Some((row.prefix + visible_length(&text[row.range.start..start.min(row.range.end)]), row.prefix + visible_length(&text[row.range.start..end]) + usize::from(hard_break)))
    }

    fn column_for(&self, row: usize, point: Point) -> Option<usize> {
        let row = self.rows.get(row)?;
        if let Some(fragments) = &row.fragments {
            let fragment = fragments.iter().find(|fragment| self.fragment_line(row, fragment) == point.line && fragment.range.start <= point.offset && point.offset <= fragment.range.end)?;
            return Some(row.prefix + fragment.columns.start + visible_length(&self.text[point.line][fragment.range.start..point.offset]));
        }
        Some(row.prefix + visible_length(&self.text[point.line][row.range.start..point.offset.min(row.range.end)]))
    }

    pub(super) fn row_for(&self, point: Point) -> Option<usize> {
        self.rows.iter().position(|row| {
            row.fragments.as_ref().map_or_else(
                || row.line == point.line && row.range.start <= point.offset && point.offset <= row.range.end,
                |fragments| fragments.iter().any(|f| self.fragment_line(row, f) == point.line && f.range.start <= point.offset && point.offset <= f.range.end),
            )
        })
    }

    pub(super) fn row_count(&self) -> usize {
        self.rows.len()
    }
}

pub(super) struct Selection {
    pub anchor: Point,
    pub end: Point,
    pub source: Arc<str>,
    text: Vec<Arc<str>>,
    tables: Vec<Option<Arc<super::table_copy::TableRow>>>,
    identity: Arc<()>,
    unit: Unit,
    origin: (Point, Point),
    keyboard_row: Option<usize>,
    preferred_column: Option<usize>,
    pointer_anchor: Option<(usize, usize)>,
    pointer_unit_origin: Option<((usize, usize), (usize, usize))>,
    selected_fragments: Option<Vec<(usize, Range<usize>)>>,
}

#[derive(Clone, Copy)]
enum Unit {
    Character,
    Word,
    Line,
}

pub(super) struct CopyRequest {
    pub text: String,
    identity: Arc<()>,
    anchor: Point,
    end: Point,
    follow: bool,
}

impl Selection {
    pub(super) fn new(point: Point, source: &str, layout: &Layout) -> Self {
        Self {
            anchor: point,
            end: point,
            source: Arc::from(source),
            text: layout.text.clone(),
            tables: layout.tables.clone(),
            identity: Arc::new(()),
            unit: Unit::Character,
            origin: (point, point),
            keyboard_row: None,
            preferred_column: None,
            pointer_anchor: None,
            pointer_unit_origin: None,
            selected_fragments: None,
        }
    }

    pub(super) fn begin(point: Point, source: &str, layout: &Layout, clicks: u8) -> Self {
        let mut selection = Self::new(point, source, layout);
        selection.unit = match clicks {
            2 => Unit::Word,
            3 => Unit::Line,
            _ => Unit::Character,
        };
        let (start, end) = selection.unit_range(point);
        selection.anchor = start;
        selection.end = end;
        selection.origin = (start, end);
        selection
    }

    fn unit_range(&self, point: Point) -> (Point, Point) {
        let text = &self.text[point.line];
        match self.unit {
            Unit::Character => (point, point),
            Unit::Word => {
                let range = text.split_word_bound_indices().map(|(start, word)| start..start + word.len()).find(|r| r.contains(&point.offset)).unwrap_or(text.len()..text.len());
                (Point { offset: range.start, ..point }, Point { offset: range.end, ..point })
            }
            Unit::Line => (Point { offset: 0, ..point }, if point.line + 1 < self.text.len() { Point { line: point.line + 1, offset: 0 } } else { Point { offset: text.len(), ..point } }),
        }
    }

    pub(super) fn begin_pointer(&mut self, row: usize, column: usize) {
        self.pointer_anchor = Some((row, column));
    }

    fn visual_unit(&self, layout: &Layout, row: usize, column: usize) -> ((usize, usize), (usize, usize)) {
        let Some(visual) = layout.rows.get(row) else { return ((row, column), (row, column)) };
        if matches!(self.unit, Unit::Line) {
            return ((row, 0), (row, usize::MAX));
        }
        let Some(point) = layout.hit(row, column) else { return ((row, column), (row, column)) };
        let (start, end) = self.unit_range(point);
        let from = layout.column_for(row, start).unwrap_or(visual.prefix);
        let to = layout.column_for(row, end).unwrap_or(visual.prefix + visible_length(&visual.visible));
        ((row, from), (row, to))
    }

    pub(super) fn extend_pointer(&mut self, point: Point, layout: &Layout, row: usize, column: usize) {
        // A resize invalidates physical coordinates, not the selected source.
        // Reconstruct the anchor only after matching the current layout.
        let anchor = self.pointer_anchor.or_else(|| {
            let row = layout.row_for(self.origin.0)?;
            Some((row, layout.column_for(row, self.origin.0)?))
        });
        let previous = self.selected_fragments.clone();
        self.extend(point);
        let Some(anchor) = anchor else { return };
        self.pointer_anchor = Some(anchor);
        let end = (row, column);
        let (mut start, mut end) = if anchor <= end { (anchor, end) } else { (end, anchor) };
        if !matches!(self.unit, Unit::Character) {
            let origin = self.pointer_unit_origin.unwrap_or_else(|| self.visual_unit(layout, anchor.0, anchor.1));
            self.pointer_unit_origin = Some(origin);
            let target = self.visual_unit(layout, row, column);
            start = origin.0.min(target.0);
            end = origin.1.max(target.1);
        }
        // A canonical interval is sufficient for prose; tables can display header
        // cells repeatedly and out of source order, so retain only visible fragments.
        if !layout.rows.get(start.0..=end.0).is_some_and(|rows| rows.iter().any(|row| row.fragments.is_some())) {
            return;
        }
        let mut selected = Vec::new();
        for index in start.0..=end.0 {
            let Some(visual) = layout.rows.get(index) else { continue };
            let left = if index == start.0 { start.1 } else { 0 };
            let right = if index == end.0 { end.1 } else { usize::MAX };
            if let Some(fragments) = &visual.fragments {
                for fragment in fragments {
                    let line = layout.fragment_line(visual, fragment);
                    let mut col = visual.prefix + fragment.columns.start;
                    let mut from = fragment.range.start;
                    let mut to = from;
                    for (offset, grapheme) in layout.text[line][fragment.range.clone()].grapheme_indices(true) {
                        let next = col + visible_length(grapheme);
                        if next <= left {
                            from = fragment.range.start + offset + grapheme.len();
                        }
                        if col < right {
                            to = fragment.range.start + offset + grapheme.len();
                        }
                        col = next;
                    }
                    if from < to || fragment.range.is_empty() && left <= col && col < right {
                        selected.push((line, from..to));
                    }
                }
            } else if visual.line != usize::MAX {
                if let (Some(from), Some(to)) = (layout.hit(index, left), layout.hit(index, right)) {
                    selected.push((visual.line, from.offset..to.offset));
                }
            }
        }
        selected.sort_by_key(|(line, range)| (*line, range.start, range.end));
        if previous.as_ref() != Some(&selected) {
            self.invalidate_copy();
        }
        self.selected_fragments = Some(selected);
    }

    pub(super) fn extend(&mut self, point: Point) {
        if self.selected_fragments.take().is_some() {
            self.invalidate_copy();
        }
        self.keyboard_row = None;
        self.preferred_column = None;
        let previous = (self.anchor, self.end);
        let (start, end) = self.unit_range(point);
        if point < self.origin.0 {
            self.anchor = self.origin.1;
            self.end = start;
        } else {
            self.anchor = self.origin.0;
            self.end = end;
        }
        if previous != (self.anchor, self.end) {
            // Even extending away and back invalidates an in-flight clipboard result.
            self.identity = Arc::new(());
        }
    }

    pub(super) fn move_endpoint(&mut self, layout: &Layout, direction: &str) -> Option<usize> {
        let had_fragments = self.selected_fragments.is_some();
        let point = self.end;
        let previous_row = self.keyboard_row.or_else(|| layout.row_for(point));
        let text = &self.text[point.line];
        let (next, visual_row) = match direction {
            "left" => {
                let next = if point.offset > 0 {
                    Point { offset: text[..point.offset].grapheme_indices(true).next_back().map_or(0, |(i, _)| i), ..point }
                } else if point.line > 0 {
                    Point { line: point.line - 1, offset: self.text[point.line - 1].len() }
                } else {
                    point
                };
                self.preferred_column = None;
                (next, layout.row_for(next)?)
            }
            "right" => {
                let next = if point.offset < text.len() {
                    Point { offset: point.offset + text[point.offset..].graphemes(true).next().map_or(0, str::len), ..point }
                } else if point.line + 1 < self.text.len() {
                    Point { line: point.line + 1, offset: 0 }
                } else {
                    point
                };
                self.preferred_column = None;
                (next, layout.row_for(next)?)
            }
            "up" | "down" => {
                let current = self
                    .keyboard_row
                    .filter(|i| layout.rows.get(*i).is_some_and(|r| r.line == point.line && r.range.start <= point.offset && point.offset <= r.range.end))
                    .or_else(|| layout.row_for(point))?;
                let row = &layout.rows[current];
                let column = *self.preferred_column.get_or_insert_with(|| {
                    if let Some(fragments) = &row.fragments {
                        if let Some(fragment) =
                            fragments.iter().find(|fragment| layout.fragment_line(row, fragment) == point.line && fragment.range.start <= point.offset && point.offset <= fragment.range.end)
                        {
                            return row.prefix + fragment.columns.start + visible_length(&text[fragment.range.start..point.offset]);
                        }
                    }
                    row.prefix + visible_length(&text[row.range.start..point.offset.min(row.range.end)])
                });
                let target =
                    if direction == "up" { (0..current).rev().find(|i| layout.rows[*i].line != usize::MAX) } else { (current + 1..layout.rows.len()).find(|i| layout.rows[*i].line != usize::MAX) }
                        .unwrap_or(current);
                (layout.hit(target, column)?, target)
            }
            _ => return None,
        };
        if self.end != next || previous_row != Some(visual_row) {
            self.invalidate_copy();
        }
        if had_fragments {
            if let Some(anchor_row) = layout.row_for(self.anchor) {
                let anchor = layout.column_for(anchor_row, self.anchor).unwrap_or(0);
                let column = layout.column_for(visual_row, next).unwrap_or(0);
                self.pointer_anchor = Some((anchor_row, anchor));
                self.extend_pointer(next, layout, visual_row, column);
            }
        }
        self.end = next;
        self.keyboard_row = Some(visual_row);
        Some(visual_row)
    }

    pub(super) fn invalidate_copy(&mut self) {
        // Reading-position changes must invalidate even an away-and-back trip.
        // Keep the immutable source and logical anchors available for a retry.
        self.identity = Arc::new(());
    }

    pub(super) fn reset_navigation(&mut self) {
        self.invalidate_copy();
        self.pointer_anchor = None; // Old physical coordinates cannot anchor a new gesture after reflow.
        self.pointer_unit_origin = None;
        self.keyboard_row = None;
        self.preferred_column = None;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.selected_fragments.as_ref().map_or(self.anchor == self.end, |selected| selected.is_empty())
    }

    pub(super) fn copy_request(&self, follow: bool) -> Option<CopyRequest> {
        Some(CopyRequest { text: self.copy()?, identity: self.identity.clone(), anchor: self.anchor, end: self.end, follow })
    }

    fn ranges_on(&self, line: usize) -> Vec<Range<usize>> {
        if let Some(selected) = &self.selected_fragments {
            return selected.iter().filter(|(selected_line, _)| *selected_line == line).map(|(_, range)| range.clone()).collect();
        }
        let (start, end) = self.range();
        if line < start.line || line > end.line || start == end {
            return vec![];
        }
        std::iter::once(if line == start.line { start.offset } else { 0 }..if line == end.line { end.offset } else { self.text[line].len() }).collect()
    }

    fn range(&self) -> (Point, Point) {
        if self.anchor <= self.end {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    fn copy(&self) -> Option<String> {
        if let Some(selected) = &self.selected_fragments {
            let mut output = Vec::new();
            let mut index = 0;
            while index < selected.len() {
                let (line, range) = &selected[index];
                if let Some(table) = self.tables.get(*line).and_then(Option::as_ref) {
                    let mut rows = Vec::new();
                    let first = index;
                    while index < selected.len() {
                        let (line, range) = &selected[index];
                        let Some(row) = self.tables.get(*line).and_then(Option::as_ref).filter(|row| Arc::ptr_eq(&row.table, &table.table)) else { break };
                        rows.push((row.clone(), range.clone()));
                        index += 1;
                    }
                    output.push(super::table_copy::copy_selected(&rows, first == 0 && index == selected.len()));
                } else {
                    output.push(self.text[*line][range.clone()].to_string());
                    index += 1;
                }
            }
            let text = output.join("\n");
            return (!text.is_empty()).then_some(text);
        }
        let (start, end) = self.range();
        if start == end {
            return None;
        }
        let mut blocks = Vec::new();
        let mut line = start.line;
        while line <= end.line {
            let range = |line: usize| {
                let from = if line == start.line { start.offset } else { 0 };
                let to = if line == end.line { end.offset } else { self.text[line].len() };
                from..to
            };
            if let Some(table) = self.tables.get(line).and_then(Option::as_ref) {
                let mut rows = Vec::new();
                let first = line;
                while line <= end.line {
                    let Some(row) = self.tables.get(line).and_then(Option::as_ref).filter(|row| Arc::ptr_eq(&row.table, &table.table)) else { break };
                    rows.push((row.clone(), range(line)));
                    line += 1;
                }
                blocks.push(super::table_copy::copy_selected(&rows, first == start.line && line > end.line));
            } else {
                blocks.push(self.text[line][range(line)].to_string());
                line += 1;
            }
        }
        let selected = blocks.join("\n");
        (!selected.is_empty()).then_some(selected)
    }
}

pub(super) fn highlight(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return text.to_string();
    }
    // Keep style positions in the plain byte projection. Segmentation must happen after
    // removing SGR, since a style change can occur inside a single emoji/grapheme.
    let mut plain = String::new();
    let mut styles = Vec::new();
    let mut last = 0;
    for sgr in re!(r"\x1b\[[0-9;:]*m").find_iter(text) {
        plain.push_str(&crate::term::strip_vt(&text[last..sgr.start()]));
        styles.push((plain.len(), sgr.as_str()));
        last = sgr.end();
    }
    plain.push_str(&crate::term::strip_vt(&text[last..]));
    let mut styles = styles.into_iter().peekable();
    let mut col = 0;
    let mut output = String::new();
    let mut selected = false;
    let mut source_reverse = false;
    for (offset, g) in plain.grapheme_indices(true) {
        while styles.peek().is_some_and(|(pos, _)| *pos == offset) {
            let (_, sgr) = styles.next().unwrap();
            append_style(&mut output, sgr, selected, &mut source_reverse);
        }
        let right = col + visible_length(g);
        let active = right > start && col < end;
        if active != selected {
            output.push_str(if active { "\x1b[7m" } else { "\x1b[27m" });
            if !active && source_reverse {
                output.push_str("\x1b[7m");
            }
            selected = active;
        }
        let mut cursor = offset;
        while styles.peek().is_some_and(|(pos, _)| *pos < offset + g.len()) {
            let (pos, sgr) = styles.next().unwrap();
            output.push_str(&plain[cursor..pos]);
            append_style(&mut output, sgr, selected, &mut source_reverse);
            cursor = pos;
        }
        output.push_str(&plain[cursor..offset + g.len()]);
        col = right;
    }
    for (_, sgr) in styles {
        append_style(&mut output, sgr, selected, &mut source_reverse);
    }
    if end > col {
        if !selected {
            output.push_str("\x1b[7m");
        }
        output.push_str(&" ".repeat(end - col));
    }
    output.push_str("\x1b[0m");
    output
}

fn append_style(output: &mut String, sgr: &str, selected: bool, source_reverse: &mut bool) {
    let mut codes = sgr[2..sgr.len() - 1].split(';');
    while let Some(code) = codes.next() {
        match code {
            "" | "0" => *source_reverse = false,
            "7" => *source_reverse = true,
            "27" => *source_reverse = false,
            "38" | "48" | "58" => {
                // Extended-color components are values, not independent SGR commands.
                let count = match codes.next() {
                    Some("5") => 1,
                    Some("2") => 3,
                    _ => 0,
                };
                for _ in 0..count {
                    codes.next();
                }
            }
            _ => {}
        }
    }
    output.push_str(sgr);
    if selected {
        output.push_str("\x1b[7m");
    }
}

impl Tui {
    pub(super) fn finish_selection_copy(&self, request: &CopyRequest, status: crate::cli::ClipboardStatus) {
        let mut st = self.state.lock().unwrap();
        if st.closed || !st.selection.as_ref().is_some_and(|s| Arc::ptr_eq(&s.identity, &request.identity) && s.anchor == request.anchor && s.end == request.end) {
            return; // A late clipboard result cannot clear a new/extended selection.
        }
        if status == crate::cli::ClipboardStatus::Confirmed {
            st.selection = None;
            st.selection_dragging = false;
            if request.follow && (st.pending.is_none() || st.remember) && st.picker.is_none() {
                st.scroll = 0;
                st.mouse_wheel = pointer::Wheel::default();
                st.follow_control = pointer::Follow::default();
            }
        }
        st.status_notice = Some(status.message("selection"));
        self.schedule(&mut st, 0);
    }

    pub fn selected_transcript_text(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        if st.native.is_some() && !st.in_transcript_overlay {
            return None;
        }
        st.selection.as_ref()?.copy()
    }

    pub(super) fn select_body(st: &mut State, body: &mut [String], top: usize, screen_row: usize, width: usize) {
        st.transcript_top_row = top;
        st.transcript_screen_row = screen_row;
        st.transcript_view_rows = body.to_vec();
        if let Some(selection) = &st.selection {
            for (index, line) in body.iter_mut().enumerate() {
                for (start, end) in st.body_layout.columns(top + index, selection) {
                    *line = highlight(line, start, end.min(width));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_endpoint_uses_graphemes_and_keeps_preferred_visual_column() {
        let layout = Layout {
            tables: vec![],
            text: vec![Arc::from("a👩‍💻b"), Arc::from("123456"), Arc::from("x"), Arc::from("abcdef")],
            rows: vec![
                LayoutRow { line: 0, range: 0..13, prefix: 0, visible: "a👩‍💻b".into(), fragments: None },
                LayoutRow { line: 1, range: 0..3, prefix: 0, visible: "123".into(), fragments: None },
                LayoutRow { line: 1, range: 3..6, prefix: 0, visible: "456".into(), fragments: None },
                LayoutRow { line: usize::MAX, range: 0..0, prefix: 0, visible: String::new(), fragments: None },
                LayoutRow { line: 2, range: 0..1, prefix: 0, visible: "x".into(), fragments: None },
                LayoutRow { line: 3, range: 0..6, prefix: 0, visible: "abcdef".into(), fragments: None },
            ],
        };
        let mut selection = Selection::new(Point { line: 0, offset: 0 }, "", &layout);
        selection.end.offset = 1;
        selection.move_endpoint(&layout, "right");
        assert_eq!(selection.copy().as_deref(), Some("a👩‍💻"));
        selection.move_endpoint(&layout, "left");
        assert_eq!(selection.end.offset, 1);
        selection.end = Point { line: 1, offset: 2 };
        selection.reset_navigation();
        assert_eq!(selection.move_endpoint(&layout, "down"), Some(2));
        assert_eq!(selection.end, Point { line: 1, offset: 5 });
        assert_eq!(selection.move_endpoint(&layout, "down"), Some(4));
        assert_eq!(selection.end, Point { line: 2, offset: 1 });
        assert_eq!(selection.move_endpoint(&layout, "down"), Some(5));
        assert_eq!(selection.end, Point { line: 3, offset: 2 }, "short row cannot erase preferred column");
        selection.move_endpoint(&layout, "up");
        selection.move_endpoint(&layout, "up");
        assert_eq!(selection.end, Point { line: 1, offset: 5 });
        selection.end = Point { line: 1, offset: 0 };
        selection.move_endpoint(&layout, "left");
        assert_eq!(selection.end, Point { line: 0, offset: 13 });
        selection.move_endpoint(&layout, "right");
        assert_eq!(selection.end, Point { line: 1, offset: 0 });
    }

    #[test]
    fn visual_wrap_boundary_navigation_invalidates_copy_even_with_unchanged_anchors() {
        let layout = Layout {
            tables: vec![],
            text: vec![Arc::from("abcdef")],
            rows: vec![LayoutRow { line: 0, range: 0..3, prefix: 0, visible: "abc".into(), fragments: None }, LayoutRow { line: 0, range: 3..6, prefix: 3, visible: "def".into(), fragments: None }],
        };
        let mut selection = Selection::new(Point { line: 0, offset: 0 }, "", &layout);
        selection.end.offset = 3;
        selection.keyboard_row = Some(0);
        let request = selection.copy_request(true).unwrap();
        assert_eq!(selection.move_endpoint(&layout, "down"), Some(1));
        assert_eq!(selection.end.offset, 3);
        assert!(!Arc::ptr_eq(&selection.identity, &request.identity));
        let request = selection.copy_request(true).unwrap();
        assert_eq!(selection.move_endpoint(&layout, "up"), Some(0));
        assert_eq!(selection.end.offset, 3);
        assert!(!Arc::ptr_eq(&selection.identity, &request.identity));
        let request = selection.copy_request(true).unwrap();
        assert_eq!(selection.move_endpoint(&layout, "up"), Some(0));
        assert!(Arc::ptr_eq(&selection.identity, &request.identity), "no-op navigation retains copy ownership");
    }

    #[test]
    fn word_and_line_drags_expand_whole_units_in_both_directions() {
        let layout = Layout { text: vec![Arc::from("one café three"), Arc::from(""), Arc::from("last word")], rows: vec![], tables: vec![] };
        let point = Point { line: 0, offset: 5 };
        let mut selection = Selection::begin(point, "", &layout, 2);
        assert_eq!(selection.copy().as_deref(), Some("café"));
        selection.extend(Point { line: 0, offset: 10 });
        assert_eq!(selection.copy().as_deref(), Some("café three"));
        selection.extend(Point { line: 0, offset: 1 });
        assert_eq!(selection.copy().as_deref(), Some("one café"));
        let mut selection = Selection::begin(Point { line: 1, offset: 0 }, "", &layout, 3);
        assert_eq!(selection.copy().as_deref(), Some("\n"));
        selection.extend(Point { line: 0, offset: 2 });
        assert_eq!(selection.copy().as_deref(), Some("one café three\n\n"));
        selection.extend(Point { line: 2, offset: 3 });
        assert_eq!(selection.copy().as_deref(), Some("\nlast word"));
    }

    #[test]
    fn selection_preserves_source_styles_and_survives_resets_inside_graphemes() {
        let styled = "\x1b[31mred\x1b[0m plain";
        let result = highlight(styled, 1, 2);
        assert!(result.contains("\x1b[31m"), "selection must not erase original color: {result:?}");
        assert!(result.contains("\x1b[7me\x1b[27m"), "only selected character is reversed: {result:?}");
        assert_eq!(crate::term::strip_vt(&result), "red plain");

        let emoji = "a👩\x1b[0m‍💻b";
        let result = highlight(emoji, 1, 3);
        assert!(result.contains("\x1b[7m👩\x1b[0m\x1b[7m‍💻\x1b[27m"), "reset cannot split the selected emoji: {result:?}");
        assert_eq!(crate::term::strip_vt(&result), "a👩‍💻b");

        let result = highlight("\x1b[7mabcd\x1b[0m", 1, 2);
        assert!(result.contains("\x1b[27m\x1b[7mcd"), "source reverse style resumes after selection: {result:?}");
        for color in ["\x1b[38;2;0;7;27m", "\x1b[48;5;7m", "\x1b[38:2::0:7:27m"] {
            let result = highlight(&format!("{color}abcd\x1b[0m"), 1, 2);
            assert!(result.starts_with(color), "extended color must be retained");
            assert!(result.contains("\x1b[27mcd"), "color components cannot change source reverse state: {result:?}");
        }
        let result = highlight("\x1b[31ma\x1b[m\x1b[1mb\x1b[0mc", 0, 2);
        assert!(result.contains("\x1b[m\x1b[7m\x1b[1m\x1b[7mb"), "empty reset and bold retain selected range: {result:?}");
    }

    #[test]
    fn hard_newline_has_a_selected_cell_but_soft_wrap_and_unselected_newline_do_not() {
        let code: Arc<str> = Arc::from("code");
        let next: Arc<str> = Arc::from("next");
        let layout = Layout {
            tables: vec![],
            text: vec![code.clone(), next],
            rows: vec![
                LayoutRow { line: 0, range: 0..2, prefix: 0, visible: "co".into(), fragments: None },
                LayoutRow { line: 0, range: 2..4, prefix: 0, visible: "de".into(), fragments: None },
                LayoutRow { line: 1, range: 0..4, prefix: 0, visible: "next".into(), fragments: None },
            ],
        };
        let mut selection = Selection::new(Point { line: 0, offset: 0 }, "code\nnext", &layout);
        selection.end = Point { line: 0, offset: 4 };
        assert_eq!(layout.plain_columns(0, &selection), Some((0, 2)), "soft-wrap boundary is not a newline");
        assert_eq!(layout.plain_columns(1, &selection), Some((0, 2)), "same-line selection excludes newline");
        selection.end = Point { line: 1, offset: 0 };
        assert_eq!(selection.copy().as_deref(), Some("code\n"));
        assert_eq!(layout.plain_columns(1, &selection), Some((0, 3)), "selected hard newline gets exactly one cell");
        assert_eq!(highlight("de", 0, 3), "\x1b[7mde \x1b[0m");
        selection.anchor = Point { line: 0, offset: 4 };
        assert_eq!(selection.copy().as_deref(), Some("\n"));
        assert_eq!(layout.plain_columns(1, &selection), Some((2, 3)));
        assert_eq!(highlight("de", 2, 3), "de\x1b[7m \x1b[0m");
        std::mem::swap(&mut selection.anchor, &mut selection.end);
        assert_eq!(layout.plain_columns(1, &selection), Some((2, 3)), "reverse drag selects the same hard break");
        assert_eq!(highlight("de", 2, 2), "de", "a newline clipped at the viewport edge does not wrap");
        let blank_layout = Layout {
            tables: vec![],
            text: vec![Arc::from("code"), Arc::from(""), Arc::from("next")],
            rows: vec![
                LayoutRow { line: 0, range: 0..4, prefix: 0, visible: "code".into(), fragments: None },
                LayoutRow { line: 1, range: 0..0, prefix: 0, visible: String::new(), fragments: None },
                LayoutRow { line: 2, range: 0..4, prefix: 0, visible: "next".into(), fragments: None },
            ],
        };
        let mut selection = Selection::new(Point { line: 0, offset: 0 }, "code\n\nnext", &blank_layout);
        selection.end = Point { line: 2, offset: 0 };
        assert_eq!(selection.copy().as_deref(), Some("code\n\n"));
        assert_eq!(blank_layout.plain_columns(1, &selection), None, "Codex leaves empty source rows visually empty");
    }
}
