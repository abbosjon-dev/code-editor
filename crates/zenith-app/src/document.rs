//! An open document = buffer + highlighting + per-view state.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use syntect::highlighting::Theme;
use zenith_core::{Buffer, SearchOptions, Selection};

use crate::highlight::{self, HighlightCache};
use zenith_lsp::{CompletionItem, LspPos, LspRange, Severity};

/// A diagnostic mapped to char indices of the current buffer.
#[derive(Clone, Debug)]
pub struct DocDiagnostic {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub severity: Severity,
    pub message: String,
    /// As sent by the server (needed for code action requests).
    pub raw: serde_json::Value,
}

/// Completion popup state.
pub struct Completion {
    pub request: u64,
    /// Char index where the word being completed starts.
    pub anchor: usize,
    pub items: Vec<CompletionItem>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub received: bool,
}

/// Parameter hints for the call around the caret.
pub struct Signature {
    pub request: u64,
    pub line: usize,
    pub help: Option<zenith_lsp::SignatureHelp>,
}

/// A semantic token's color on `line`, chars `start..end`.
#[derive(Clone, Debug, PartialEq)]
pub struct SemSpan {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub color: eframe::egui::Color32,
    pub bold: bool,
    pub italic: bool,
}

/// An inlay hint placed before char `col` of `line`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayView {
    pub line: usize,
    pub col: usize,
    pub label: String,
}

pub struct Hover {
    pub request: u64,
    pub idx: usize,
    pub text: Option<String>,
}

#[derive(Default)]
pub struct FindState {
    pub open: bool,
    pub show_replace: bool,
    pub query: String,
    pub replacement: String,
    pub opts: SearchOptions,
    pub matches: Vec<Range<usize>>,
    pub current: usize,
    computed_for: Option<(String, u64, bool, bool)>,
    pub focus_query: bool,
}

pub struct Document {
    pub id: u64,
    pub buffer: Buffer,
    pub highlight: HighlightCache,
    pub find: FindState,
    pub untitled_name: String,
    /// Set when the caret moved via keyboard and should be scrolled into view.
    pub scroll_to_cursor: bool,
    /// Explicit line to center on (go-to-line, search result).
    pub reveal_line: Option<usize>,
    pub disk_mtime: Option<SystemTime>,
    longest_line: (u64, usize),
    longest_dirty: Option<(usize, usize)>,
    longest_full_at: Option<std::time::Instant>,
    /// How long the last full scan took.
    longest_full_cost: std::time::Duration,
    // --- language server state ---
    pub diagnostics: Vec<DocDiagnostic>,
    pub completion: Option<Completion>,
    pub hover: Option<Hover>,
    pub signature: Option<Signature>,
    /// Buffer version last sent to the language server.
    pub lsp_version: Option<u64>,
    pub lsp_sent_at: Option<std::time::Instant>,
    /// Characters that trigger completion for this language.
    pub lsp_triggers: Vec<char>,
    pub lsp_active: bool,
    // --- git ---
    /// File content at HEAD (None = untracked or not in a repo).
    pub git_base: Option<String>,
    pub git_hunks: crate::git::Hunks,
    git_hunks_version: Option<u64>,
    /// Git merge conflicts (marker lines) in the buffer.
    pub conflicts: Vec<crate::conflicts::Conflict>,
    /// Vim mode state (when enabled in settings).
    pub vim: crate::vim::VimState,
    /// Breakpoint lines (0-based); `breakpoints_gen` changes on every edit of the set.
    pub breakpoints: crate::debug::Breakpoints,
    pub breakpoints_gen: u64,
    /// Line where the debugger is stopped.
    pub debug_line: Option<usize>,
    /// Inlay hints sorted by position; the version they were computed for,
    /// and the request in flight (id, version).
    pub inlay_hints: Vec<InlayView>,
    pub inlay_version: Option<u64>,
    pub inlay_request: Option<(u64, u64)>,
    /// Empty answers are retried (servers answer [] while indexing).
    pub inlay_retry: (u8, Option<std::time::Instant>),
    /// Semantic token colors (sorted), their version and the request in flight.
    pub semantic: Vec<SemSpan>,
    pub semantic_version: Option<u64>,
    pub semantic_request: Option<(u64, u64)>,
    pub semantic_retry: (u8, Option<std::time::Instant>),
    /// The debugger is paused (hovering shows values).
    pub debug_paused: bool,
    /// Value of the expression under the mouse: (char index, "expr = value").
    pub debug_value: Option<(usize, String)>,
    conflicts_version: Option<u64>,
    /// Pending review of a change made outside the editor (e.g. by an AI agent).
    pub review: Option<crate::review::Review>,
    /// Show the side-by-side diff against HEAD instead of the editor.
    pub show_git_diff: bool,
    /// Markdown: show the rendered preview beside the editor.
    pub md_preview: bool,
    /// Per-line blame of the file on disk (valid while the buffer is clean).
    pub blame: Option<Vec<crate::git::BlameLine>>,
    pub blame_rx: Option<std::sync::mpsc::Receiver<Vec<crate::git::BlameLine>>>,
    /// Line whose git change popup (gutter click) is open.
    pub hunk_popup: Option<usize>,
    pub git_diff: crate::diff_view::DiffState,
    // --- folding ---
    /// Header lines of folded regions.
    pub folds: std::collections::BTreeSet<usize>,
    folds_gen: u64,
    /// Soft-wrap width in columns, set by the editor view each frame (0 = off).
    pub wrap_cols: usize,
    fold_cache: Option<(u64, u64, std::sync::Arc<crate::folding::FoldMap>)>,
    last_len_lines: usize,
}

