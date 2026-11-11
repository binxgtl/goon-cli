//! Content-aware table layout, adapted from Codex's Markdown table allocation.
//!
//! Widths are terminal cells, excluding each column's padding and grid borders.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Grid(Vec<usize>),
    Records { aligned: bool, label_width: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    TokenHeavy,
    Narrative,
    Compact,
}

#[derive(Debug)]
struct Metrics {
    max_width: usize,
    header_token_width: usize,
    body_token_width: usize,
    kind: Kind,
}

pub(super) fn choose(headers: &[String], rows: &[Vec<String>], available: usize) -> Mode {
    let metrics = collect_table_column_metrics(headers, rows);
    // `│ x │ y │`: two padding cells and one separator per column, plus
    // the opening border. The caller has already removed external prefixes.
    let overhead = headers.len().saturating_mul(3).saturating_add(1);
    if let Some(widths) = available.checked_sub(overhead).and_then(|width| compute_column_widths(&metrics, width)) {
        if !should_render_records(rows, &widths, &metrics) {
            return Mode::Grid(widths);
        }
    }
    let label_width = headers.iter().map(|header| display_width(header)).max().unwrap_or(0);
    let minimum_value = if metrics.iter().any(|metric| metric.kind != Kind::Compact) { 24 } else { 12 };
    Mode::Records { aligned: label_width.saturating_add(3).saturating_add(minimum_value) <= available, label_width }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn cell_width(text: &str) -> usize {
    text.split('\n').map(display_width).max().unwrap_or(0)
}

fn longest_token_width(text: &str) -> usize {
    text.split_whitespace().map(display_width).max().unwrap_or(0)
}

fn collect_table_column_metrics(headers: &[String], rows: &[Vec<String>]) -> Vec<Metrics> {
    headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let mut max_width = cell_width(header);
            let mut body_token_width = 0;
            let mut body_token_count = 0usize;
            let mut long_body_token_count = 0usize;
            let mut total_cells = 0usize;
            let mut total_cell_width = 0usize;
            for row in rows {
                let plain = row.get(column).map(String::as_str).unwrap_or("");
                max_width = max_width.max(cell_width(plain));
                let mut words = 0;
                for token in plain.split_whitespace() {
                    let width = display_width(token);
                    body_token_width = body_token_width.max(width);
                    long_body_token_count += usize::from(width >= 20);
                    words += 1;
                }
                if words > 0 {
                    body_token_count += words;
                    total_cells += 1;
                    total_cell_width += display_width(plain);
                }
            }
            let (avg_words, avg_width) = if total_cells == 0 {
                (header.split_whitespace().count() as f64, display_width(header) as f64)
            } else {
                (body_token_count as f64 / total_cells as f64, total_cell_width as f64 / total_cells as f64)
            };
            let kind = if long_body_token_count > 0 && long_body_token_count >= body_token_count.saturating_sub(long_body_token_count) {
                Kind::TokenHeavy
            } else if avg_words >= 4.0 || avg_width >= 28.0 {
                Kind::Narrative
            } else {
                Kind::Compact
            };
            Metrics { max_width, header_token_width: longest_token_width(header), body_token_width, kind }
        })
        .collect()
}

fn preferred_column_floor(metric: &Metrics) -> usize {
    let target = match metric.kind {
        Kind::Narrative | Kind::TokenHeavy => 16,
        Kind::Compact => metric.header_token_width.max(metric.body_token_width.min(16)),
    };
    target.min(metric.max_width).max(3)
}

fn compute_column_widths(metrics: &[Metrics], available: usize) -> Option<Vec<usize>> {
    if metrics.len().saturating_mul(3) > available {
        return None;
    }
    let mut floors: Vec<_> = metrics.iter().map(preferred_column_floor).collect();
    let floor_total: usize = floors.iter().sum();
    if floor_total > available {
        shrink_columns(&mut floors, &vec![3; metrics.len()], metrics, floor_total - available);
    }
    let mut widths: Vec<_> = metrics.iter().map(|metric| metric.max_width.max(3)).collect();
    let total: usize = widths.iter().sum();
    if total > available && shrink_columns(&mut widths, &floors, metrics, total - available) > 0 {
        return None;
    }
    Some(widths)
}

