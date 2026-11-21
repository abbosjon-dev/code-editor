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

/// Can `line` be folded (the next non-blank line is indented deeper)?
pub fn is_foldable(buf: &Buffer, line: usize) -> bool {
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
pub fn fold_end(buf: &Buffer, header: usize) -> Option<usize> {
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
pub fn enclosing_header(buf: &Buffer, line: usize) -> Option<usize> {
    if is_foldable(buf, line) {
        return Some(line);
    }
    let base = (0..=line).rev().find_map(|l| indent_of(buf, l))?;
    (0..line)
        .rev()
        .find(|&l| indent_of(buf, l).is_some_and(|i| i < base) && fold_end(buf, l).is_some_and(|e| e >= line))
}

/// Mapping between document lines and visible rows.
#[derive(Debug, Default)]
pub struct FoldMap {
    /// Visible document lines in order. Empty = no folds (identity mapping).
    rows: Vec<usize>,
    total_lines: usize,
}

impl FoldMap {
    pub fn build(buf: &Buffer, folds: &BTreeSet<usize>) -> Self {
        let n = buf.len_lines();
        if folds.is_empty() {
            return Self { rows: vec![], total_lines: n };
        }
        let mut hidden: Vec<(usize, usize)> =
            folds.iter().filter(|&&h| h < n).filter_map(|&h| fold_end(buf, h).map(|e| (h + 1, e))).collect();
        hidden.sort_unstable();
        let mut rows = Vec::with_capacity(n);
        let mut hi = 0;
        let mut line = 0;
        while line < n {
            while hi < hidden.len() && hidden[hi].1 < line {
                hi += 1;
            }
            if hi < hidden.len() && hidden[hi].0 <= line {
                line = hidden[hi].1 + 1;
                continue;
            }
            rows.push(line);
            line += 1;
        }
        Self { rows, total_lines: n }
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
            self.rows[row.min(self.rows.len().saturating_sub(1))]
        }
    }

    /// Row showing `line`; hidden lines map to their fold header's row.
    pub fn row_of(&self, line: usize) -> usize {
        if self.is_identity() {
            return line;
        }
        match self.rows.binary_search(&line) {
            Ok(r) => r,
            Err(r) => r.saturating_sub(1),
        }
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        !self.is_identity() && self.rows.binary_search(&line).is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn a() {\n    let x = 1;\n\n    if x {\n        y();\n    }\n}\nfn b() {}\n";

    #[test]
    fn foldable_and_ends() {
        let b = Buffer::from_str(SRC);
        assert!(is_foldable(&b, 0));
        assert_eq!(fold_end(&b, 0), Some(5));
        assert_eq!(fold_end(&b, 3), Some(4));
        assert!(!is_foldable(&b, 7));
        assert_eq!(enclosing_header(&b, 4), Some(3));
        assert_eq!(enclosing_header(&b, 1), Some(0));
    }

    #[test]
    fn map_rows() {
        let b = Buffer::from_str(SRC);
        let m = FoldMap::build(&b, &BTreeSet::from([0]));
        // Lines 1..=5 hidden.
        assert_eq!(m.row_count(), b.len_lines() - 5);
        assert_eq!(m.line_at(1), 6);
        assert_eq!(m.row_of(3), 0);
        assert!(m.is_hidden(4));
        assert!(!m.is_hidden(6));
        let nested = FoldMap::build(&b, &BTreeSet::from([0, 3]));
        assert_eq!(nested.row_count(), m.row_count());
        let id = FoldMap::build(&b, &BTreeSet::new());
        assert!(id.is_identity());
        assert_eq!(id.row_of(4), 4);
    }
}