/// Length in chars of the longest line (without its line break): one pass
/// over the bytes, counting chars as the bytes that don't continue one.
fn longest_line_scan(rope: &zenith_core::Rope) -> usize {
    let (mut best, mut cur) = (0usize, 0usize);
    for chunk in rope.chunks() {
        for &b in chunk.as_bytes() {
            if b == b'\n' {
                best = best.max(cur);
                cur = 0;
            } else if b & 0xC0 != 0x80 && b != b'\r' {
                cur += 1;
            }
        }
    }
    best.max(cur)
}

/// In-file find stops here (a one-letter query in a huge file).
pub const MAX_FIND_MATCHES: usize = 100_000;

impl Document {
    pub fn new_untitled(id: u64, n: usize, theme: Arc<Theme>) -> Self {
        let buffer = Buffer::from_str("");
        let syn = highlight::syntax_set().find_syntax_plain_text();
        Self::from_buffer(id, buffer, HighlightCache::new(syn, theme), format!("Untitled-{n}"))
    }

    pub fn open(id: u64, path: &Path, theme: Arc<Theme>, tab_size: usize) -> std::io::Result<Self> {
        let mut buffer = Buffer::load(path)?;
        buffer.tab_size = tab_size;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let first = buffer.line_text(0);
        let syn = highlight::detect_syntax(name.as_deref(), &first);
        let mut hl = HighlightCache::new(syn, theme);
        // Very large files: skip highlighting to stay snappy.
        if buffer.len_chars() > 30_000_000 {
            hl.enabled = false;
        }
        let mut doc = Self::from_buffer(id, buffer, hl, String::new());
        doc.disk_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok(doc)
    }

    fn from_buffer(id: u64, buffer: Buffer, highlight: HighlightCache, untitled_name: String) -> Self {
        Self {
            id,
            buffer,
            highlight,
            find: FindState::default(),
            untitled_name,
            scroll_to_cursor: false,
            reveal_line: None,
            disk_mtime: None,
            longest_line: (u64::MAX, 0),
            longest_dirty: None,
            longest_full_at: None,
            longest_full_cost: std::time::Duration::ZERO,
            diagnostics: vec![],
            completion: None,
            hover: None,
            signature: None,
            lsp_version: None,
            lsp_sent_at: None,
            lsp_triggers: vec![],
            lsp_active: false,
            git_base: None,
            git_hunks: vec![],
            git_hunks_version: None,
            conflicts: vec![],
            vim: Default::default(),
            breakpoints: Default::default(),
            breakpoints_gen: 0,
            debug_line: None,
            debug_paused: false,
            inlay_hints: vec![],
            inlay_version: None,
            inlay_request: None,
            inlay_retry: (0, None),
            semantic: vec![],
            semantic_version: None,
            semantic_request: None,
            semantic_retry: (0, None),
            debug_value: None,
            conflicts_version: None,
            review: None,
            show_git_diff: false,
            md_preview: false,
            blame: None,
            blame_rx: None,
            hunk_popup: None,
            git_diff: Default::default(),
            folds: Default::default(),
            folds_gen: 0,
            wrap_cols: 0,
            fold_cache: None,
            last_len_lines: 0,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.buffer.path()
    }

    pub fn title(&self) -> String {
        match self.buffer.path().and_then(|p| p.file_name()) {
            Some(n) => n.to_string_lossy().into_owned(),
            None => self.untitled_name.clone(),
        }
    }

    pub fn language(&self) -> &str {
        self.highlight.syntax_name()
    }

    pub fn comment_token(&self) -> &'static str {
        highlight::comment_token(self.language())
    }

    pub fn save(&mut self, trim_trailing: bool) -> std::io::Result<()> {
        if trim_trailing {
            self.trim_trailing_whitespace();
        }
        self.buffer.save()?;
        self.after_save();
        Ok(())
    }