/// Water-fill the slack in bulk, preserving the reference's column-index tie
/// break. Runtime depends logarithmically, not linearly, on oversized tokens.
fn shrink_columns(widths: &mut [usize], floors: &[usize], metrics: &[Metrics], mut amount: usize) -> usize {
    for kind in [Kind::TokenHeavy, Kind::Narrative, Kind::Compact] {
        let slack = |idx: usize, width: usize| if metrics[idx].kind == kind { width.saturating_sub(floors[idx]) } else { 0 };
        let total: usize = widths.iter().enumerate().map(|(idx, width)| slack(idx, *width)).sum();
        let to_remove = amount.min(total);
        if to_remove == 0 {
            continue;
        }
        let mut low = 0;
        let mut high = widths.iter().enumerate().map(|(idx, width)| slack(idx, *width)).max().unwrap_or(0);
        while low < high {
            let cap = low + (high - low) / 2;
            let removed: usize = widths.iter().enumerate().map(|(idx, width)| slack(idx, *width).saturating_sub(cap)).sum();
            if removed > to_remove {
                low = cap + 1;
            } else {
                high = cap;
            }
        }
        let cap = low;
        let mut removed = 0;
        for (idx, width) in widths.iter_mut().enumerate() {
            let reduction = slack(idx, *width).saturating_sub(cap);
            *width -= reduction;
            removed += reduction;
        }
        let mut remainder = to_remove - removed;
        for (idx, width) in widths.iter_mut().enumerate() {
            if remainder == 0 {
                break;
            }
            if metrics[idx].kind == kind && width.saturating_sub(floors[idx]) == cap {
                *width -= 1;
                remainder -= 1;
            }
        }
        amount -= to_remove;
        if amount == 0 {
            break;
        }
    }
    amount
}

fn should_render_records(rows: &[Vec<String>], widths: &[usize], metrics: &[Metrics]) -> bool {
    if rows.is_empty() {
        return false;
    }
    let affected = rows
        .iter()
        .filter(|row| {
            let mut cramped = 0;
            for (column, (width, metric)) in widths.iter().zip(metrics).enumerate() {
                let cell = row.get(column).map(String::as_str).unwrap_or("");
                let fragmented = match metric.kind {
                    Kind::Compact => true,
                    Kind::TokenHeavy => *width < 12,
                    Kind::Narrative => false,
                } && cell.split_whitespace().any(|token| display_width(token) > *width);
                if fragmented {
                    return true;
                }
                if metric.kind != Kind::Compact {
                    let height = wrapped_height(cell, *width);
                    cramped += usize::from(height >= 4);
                    if cramped >= 2 || (metric.kind == Kind::Narrative && *width < 12 && height >= 7) {
                        return true;
                    }
                }
            }
            false
        })
        .count();
    let threshold = if rows.len() == 1 { 1 } else { 2.max(rows.len().div_ceil(3)) };
    affected >= threshold
}

