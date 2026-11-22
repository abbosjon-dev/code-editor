//! Code folding based on indentation (works for every language).
//!
//! A fold is identified by its header line. The lines hidden by a fold are the
//! following lines indented deeper than the header (blank lines inside are
//! included); the closing line (e.g. `}`) stays visible.

use std::collections::BTreeSet;

use zenith_core::Buffer;

fn indent_of(buf: &Buffer, line: usize) -> Option<usize> {
    let text = buf.line_text(line);
    if text.trim().is_empty() {
        return None;
    }
    let tab = buf.tab_size.max(1);
    let mut v = 0;
    for c in text.chars() {
        match c {
            ' ' => v += 1,
            '\t' => v += tab - v % tab,
            _ => break,
        }
    }
    Some(v)
}

/// Fold ranges from a syntax tree: header line → last hidden line.
pub type Ranges = std::collections::BTreeMap<usize, usize>;

/// Can `line` be folded? Syntax ranges when known, else indentation (the
/// next non-blank line is indented deeper).
pub fn is_foldable(buf: &Buffer, line: usize, ranges: Option<&Ranges>) -> bool {
    if let Some(r) = ranges {
        return r.contains_key(&line);
    }
    let Some(base) = indent_of(buf, line) else { return false };
    let n = buf.len_lines();
    for l in line + 1..n.min(line + 200) {
        if let Some(i) = indent_of(buf, l) {
            return i > base;
        }
    }
    false
}

/// Last line hidden by folding `header`, if foldable.
pub fn fold_end(buf: &Buffer, header: usize, ranges: Option<&Ranges>) -> Option<usize> {
    if let Some(r) = ranges {
        return r.get(&header).copied();
    }
    let base = indent_of(buf, header)?;
    let mut end = None;
    for l in header + 1..buf.len_lines() {
        match indent_of(buf, l) {
            None => continue,
            Some(i) if i > base => end = Some(l),
            Some(_) => break,
        }
    }
    end
}

/// Innermost foldable header containing `line` (or `line` itself).
pub fn enclosing_header(buf: &Buffer, line: usize, ranges: Option<&Ranges>) -> Option<usize> {
    if let Some(r) = ranges {
        if r.contains_key(&line) {
            return Some(line);
        }
        // Innermost range around the line: the nearest header above that reaches it.
        return r.range(..line).rev().find(|(_, &e)| e >= line).map(|(&h, _)| h);
    }
    if is_foldable(buf, line, None) {
        return Some(line);
    }
    let base = (0..=line).rev().find_map(|l| indent_of(buf, l))?;
    (0..line)
        .rev()
        .find(|&l| indent_of(buf, l).is_some_and(|i| i < base) && fold_end(buf, l, None).is_some_and(|e| e >= line))
}

/// Mapping between document lines and visible rows (folding + soft wrap).
///
/// Each row is `(document line, first char column of the row)`. With no
/// folds and no wrapping the map is the identity and stores nothing.
#[derive(Debug, Default)]
pub struct FoldMap {
    rows: Vec<(usize, usize)>,
    total_lines: usize,
    /// Soft-wrap width in visual columns (0 = off).
    pub wrap_cols: usize,
}

/// Files with more lines than this are never soft-wrapped (keeps edits O(1)).
pub const MAX_WRAP_LINES: usize = 50_000;

/// Char columns where the rows of a wrapped line start (always begins with 0).
/// Breaks after whitespace when possible, otherwise mid-word.
pub fn wrap_points(chars: &[char], wrap_cols: usize, tab: usize) -> Vec<usize> {
    use unicode_width::UnicodeWidthChar;
    let mut out = vec![0];
    if wrap_cols == 0 {
        return out;
    }
    let mut row_start = 0;
    let mut width = 0;
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let w = if c == '\t' { tab - width % tab } else { c.width().unwrap_or(1).max(1) };
        if width + w > wrap_cols && i > row_start {
            // Break after the last space in this row, if any.
            let brk = match last_space {
                Some(sp) if sp + 1 > row_start && sp + 1 < i + 1 && sp + 1 > row_start => sp + 1,
                _ => i,
            };
            out.push(brk);
            row_start = brk;
            last_space = None;
            width = 0;
            i = brk;
            continue;
        }
        if c == ' ' || c == '\t' {
            last_space = Some(i);
        }
        width += w;
        i += 1;
    }
    out
}

impl FoldMap {
    pub fn build(buf: &Buffer, folds: &BTreeSet<usize>, wrap_cols: usize, ranges: Option<&Ranges>) -> Self {
        let n = buf.len_lines();
        let wrap_cols = if n > MAX_WRAP_LINES { 0 } else { wrap_cols };
        if folds.is_empty() && wrap_cols == 0 {
            return Self { rows: vec![], total_lines: n, wrap_cols };
        }
        let mut hidden: Vec<(usize, usize)> =
            folds.iter().filter(|&&h| h < n).filter_map(|&h| fold_end(buf, h, ranges).map(|e| (h + 1, e))).collect();
        hidden.sort_unstable();
        let tab = buf.tab_size.max(1);
        let mut rows = Vec::with_capacity(n);
        let mut hi = 0;
        let mut line = 0;
        let rope = buf.rope();
        while line < n {
            while hi < hidden.len() && hidden[hi].1 < line {
                hi += 1;
            }
            if hi < hidden.len() && hidden[hi].0 <= line {
                line = hidden[hi].1 + 1;
                continue;
            }
            let len = buf.line_len(line);
            if wrap_cols > 0 && len > wrap_cols / 2 {
                let chars: Vec<char> = rope.line(line).chars().take(len).collect();
                for col in wrap_points(&chars, wrap_cols, tab) {
                    rows.push((line, col));
                }
            } else {
                rows.push((line, 0));
            }
            line += 1;
        }
        Self { rows, total_lines: n, wrap_cols }
    }