    pub fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.buffer.save_as(&path)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let syn = highlight::detect_syntax(name.as_deref(), &self.buffer.line_text(0));
        self.highlight.set_syntax(syn);
        self.after_save();
        Ok(())
    }

    fn after_save(&mut self) {
        self.disk_mtime = self.path().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    }

    fn trim_trailing_whitespace(&mut self) {
        let text = self.buffer.text();
        let trimmed: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches([' ', '\t'])).collect();
        let new = trimmed.join("\n");
        if new != text {
            let sel = self.buffer.sel;
            self.buffer.set_text(&new);
            self.buffer.sel = sel;
            self.buffer.sel.clamp(self.buffer.len_chars());
        }
    }

    /// Has the file been modified on disk by another program?
    pub fn changed_on_disk(&self) -> bool {
        let Some(p) = self.path() else { return false };
        let now = std::fs::metadata(p).and_then(|m| m.modified()).ok();
        now.is_some() && self.disk_mtime.is_some() && now != self.disk_mtime
    }

    pub fn reload_from_disk(&mut self) -> std::io::Result<()> {
        let Some(p) = self.path().map(Path::to_path_buf) else { return Ok(()) };
        let text = String::from_utf8_lossy(&std::fs::read(&p)?).into_owned();
        let sel = self.buffer.sel;
        self.buffer.set_text(&text);
        self.buffer.mark_clean();
        self.buffer.sel = sel;
        self.buffer.sel.clamp(self.buffer.len_chars());
        self.after_save();
        Ok(())
    }

    /// Reload from disk, remembering the previous content for review.
    pub fn reload_with_review(&mut self) -> std::io::Result<()> {
        let base = match self.review.take() {
            Some(r) => r.base,
            None => self.buffer.text(),
        };
        self.reload_from_disk()?;
        if self.buffer.text() != base {
            self.review = Some(crate::review::Review::new(base));
        }
        Ok(())
    }

    /// Sync caches after edits. Call once per frame before rendering.
    pub fn sync(&mut self) {
        let n = self.buffer.len_lines();
        let changed = self.buffer.take_changed_lines();
        if let Some((line, last)) = changed {
            let delta_lines = n as isize - self.last_len_lines as isize;
            if self.last_len_lines == 0 {
                self.highlight.invalidate_from(line);
            } else {
                self.highlight.edited(line, last, delta_lines);
            }
            self.longest_dirty = Some(self.longest_dirty.map_or((line, last), |(a, b)| (a.min(line), b.max(last))));
            // Keep folds attached to their lines when lines are added/removed above.
            let delta = n as isize - self.last_len_lines as isize;
            if delta != 0 && !self.folds.is_empty() && self.last_len_lines > 0 {
                let removed_end = if delta < 0 { line as isize - delta } else { line as isize };
                self.folds = self
                    .folds
                    .iter()
                    .filter_map(|&h| {
                        if h <= line {
                            Some(h)
                        } else if (h as isize) <= removed_end {
                            None
                        } else {
                            Some((h as isize + delta) as usize)
                        }
                    })
                    .collect();
                self.folds_gen += 1;
            }
            // Inlay hints: drop the edited lines' hints, move the rest.
            if !self.inlay_hints.is_empty() {
                // Last edited line, in the old numbering.
                let old_last = (last as isize - delta).max(line as isize);
                self.inlay_hints.retain_mut(|h| {
                    if h.line < line {
                        true
                    } else if (h.line as isize) <= old_last {
                        false
                    } else {
                        h.line = (h.line as isize + delta) as usize;
                        true
                    }
                });
            }
            // Semantic colors: same as inlay hints.
            if !self.semantic.is_empty() {
                let old_last = (last as isize - delta).max(line as isize);
                self.semantic.retain_mut(|t| {
                    if t.line < line {
                        true
                    } else if (t.line as isize) <= old_last {
                        false
                    } else {
                        t.line = (t.line as isize + delta) as usize;
                        true
                    }
                });
            }
            // Breakpoints move with their lines too.
            if delta != 0 && !self.breakpoints.is_empty() && self.last_len_lines > 0 {
                let removed_end = if delta < 0 { line as isize - delta } else { line as isize };
                let moved: crate::debug::Breakpoints = self
                    .breakpoints
                    .iter()
                    .filter_map(|(&h, b)| {
                        let to = if h <= line {
                            h
                        } else if (h as isize) <= removed_end {
                            return None;
                        } else {
                            (h as isize + delta) as usize
                        };
                        Some((to, b.clone()))
                    })
                    .collect();
                if moved != self.breakpoints {
                    self.breakpoints = moved;
                    self.breakpoints_gen += 1;
                }
            }
        }
        self.last_len_lines = n;
        self.update_conflicts(changed);
        self.update_find();
    }

    /// Restore the committed lines of the git change covering `line`.
    pub fn revert_git_hunk(&mut self, line: usize) -> bool {
        let Some(base) = self.git_base.clone() else { return false };
        let hunks = crate::review::hunks(&base, &self.buffer.text());
        let Some(h) = hunks.iter().find(|h| h.new_start <= line && line < h.new_start + h.new_lines.len().max(1))
        else {
            return false;
        };
        let start = self.buffer.rope().line_to_char(h.new_start.min(self.buffer.len_lines()));
        let end_line = h.new_start + h.new_lines.len();
        let end = if end_line >= self.buffer.len_lines() {
            self.buffer.len_chars()
        } else {
            self.buffer.rope().line_to_char(end_line)
        };
        self.buffer.apply_edits(vec![(start, end, h.old_lines.concat())]);
        true
    }

    pub fn set_git_base(&mut self, base: Option<String>) {
        self.git_base = base.map(|b| b.replace("\r\n", "\n"));
        self.git_hunks_version = None;
        self.git_hunks.clear();
    }

    /// Recompute gutter change markers when the text changed (debounced by the caller's frame rate).
    pub fn update_git_hunks(&mut self) {
        let v = self.buffer.version();
        if self.git_hunks_version == Some(v) {
            return;
        }
        self.git_hunks_version = Some(v);
        self.git_hunks = match &self.git_base {
            Some(base) if self.buffer.len_lines() < 50_000 => crate::git::diff_hunks(base, &self.buffer.text()),
            _ => vec![],
        };
    }

    /// Semantic token colors on `line`.
    pub fn semantic_on(&self, line: usize) -> &[SemSpan] {
        let a = self.semantic.partition_point(|t| t.line < line);
        let b = self.semantic.partition_point(|t| t.line <= line);
        &self.semantic[a..b]
    }

    /// Inlay hints on `line`.
    pub fn hints_on(&self, line: usize) -> &[InlayView] {
        let a = self.inlay_hints.partition_point(|h| h.line < line);
        let b = self.inlay_hints.partition_point(|h| h.line <= line);
        &self.inlay_hints[a..b]
    }

    // ---------- debugging ----------

    pub fn toggle_breakpoint(&mut self, line: usize) {
        if self.breakpoints.remove(&line).is_none() {
            self.breakpoints.insert(line, Default::default());
        }
        self.breakpoints_gen += 1;
    }

    // ---------- merge conflicts ----------

    /// Rescan for conflict markers after an edit of lines `changed` (None =
    /// unknown). Typing in a file without conflicts costs only a look at the
    /// edited lines.
    fn update_conflicts(&mut self, changed: Option<(usize, usize)>) {
        let v = self.buffer.version();
        if self.conflicts_version == Some(v) {
            return;
        }
        let first_scan = self.conflicts_version.is_none();
        self.conflicts_version = Some(v);
        let rope = self.buffer.rope();
        if !first_scan && self.conflicts.is_empty() {
            let Some((a, b)) = changed else { return };
            let n = rope.len_lines();
            let marker =
                (a.min(n)..(b + 1).min(n)).any(|l| matches!(rope.line(l).chars().next(), Some('<' | '=' | '>' | '|')));
            if !marker {
                return;
            }
        }
        if rope.len_lines() > 200_000 {
            self.conflicts.clear();
            return;
        }
        // Cheap pre-check: only lines starting with a marker character matter.
        let lines: Vec<String> = rope
            .lines()
            .map(|l| match l.chars().next() {
                Some('<' | '=' | '>' | '|') => l.chars().take(64).collect(),
                _ => String::new(),
            })
            .collect();
        self.conflicts = crate::conflicts::find(lines.iter().map(String::as_str));
    }

    /// Replace conflict `i` with the chosen side(s) (one undo step).
    pub fn resolve_conflict(&mut self, i: usize, r: crate::conflicts::Resolution) {
        self.resolve_conflicts(&[i], r);
    }

    pub fn resolve_all_conflicts(&mut self, r: crate::conflicts::Resolution) {
        let all: Vec<usize> = (0..self.conflicts.len()).collect();
        self.resolve_conflicts(&all, r);
    }

    fn resolve_conflicts(&mut self, which: &[usize], r: crate::conflicts::Resolution) {
        let rope = self.buffer.rope();
        let edits: Vec<(usize, usize, String)> = which
            .iter()
            .filter_map(|&i| self.conflicts.get(i))
            .map(|c| {
                let lines: Vec<String> = (0..=c.end).map(|l| rope.line(l).to_string()).collect();
                let text = crate::conflicts::resolve(&lines, c, r);
                let end = if c.end + 1 < rope.len_lines() { rope.line_to_char(c.end + 1) } else { rope.len_chars() };
                (rope.line_to_char(c.start), end, text)
            })
            .collect();
        if !edits.is_empty() {
            self.buffer.apply_edits(edits);
            self.conflicts_version = None;
            self.update_conflicts(None);
        }
    }

    /// Move the caret to the next conflict after the caret (wrapping).
    pub fn next_conflict(&mut self) -> bool {
        let line = self.buffer.cursor_pos().line;
        let Some(c) = self.conflicts.iter().find(|c| c.start > line).or(self.conflicts.first()).copied() else {
            return false;
        };
        self.goto_line(c.start);
        true
    }

    // ---------- folding ----------

    pub fn fold_map(&mut self) -> std::sync::Arc<crate::folding::FoldMap> {
        // Syntax fold ranges change when the tree is reparsed.
        let ranges_id = if self.folds.is_empty() {
            0
        } else {
            self.syntax_folds().map_or(0, |r| std::sync::Arc::as_ptr(&r) as u64)
        };
        let key = (self.buffer.version(), self.folds_gen.wrapping_mul(31).wrapping_add(ranges_id));
        if let Some((v, g, m)) = &self.fold_cache {
            if (*v, *g) == key
                && m.wrap_cols
                    == self.wrap_cols.min(if self.buffer.len_lines() > crate::folding::MAX_WRAP_LINES {
                        0
                    } else {
                        usize::MAX
                    })
            {
                return m.clone();
            }
        }
        let sf = if self.folds.is_empty() { None } else { self.syntax_folds() };
        let m = std::sync::Arc::new(crate::folding::FoldMap::build(
            &self.buffer,
            &self.folds,
            self.wrap_cols,
            sf.as_deref(),
        ));
        self.fold_cache = Some((key.0, key.1, m.clone()));
        m
    }

    /// Fold ranges from the syntax tree, when it matches the current text.
    pub fn syntax_folds(&self) -> Option<std::sync::Arc<crate::folding::Ranges>> {
        self.highlight.syntax_folds(&self.buffer)
    }

    pub fn toggle_fold(&mut self, header: usize) {
        if !self.folds.remove(&header)
            && crate::folding::is_foldable(&self.buffer, header, self.syntax_folds().as_deref())
        {
            self.folds.insert(header);
            self.move_cursor_out_of_folds();
        }
        self.folds_gen += 1;
    }

    pub fn fold_at_cursor(&mut self) {
        let line = self.buffer.cursor_pos().line;
        let sf = self.syntax_folds();
        // Fold the innermost unfolded region around the caret.
        let mut l = line;
        loop {
            match crate::folding::enclosing_header(&self.buffer, l, sf.as_deref()) {
                Some(h) if self.folds.contains(&h) && h > 0 => l = h - 1,
                Some(h) if !self.folds.contains(&h) => {
                    self.folds.insert(h);
                    break;
                }
                _ => return,
            }
        }
        self.folds_gen += 1;
        self.move_cursor_out_of_folds();
    }

    pub fn unfold_at_cursor(&mut self) {
        let line = self.buffer.cursor_pos().line;
        let before = self.folds.len();
        self.folds.remove(&line);
        if let Some(h) = crate::folding::enclosing_header(&self.buffer, line, self.syntax_folds().as_deref()) {
            self.folds.remove(&h);
        }
        if self.folds.len() != before {
            self.folds_gen += 1;
        }
    }

    pub fn fold_all(&mut self) {
        let sf = self.syntax_folds();
        self.folds = match &sf {
            Some(r) => r.keys().copied().collect(),
            None => {
                (0..self.buffer.len_lines()).filter(|&l| crate::folding::is_foldable(&self.buffer, l, None)).collect()
            }
        };
        self.folds_gen += 1;
        self.move_cursor_out_of_folds();
    }

    pub fn unfold_all(&mut self) {
        self.folds.clear();
        self.folds_gen += 1;
    }

    /// Unfold any fold that hides the caret (after goto / search / typing).
    pub fn reveal_cursor_in_folds(&mut self) {
        if self.folds.is_empty() {
            return;
        }
        let line = self.buffer.cursor_pos().line;
        let map = self.fold_map();
        if !map.is_hidden(line) {
            return;
        }
        let buf = &self.buffer;
        let sf = self.highlight.syntax_folds(buf);
        self.folds
            .retain(|&h| !(h < line && crate::folding::fold_end(buf, h, sf.as_deref()).is_some_and(|e| e >= line)));
        self.folds_gen += 1;
    }

    fn move_cursor_out_of_folds(&mut self) {
        let line = self.buffer.cursor_pos().line;
        let map = self.fold_map();
        if map.is_hidden(line) {
            let header = map.line_at(map.row_of(line));
            let idx = self.buffer.pos_to_char(zenith_core::Position::new(header, usize::MAX));
            self.buffer.clear_extra();
            self.buffer.set_cursor(idx, false);
        }
    }

    pub fn longest_line_chars(&mut self) -> usize {
        let v = self.buffer.version();
        if self.longest_line.0 == v {
            return self.longest_line.1;
        }
        // Full scans are O(lines): do them at most twice a second; in between,
        // only grow the width from the edited lines (shrinking can wait).
        // A big file's scan takes a while: wait at least 20× as long between them.
        let gap = std::time::Duration::from_millis(500).max(self.longest_full_cost * 20);
        let full_due = self.longest_full_at.is_none_or(|t| t.elapsed() > gap);
        let n = match (self.longest_dirty.take(), full_due) {
            (Some((a, b)), false) if self.longest_line.0 != u64::MAX => {
                let b = b.min(self.buffer.len_lines().saturating_sub(1));
                (a..=b).map(|l| self.buffer.line_len(l)).max().unwrap_or(0).max(self.longest_line.1)
            }
            _ => {
                let t = std::time::Instant::now();
                self.longest_full_at = Some(t);
                let n = longest_line_scan(self.buffer.rope());
                self.longest_full_cost = t.elapsed();
                n
            }
        };
        self.longest_line = (v, n);
        n
    }

    pub fn update_find(&mut self) {
        let f = &mut self.find;
        if !f.open || f.query.is_empty() {
            f.matches.clear();
            f.computed_for = None;
            return;
        }
        let key = (f.query.clone(), self.buffer.version(), f.opts.case_sensitive, f.opts.whole_word);
        if f.computed_for.as_ref() == Some(&key) {
            return;
        }
        f.matches = zenith_core::find_limited(self.buffer.rope(), &f.query, f.opts, MAX_FIND_MATCHES);
        let head = self.buffer.sel.start();
        f.current = f.matches.iter().position(|m| m.start >= head).unwrap_or(0);
        f.computed_for = Some(key);
    }

    pub fn find_step(&mut self, forward: bool) {
        self.update_find();
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        let head = self.buffer.sel.head;
        let idx = if forward {
            self.find.matches.iter().position(|m| m.start >= head).unwrap_or(0)
        } else {
            let start = self.buffer.sel.start();
            self.find.matches.iter().rposition(|m| m.start < start).unwrap_or(n - 1)
        };
        self.select_match(idx);
    }

    fn select_match(&mut self, idx: usize) {
        if let Some(m) = self.find.matches.get(idx).cloned() {
            self.find.current = idx;
            self.buffer.clear_extra();
            self.buffer.sel = zenith_core::Selection::new(m.start, m.end);
            self.reveal_line = Some(self.buffer.char_to_pos(m.start).line);
        }
    }

    pub fn replace_current(&mut self) {
        self.update_find();
        let sel = self.buffer.sel.range();
        if self.find.matches.contains(&sel) {
            let rep = self.find.replacement.clone();
            self.buffer.insert(&rep);
            self.sync();
        }
        self.find_step(true);
    }

    pub fn replace_all(&mut self) -> usize {
        self.update_find();
        let matches = self.find.matches.clone();
        if matches.is_empty() {
            return 0;
        }
        // Build the new text in one pass and apply as a single undo step.
        let text = self.buffer.text();
        let mut out = String::with_capacity(text.len());
        let mut last_byte = 0;
        let char_to_byte = |c: usize| self.buffer.rope().char_to_byte(c);
        for m in &matches {
            let (s, e) = (char_to_byte(m.start), char_to_byte(m.end));
            out.push_str(&text[last_byte..s]);
            out.push_str(&self.find.replacement);
            last_byte = e;
        }
        out.push_str(&text[last_byte..]);
        let sel = self.buffer.sel;
        self.buffer.set_text(&out);
        self.buffer.sel = sel;
        self.buffer.sel.clamp(self.buffer.len_chars());
        self.sync();
        matches.len()
    }

    // ---------- LSP coordinate helpers ----------

    pub fn to_lsp(&self, idx: usize) -> LspPos {
        let pos = self.buffer.char_to_pos(idx);
        let line = self.buffer.line_text(pos.line);
        LspPos { line: pos.line as u32, character: zenith_lsp::char_col_to_utf16(&line, pos.col) }
    }

    pub fn lsp_to_char(&self, p: LspPos) -> usize {
        let line = (p.line as usize).min(self.buffer.len_lines().saturating_sub(1));
        let text = self.buffer.line_text(line);
        let col = zenith_lsp::utf16_to_char_col(&text, p.character);
        self.buffer.pos_to_char(zenith_core::Position::new(line, col))
    }

    pub fn range_from_lsp(&self, r: LspRange) -> (usize, usize) {
        (self.lsp_to_char(r.start), self.lsp_to_char(r.end))
    }

    /// Apply LSP text edits (positions relative to the current text) as one undo step.
    pub fn apply_lsp_edits(&mut self, edits: &[zenith_lsp::TextEdit]) {
        let converted: Vec<(usize, usize, String)> = edits
            .iter()
            .map(|e| {
                let (a, b) = self.range_from_lsp(e.range);
                (a, b, e.new_text.replace("\r\n", "\n"))
            })
            .collect();
        self.buffer.apply_edits(converted);
    }

    pub fn set_diagnostics(&mut self, diags: Vec<zenith_lsp::Diagnostic>) {
        let mut out: Vec<DocDiagnostic> = diags
            .into_iter()
            .map(|d| {
                let (start, end) = self.range_from_lsp(d.range);
                DocDiagnostic {
                    start,
                    end,
                    line: d.range.start.line as usize,
                    severity: d.severity,
                    message: d.message,
                    raw: d.raw,
                }
            })
            .collect();
        out.sort_by_key(|d| (d.start, d.severity));
        self.diagnostics = out;
    }

    pub fn diagnostic_counts(&self) -> (usize, usize) {
        let e = self.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        let w = self.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
        (e, w)
    }

    /// Jump to the next diagnostic after the caret (F8).
    pub fn next_diagnostic(&mut self) -> Option<String> {
        if self.diagnostics.is_empty() {
            return None;
        }
        let head = self.buffer.sel.head;
        self.buffer.clear_extra();
        let d = self.diagnostics.iter().find(|d| d.start > head).unwrap_or(&self.diagnostics[0]).clone();
        self.buffer.set_cursor(d.start.min(self.buffer.len_chars()), false);
        self.reveal_line = Some(d.line);
        Some(d.message)
    }

    /// Identifier around a char index.
    pub fn word_range_at(&self, idx: usize) -> Option<std::ops::Range<usize>> {
        let rope = self.buffer.rope();
        let len = rope.len_chars();
        let is_w = |i: usize| i < len && (rope.char(i).is_alphanumeric() || rope.char(i) == '_');
        if !is_w(idx) && !(idx > 0 && is_w(idx - 1)) {
            return None;
        }
        let mut s = idx;
        while s > 0 && is_w(s - 1) {
            s -= 1;
        }
        let mut e = idx;
        while is_w(e) {
            e += 1;
        }
        Some(s..e)
    }

    /// Start of the identifier ending at the caret.
    pub fn word_start(&self) -> usize {
        let rope = self.buffer.rope();
        let mut i = self.buffer.sel.head;
        while i > 0 {
            let c = rope.char(i - 1);
            if c.is_alphanumeric() || c == '_' {
                i -= 1;
            } else {
                break;
            }
        }
        i
    }

    /// Re-filter completion items against the typed prefix. Closes the popup
    /// when the caret leaves the word.
    pub fn refilter_completion(&mut self) {
        let head = self.buffer.sel.head;
        let Some(c) = &mut self.completion else { return };
        if head < c.anchor || !self.buffer.sel.is_empty() {
            self.completion = None;
            return;
        }
        let prefix: String = self.buffer.rope().slice(c.anchor..head).to_string();
        if prefix.contains(|ch: char| !(ch.is_alphanumeric() || ch == '_')) {
            self.completion = None;
            return;
        }
        let p = prefix.to_lowercase();
        let mut scored: Vec<(usize, (u8, &str, usize))> = c
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let f = it.filter_text.to_lowercase();
                let rank = if f == p {
                    0
                } else if f.starts_with(&p) {
                    1
                } else if p.is_empty()
                    || (f.starts_with(&p[..p.chars().next().map_or(0, |c| c.len_utf8())]) && is_subsequence(&p, &f))
                    || f.contains(&p)
                {
                    2
                } else {
                    return None;
                };
                Some((i, (rank, it.sort_text.as_str(), it.label.len())))
            })
            .collect();
        scored.sort_by(|a, b| a.1.cmp(&b.1));
        c.filtered = scored.into_iter().map(|(i, _)| i).take(200).collect();
        if c.selected >= c.filtered.len() {
            c.selected = 0;
        }
        if c.received && c.filtered.is_empty() {
            self.completion = None;
        }
    }

    /// Apply the selected completion item.
    pub fn accept_completion(&mut self) {
        let Some(c) = self.completion.take() else { return };
        let Some(&i) = c.filtered.get(c.selected) else { return };
        let item = &c.items[i];
        let head = self.buffer.sel.head;
        let (start, end) = match item.edit_range {
            Some(r) => {
                let (s, e) = self.range_from_lsp(r);
                (s.min(c.anchor), e.max(head))
            }
            None => (c.anchor, head),
        };
        let text = item.insert_text.clone();
        let select = item.select;
        self.buffer.seal_history();
        self.buffer.replace_range(start, end, &text);
        if let Some((a, b)) = select {
            self.buffer.sel = Selection::new(start + a, start + b);
        }
        self.buffer.seal_history();
        self.scroll_to_cursor = true;
    }

    pub fn goto_line(&mut self, line: usize) {
        let line = line.min(self.buffer.len_lines().saturating_sub(1));
        let idx = self.buffer.pos_to_char(zenith_core::Position::new(line, 0));
        self.buffer.clear_extra();
        self.buffer.set_cursor(idx, false);
        self.reveal_line = Some(line);
    }
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

