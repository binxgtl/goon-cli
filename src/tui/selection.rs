//! Logical transcript selection with immutable revision ownership and wrapping provenance.
use super::*;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug)]
pub(super) struct Origin {
    pub text: Arc<str>,
    pub range: Range<usize>,
    pub projected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Point {
    pub line: usize,
    pub offset: usize,
}

#[derive(Clone, Default)]
pub(super) struct Layout {
    pub text: Vec<Arc<str>>,
    rows: Vec<LayoutRow>,
}

#[derive(Clone)]
struct LayoutRow {
    line: usize,
    range: Range<usize>,
    prefix: usize,
    visible: String,
}

impl Layout {
    pub(super) fn from_rows(rows: &[Row], styled: &[String], rich: bool) -> Self {
        let mut layout = Self::default();
        let mut previous: Option<&Arc<str>> = None;
        for (row, painted) in rows.iter().zip(styled) {
            if (row.origin.is_none() && row.kind == RowKind::Blank)
                || matches!(row.kind, RowKind::Rule | RowKind::CodeHead | RowKind::ReasoningHead | RowKind::BannerTitle | RowKind::Banner)
                || (row.kind == RowKind::Table && row.text.starts_with('├'))
            {
                layout.rows.push(LayoutRow { line: usize::MAX, range: 0..0, prefix: 0, visible: String::new() });
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
            if !continuation {
                layout.text.push(text.clone());
            }
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
            layout.rows.push(LayoutRow { line: layout.text.len() - 1, range, prefix, visible });
            previous = row.origin.as_ref().map(|o| &o.text);
        }
        layout
    }

    pub(super) fn hit(&self, row: usize, column: usize) -> Option<Point> {
        let row = self.rows.get(row)?;
        if row.line == usize::MAX {
            return None;
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

    fn columns(&self, row: usize, selection: &Selection) -> Option<(usize, usize)> {
        let row = self.rows.get(row)?;
        let (start, end) = selection.range();
        if row.line < start.line || row.line > end.line || start == end {
            return None;
        }
        let start = if row.line == start.line { start.offset.max(row.range.start) } else { row.range.start };
        let end = if row.line == end.line { end.offset.min(row.range.end) } else { row.range.end };
        if start >= end {
            return None;
        }
        let text = &self.text[row.line];
        Some((row.prefix + visible_length(&text[row.range.start..start.min(row.range.end)]), row.prefix + visible_length(&text[row.range.start..end])))
    }

    pub(super) fn row_for(&self, point: Point) -> Option<usize> {
        self.rows.iter().position(|row| row.line == point.line && row.range.start <= point.offset && point.offset <= row.range.end)
    }
}

pub(super) struct Selection {
    pub anchor: Point,
    pub end: Point,
    pub source: Arc<str>,
    text: Vec<Arc<str>>,
}

impl Selection {
    pub(super) fn new(point: Point, source: &str, layout: &Layout) -> Self {
        Self { anchor: point, end: point, source: Arc::from(source), text: layout.text.clone() }
    }

    fn range(&self) -> (Point, Point) {
        if self.anchor <= self.end {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    fn copy(&self) -> Option<String> {
        let (start, end) = self.range();
        if start == end {
            return None;
        }
        let selected = (start.line..=end.line)
            .map(|line| {
                let text = &self.text[line];
                let from = if line == start.line { start.offset } else { 0 };
                let to = if line == end.line { end.offset } else { text.len() };
                &text[from..to]
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!selected.is_empty()).then_some(selected)
    }
}

pub(super) fn highlight(text: &str, start: usize, end: usize) -> String {
    let plain = crate::term::strip_vt(text);
    let mut col = 0;
    let mut output = String::new();
    let mut selected = false;
    for g in plain.graphemes(true) {
        let right = col + visible_length(g);
        let active = right > start && col < end;
        if active != selected {
            output.push_str(if active { "\x1b[7m" } else { "\x1b[27m" });
            selected = active;
        }
        output.push_str(g);
        col = right;
    }
    output.push_str("\x1b[0m");
    output
}

impl Tui {
    pub fn selected_transcript_text(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        if st.native.is_some() && !st.in_transcript_overlay {
            return None;
        }
        st.selection.as_ref()?.copy()
    }

    pub(super) fn select_body(st: &mut State, body: &mut [String], top: usize, screen_row: usize) {
        st.transcript_top_row = top;
        st.transcript_screen_row = screen_row;
        st.transcript_view_rows = body.to_vec();
        if let Some(selection) = &st.selection {
            for (index, line) in body.iter_mut().enumerate() {
                if let Some((start, end)) = st.body_layout.columns(top + index, selection) {
                    *line = highlight(line, start, end);
                }
            }
        }
    }
}