    pub fn is_identity(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row_count(&self) -> usize {
        if self.is_identity() {
            self.total_lines
        } else {
            self.rows.len()
        }
    }

    /// Document line shown at `row`.
    pub fn line_at(&self, row: usize) -> usize {
        if self.is_identity() {
            row.min(self.total_lines.saturating_sub(1))
        } else {
            self.rows[row.min(self.rows.len().saturating_sub(1))].0
        }
    }

    /// Char column range `[start, end)` of `row` within its line (`end` is
    /// `usize::MAX` for the last row of a line).
    pub fn row_cols(&self, row: usize) -> (usize, usize) {
        if self.is_identity() || row >= self.rows.len() {
            return (0, usize::MAX);
        }
        let (line, start) = self.rows[row];
        let end = match self.rows.get(row + 1) {
            Some(&(l, c)) if l == line => c,
            _ => usize::MAX,
        };
        (start, end)
    }

    /// First row showing `line`; hidden lines map to their fold header's row.
    pub fn row_of(&self, line: usize) -> usize {
        if self.is_identity() {
            return line;
        }
        let i = self.rows.partition_point(|&(l, _)| l < line);
        match self.rows.get(i) {
            Some(&(l, _)) if l == line => i,
            _ => i.saturating_sub(1),
        }
    }

    /// Row containing char column `col` of `line`.
    pub fn row_of_pos(&self, line: usize, col: usize) -> usize {
        if self.is_identity() {
            return line;
        }
        let first = self.row_of(line);
        let mut r = first;
        while r + 1 < self.rows.len() && self.rows[r + 1].0 == line && self.rows[r + 1].1 <= col {
            r += 1;
        }
        r
    }

    /// Last row showing `line`.
    pub fn last_row_of(&self, line: usize) -> usize {
        if self.is_identity() {
            return line;
        }
        let mut r = self.row_of(line);
        while r + 1 < self.rows.len() && self.rows[r + 1].0 == line {
            r += 1;
        }
        r
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        if self.is_identity() {
            return false;
        }
        let i = self.rows.partition_point(|&(l, _)| l < line);
        !matches!(self.rows.get(i), Some(&(l, _)) if l == line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn a() {\n    let x = 1;\n\n    if x {\n        y();\n    }\n}\nfn b() {}\n";

    #[test]
    fn foldable_and_ends() {
        let b = Buffer::from_str(SRC);
        assert!(is_foldable(&b, 0, None));
        assert_eq!(fold_end(&b, 0, None), Some(5));
        assert_eq!(fold_end(&b, 3, None), Some(4));
        assert!(!is_foldable(&b, 7, None));
        assert_eq!(enclosing_header(&b, 4, None), Some(3));
        assert_eq!(enclosing_header(&b, 1, None), Some(0));
    }

    #[test]
    fn map_rows() {
        let b = Buffer::from_str(SRC);
        let m = FoldMap::build(&b, &BTreeSet::from([0]), 0, None);
        // Lines 1..=5 hidden.
        assert_eq!(m.row_count(), b.len_lines() - 5);
        assert_eq!(m.line_at(1), 6);
        assert_eq!(m.row_of(3), 0);
        assert!(m.is_hidden(4));
        assert!(!m.is_hidden(6));
        let nested = FoldMap::build(&b, &BTreeSet::from([0, 3]), 0, None);
        assert_eq!(nested.row_count(), m.row_count());
        let id = FoldMap::build(&b, &BTreeSet::new(), 0, None);
        assert!(id.is_identity());
        assert_eq!(id.row_of(4), 4);
    }

    #[test]
    fn wrap_points_prefer_spaces() {
        let c: Vec<char> = "hello world this is long".chars().collect();
        assert_eq!(wrap_points(&c, 12, 4), vec![0, 12]); // "this is long" fits exactly
        assert_eq!(wrap_points(&c, 10, 4), vec![0, 6, 12, 20]);
        let c: Vec<char> = "abcdefghij".chars().collect();
        assert_eq!(wrap_points(&c, 4, 4), vec![0, 4, 8]);
        assert_eq!(wrap_points(&c, 0, 4), vec![0]);
    }

    #[test]
    fn wrapped_rows() {
        let b = Buffer::from_str("short\nhello world this is long\nend");
        let m = FoldMap::build(&b, &BTreeSet::new(), 10, None);
        assert_eq!(m.row_count(), 6);
        assert_eq!(m.row_of(1), 1);
        assert_eq!(m.last_row_of(1), 4);
        assert_eq!(m.row_cols(2), (6, 12));
        assert_eq!(m.row_cols(4), (20, usize::MAX));
        assert_eq!(m.row_of_pos(1, 13), 3);
        assert_eq!(m.row_of_pos(1, 12), 3);
        assert_eq!(m.line_at(5), 2);
        assert!(!m.is_hidden(1));
    }
}