#[cfg(test)]
mod perf {
    use super::*;
    use std::time::Instant;

    /// Tree-sitter keystroke cost on the largest file it handles.
    #[test]
    #[ignore]
    fn perf_tree_sitter_keystroke() {
        let line = "    let value = compute(alpha, beta, gamma) + other_function(delta); // comment\n";
        let mut text = String::new();
        for i in 0..1_900 {
            text.push_str(&format!("fn f{i}() {{\n"));
            text.push_str(&line.repeat(8));
            text.push_str("}\n");
        }
        let dir = std::env::temp_dir().join("zenith-perf-ts");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mid.rs");
        std::fs::write(&path, &text).unwrap();
        let mut doc = Document::open(1, &path, highlight::load_theme("OneHalfDark"), 4).unwrap();
        println!("lines: {}", doc.buffer.len_lines());
        doc.goto_line(9_000);
        doc.sync();
        let t = Instant::now();
        doc.highlight.ensure(&doc.buffer, 9_000, 9_060);
        println!("first parse + highlight: {:?}", t.elapsed());
        assert!(doc.highlight.tree_sitter_name().is_some());
        let n = 30;
        let t = Instant::now();
        for i in 0..n {
            doc.buffer.type_char(if i % 2 == 0 { 'x' } else { 'y' });
            doc.sync();
            doc.fold_map();
            doc.highlight.ensure(&doc.buffer, 9_000, 9_060);
        }
        let per = t.elapsed() / n;
        println!("tree-sitter per keystroke: {per:?}");
        assert!(per.as_millis() < 16, "keystroke too slow: {per:?}");
        // With folds in the file, fold ranges are needed after every edit.
        doc.fold_all();
        doc.unfold_at_cursor();
        let t = Instant::now();
        for i in 0..n {
            doc.buffer.type_char(if i % 2 == 0 { 'x' } else { 'y' });
            doc.sync();
            doc.highlight.ensure(&doc.buffer, 9_000, 9_060);
            doc.fold_map();
        }
        let per = t.elapsed() / n;
        println!("tree-sitter per keystroke with folds: {per:?}");
        assert!(per.as_millis() < 16, "keystroke too slow with folds: {per:?}");
    }