/// Greedy word wrapping with grapheme-safe splitting of oversized words. Only
/// heights up to seven matter to the decision, so avoid allocating wrapped text.
fn wrapped_height(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut height = 0;
    for line in text.split('\n') {
        height += 1;
        let mut used = 0;
        for word in line.split_whitespace() {
            let word_width = display_width(word);
            if used > 0 {
                if used + 1 + word_width <= width {
                    used += 1 + word_width;
                    continue;
                }
                height += 1;
                used = 0;
            }
            if word_width <= width {
                used = word_width;
            } else {
                for grapheme in word.graphemes(true) {
                    let size = display_width(grapheme);
                    if used > 0 && used + size > width {
                        height += 1;
                        used = 0;
                    }
                    used += size;
                    if height >= 7 {
                        return 7;
                    }
                }
            }
            if height >= 7 {
                return 7;
            }
        }
        if height >= 7 {
            return 7;
        }
    }
    height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn grid_accounts_for_borders_and_minimums() {
        let headers = strings(&["Name", "State"]);
        let rows = vec![strings(&["Ada", "ready"])];
        assert_eq!(choose(&headers, &rows, 16), Mode::Grid(vec![4, 5]));
        assert!(matches!(choose(&headers, &rows, 12), Mode::Records { .. }));
        assert_eq!(choose(&strings(&["", ""]), &[], 13), Mode::Grid(vec![3, 3]));
    }

    #[test]
    fn fragmented_compact_values_switch_after_enough_rows() {
        let headers = strings(&["Name", "State"]);
        let broken = strings(&["Alexander", "pending"]);
        assert!(matches!(choose(&headers, std::slice::from_ref(&broken), 18), Mode::Records { .. }));
        assert_eq!(choose(&headers, std::slice::from_ref(&broken), 20), Mode::Records { aligned: true, label_width: 5 });
        assert_eq!(choose(&headers, std::slice::from_ref(&broken), 19), Mode::Records { aligned: false, label_width: 5 });
        assert_eq!(choose(&headers, &[], 12), Mode::Records { aligned: false, label_width: 5 });
        assert!(matches!(choose(&headers, std::slice::from_ref(&broken), 0), Mode::Records { aligned: false, .. }));
        assert_eq!(choose(&headers, std::slice::from_ref(&broken), 23), Mode::Grid(vec![9, 7]));
        let small = strings(&["Al", "ok"]);
        assert!(matches!(choose(&headers, &[broken.clone(), small.clone()], 18), Mode::Grid(_)));
        assert!(matches!(choose(&headers, &[broken.clone(), broken, small], 18), Mode::Records { .. }));
    }

    #[test]
    fn narrative_starvation_switches_to_records() {
        let headers = strings(&["Detail", "Reason"]);
        let prose = "one two three four five six seven eight nine ten eleven twelve";
        let rows = vec![strings(&[prose, prose])];
        assert!(matches!(choose(&headers, &rows, 39), Mode::Records { aligned: true, .. }));
        assert!(matches!(choose(&headers, &rows, 30), Mode::Records { aligned: false, .. }));
        assert!(matches!(choose(&headers, &rows, 100), Mode::Grid(_)));
    }

    #[test]
    fn unicode_uses_display_cells_not_bytes_or_codepoints() {
        let headers = strings(&["名称", "e\u{301}"]);
        let rows = vec![strings(&["東京", "👩‍💻"])];
        assert_eq!(choose(&headers, &rows, 14), Mode::Grid(vec![4, 3]));
        assert_eq!(wrapped_height("東京東京", 3), 4);
        assert_eq!(wrapped_height("e\u{301}e\u{301}", 1), 2);
    }

    #[test]
    fn bulk_shrink_preserves_priority_and_balances_ties() {
        let metric = |kind| Metrics { max_width: 1_000_000_000, header_token_width: 3, body_token_width: 3, kind };
        let metrics = vec![metric(Kind::TokenHeavy), metric(Kind::TokenHeavy), metric(Kind::Narrative), metric(Kind::Compact)];
        let mut widths = vec![1_000_000_000, 1_000_000_000, 30, 10];
        assert_eq!(shrink_columns(&mut widths, &[3, 3, 3, 3], &metrics, 1_999_999_991), 0);
        assert_eq!(widths, vec![4, 5, 30, 10]);
    }

    #[test]
    fn huge_tokens_do_not_need_per_cell_shrinking() {
        let headers = strings(&["Path", "State"]);
        let rows = vec![vec!["x".repeat(100_000), "ok".into()]];
        assert!(matches!(choose(&headers, &rows, 80), Mode::Grid(_)));
        assert!(matches!(choose(&headers, &rows, 20), Mode::Records { .. }));
    }
}
