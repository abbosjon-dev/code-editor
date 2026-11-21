//! An open document = buffer + highlighting + per-view state.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use syntect::highlighting::Theme;
use zenith_core::{find_all, Buffer, SearchOptions, Selection};

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
    // --- language server state ---
    pub diagnostics: Vec<DocDiagnostic>,
    pub completion: Option<Completion>,
    pub hover: Option<Hover>,
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
    /// Pending review of a change made outside the editor (e.g. by an AI agent).
    pub review: Option<crate::review::Review>,
}

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
            diagnostics: vec![],
            completion: None,
            hover: None,
            lsp_version: None,
            lsp_sent_at: None,
            lsp_triggers: vec![],
            lsp_active: false,
            git_base: None,
            git_hunks: vec![],
            git_hunks_version: None,
            review: None,
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
            self.review = Some(crate::review::Review { base, show: false });
        }
        Ok(())
    }

    /// Sync caches after edits. Call once per frame before rendering.
    pub fn sync(&mut self) {
        if let Some(line) = self.buffer.take_changed_line() {
            self.highlight.invalidate_from(line);
        }
        self.update_find();
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

    pub fn longest_line_chars(&mut self) -> usize {
        let v = self.buffer.version();
        if self.longest_line.0 != v {
            let n = (0..self.buffer.len_lines()).map(|l| self.buffer.line_len(l)).max().unwrap_or(0);
            self.longest_line = (v, n);
        }
        self.longest_line.1
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
        f.matches = find_all(self.buffer.rope(), &f.query, f.opts);
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