    /// Per-keystroke work on a large file. Run with:
    #[test]
    fn longest_line_scan_counts_chars() {
        let r = zenith_core::Rope::from_str("ab\r\nsalom dünyo\n\nxyz");
        assert_eq!(longest_line_scan(&r), "salom dünyo".chars().count());
        assert_eq!(longest_line_scan(&zenith_core::Rope::from_str("")), 0);
    }

    /// A 100 MB+ log: open, first frames, jump to the end, type, find, save.
    /// `cargo test --release -p zenith-app perf_huge_log -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_huge_log() {
        let dir = std::env::temp_dir().join("zenith-perf");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.log");
        let mut text = String::with_capacity(130 << 20);
        let mut i = 0u64;
        while text.len() < 120 << 20 {
            text.push_str(&format!(
                "2026-11-25T10:{:02}:{:02}.{:03}Z INFO  request id={i} path=/api/v1/items/{} status=200 took={}ms\n",
                (i / 60) % 60,
                i % 60,
                i % 1000,
                i * 7 % 10_000,
                i % 97
            ));
            i += 1;
        }
        std::fs::write(&path, &text).unwrap();
        println!("file: {} MB, {} lines", text.len() >> 20, i);
        drop(text);
        let t = Instant::now();
        let mut doc = Document::open(1, &path, highlight::load_theme("OneHalfDark"), 4).unwrap();
        println!("open: {:?}", t.elapsed());
        let t = Instant::now();
        doc.sync();
        println!("  sync: {:?}", t.elapsed());
        let t2 = Instant::now();
        doc.longest_line_chars();
        println!("  longest line: {:?}", t2.elapsed());
        let t2 = Instant::now();
        doc.fold_map();
        println!("  fold map: {:?}", t2.elapsed());
        let t2 = Instant::now();
        doc.highlight.ensure(&doc.buffer, 0, 60);
        println!("  highlight: {:?}", t2.elapsed());
        println!("first frame: {:?}", t.elapsed());
        let last = doc.buffer.len_lines() - 1;
        let t = Instant::now();
        doc.goto_line(last);
        doc.sync();
        doc.highlight.ensure(&doc.buffer, last - 60, last);
        println!("jump to the end: {:?}", t.elapsed());
        let t = Instant::now();
        let mut parts = [std::time::Duration::ZERO; 4];
        for c in "hello".chars() {
            let t = Instant::now();
            doc.buffer.type_char(c);
            parts[0] += t.elapsed();
            let t = Instant::now();
            doc.sync();
            parts[1] += t.elapsed();
            let t = Instant::now();
            doc.longest_line_chars();
            parts[2] += t.elapsed();
            let t = Instant::now();
            doc.fold_map();
            parts[3] += t.elapsed();
        }
        println!("  type / sync / longest / folds: {parts:?}");
        println!("5 keystrokes at the end: {:?}", t.elapsed());
        let t = Instant::now();
        let hits = zenith_core::find_all(doc.buffer.rope(), "id=1234567 ", Default::default());
        println!("find (1 hit): {} in {:?}", hits.len(), t.elapsed());
        let t = Instant::now();
        doc.buffer.save().unwrap();
        println!("save: {:?}", t.elapsed());
        let _ = std::fs::remove_file(&path);
    }

    /// `cargo test --release -p zenith-app perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn keystroke_cost_on_large_file() {
        let line = "    let value = compute(alpha, beta, gamma) + other_function(delta); // comment\n";
        let text: String = line.repeat(100_000);
        let dir = std::env::temp_dir().join("zenith-perf");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.rs");
        std::fs::write(&path, &text).unwrap();
        let t = Instant::now();
        let mut doc = Document::open(1, &path, highlight::load_theme("OneHalfDark"), 4).unwrap();
        doc.set_git_base(Some(text.clone()));
        println!("open: {:?}", t.elapsed());
        // Caret in the middle; warm caches like the first frames would.
        doc.goto_line(50_000);
        doc.sync();
        doc.longest_line_chars();
        doc.update_git_hunks();
        let t = Instant::now();
        doc.highlight.ensure(&doc.buffer, 50_000, 50_060);
        let jump = t.elapsed();
        assert!(doc.highlight.line(50_030).is_some_and(|l| l.len() > 1), "visible window not highlighted");
        println!("first frame after jumping to line 50k: {jump:?}");
        assert!(jump.as_millis() < 50, "jump too slow: {jump:?}");
        let t = Instant::now();
        while !doc.highlight.ensure(&doc.buffer, 0, 50_060) {}
        println!("highlight to line 50k: {:?}", t.elapsed());

        let n = 50;
        let mut parts = [std::time::Duration::ZERO; 5];
        for i in 0..n {
            let t = Instant::now();
            doc.buffer.type_char(if i % 2 == 0 { 'x' } else { 'y' });
            parts[0] += t.elapsed();
            let t = Instant::now();
            doc.sync();
            parts[1] += t.elapsed();
            let t = Instant::now();
            doc.longest_line_chars();
            parts[2] += t.elapsed();
            let t = Instant::now();
            doc.update_git_hunks();
            doc.fold_map();
            parts[3] += t.elapsed();
            let t = Instant::now();
            doc.highlight.ensure(&doc.buffer, 50_000, 50_060);
            parts[4] += t.elapsed();
        }
        println!("type/sync/longest/git+fold/highlight: {:?}", parts.map(|d| d / n));
        let t = Instant::now();
        for i in 0..n {
            doc.buffer.type_char(if i % 2 == 0 { 'x' } else { 'y' });
            doc.sync();
            doc.longest_line_chars();
            doc.update_git_hunks();
            doc.fold_map();
            doc.highlight.ensure(&doc.buffer, 50_000, 50_060);
        }
        let per = t.elapsed() / n;
        println!("per keystroke: {per:?}");
        assert!(per.as_millis() < 16, "keystroke too slow: {per:?}");
    }
}
