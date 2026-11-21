use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ropey::Rope;

use crate::history::{Edit, GroupKind, History};
use crate::{char_class, CharClass, Position, Selection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// An editable text document. Internally always `\n` line endings; the
/// original style is restored on save.
#[derive(Debug)]
pub struct Buffer {
    rope: Rope,
    pub sel: Selection,
    /// Secondary cursors/selections (multi-cursor). The primary is `sel`.
    pub extra: Vec<Selection>,
    /// Column the caret "wants" to be in during vertical movement.
    preferred_col: Option<usize>,
    history: History,
    path: Option<PathBuf>,
    version: u64,
    saved_version: u64,
    changed_from_line: Option<usize>,
    pub line_ending: LineEnding,
    pub tab_size: usize,
    pub use_spaces: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::from_str("")
    }
}

impl Buffer {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        let (normalized, le) = normalize(text);
        Self {
            rope: Rope::from_str(&normalized),
            sel: Selection::default(),
            extra: vec![],
            preferred_col: None,
            history: History::default(),
            path: None,
            version: 0,
            saved_version: 0,
            changed_from_line: Some(0),
            line_ending: le,
            tab_size: 4,
            use_spaces: true,
        }
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut b = Self::from_str(&text);
        b.use_spaces = !detect_tabs(&b.rope);
        b.path = Some(path.to_path_buf());
        Ok(b)
    }

    pub fn save(&mut self) -> io::Result<()> {
        let path = self.path.clone().ok_or_else(|| io::Error::other("buffer has no path"))?;
        self.save_as(path)
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let mut text = self.rope.to_string();
        if self.line_ending == LineEnding::CrLf {
            text = text.replace('\n', "\r\n");
        }
        // Write atomically so a crash never leaves a half-written file.
        let tmp =
            path.with_extension(format!("{}.zenith-tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
        fs::write(&tmp, text.as_bytes())?;
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            fs::write(path, text.as_bytes()).map_err(|_| e)?;
        }
        self.path = Some(path.to_path_buf());
        self.saved_version = self.version;
        self.history.seal();
        Ok(())
    }

    // ---------- accessors ----------

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, p: PathBuf) {
        self.path = Some(p);
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Start a new undo group with the next edit.
    pub fn seal_history(&mut self) {
        self.history.seal();
    }

    /// Mark the current content as matching what is on disk.
    pub fn mark_clean(&mut self) {
        self.saved_version = self.version;
        self.history.seal();
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Number of chars on a line excluding the trailing newline.
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return 0;
        }
        let l = self.rope.line(line);
        let n = l.len_chars();
        if n > 0 && l.char(n - 1) == '\n' {
            n - 1
        } else {
            n
        }
    }

    pub fn line_text(&self, line: usize) -> String {
        if line >= self.rope.len_lines() {
            return String::new();
        }
        let s = self.rope.line_to_char(line);
        self.rope.slice(s..s + self.line_len(line)).to_string()
    }

    pub fn char_to_pos(&self, idx: usize) -> Position {
        let idx = idx.min(self.rope.len_chars());
        let line = self.rope.char_to_line(idx);
        Position { line, col: idx - self.rope.line_to_char(line) }
    }

    pub fn pos_to_char(&self, pos: Position) -> usize {
        let line = pos.line.min(self.rope.len_lines().saturating_sub(1));
        self.rope.line_to_char(line) + pos.col.min(self.line_len(line))
    }

    pub fn cursor_pos(&self) -> Position {
        self.char_to_pos(self.sel.head)
    }

    pub fn selected_text(&self) -> String {
        self.rope.slice(self.sel.range()).to_string()
    }

    /// Returns the first line changed since the last call (for highlight caches).
    pub fn take_changed_line(&mut self) -> Option<usize> {
        self.changed_from_line.take()
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    // ---------- low-level editing ----------

    fn raw_replace(&mut self, start: usize, end: usize, text: &str) -> Edit {
        let removed = self.rope.slice(start..end).to_string();
        let line = self.rope.char_to_line(start);
        self.rope.remove(start..end);
        self.rope.insert(start, text);
        self.version += 1;
        self.changed_from_line = Some(self.changed_from_line.map_or(line, |l| l.min(line)));
        Edit { pos: start, removed, inserted: text.to_string() }
    }

    fn replace(&mut self, start: usize, end: usize, text: &str, kind: GroupKind) {
        let before = self.sel;
        let edit = self.raw_replace(start, end, text);
        let caret = start + text.chars().count();
        self.sel = Selection::cursor(caret);
        self.preferred_col = None;
        self.history.record(edit, before, self.sel, kind);
    }

    /// Replace an arbitrary char range (used by plugins / find-replace).
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        let len = self.len_chars();
        self.replace(start.min(len), end.min(len), text, GroupKind::Other);
    }

    /// Replace the whole document as one undoable step.
    pub fn set_text(&mut self, text: &str) {
        let (normalized, _) = normalize(text);
        let len = self.len_chars();
        self.replace(0, len, &normalized, GroupKind::Other);
    }

    // ---------- high-level editing ----------

    /// Type text at the caret, replacing any selection.
    pub fn insert(&mut self, text: &str) {
        let (text, _) = normalize(text);
        let r = self.sel.range();
        let kind = if r.is_empty() && text.chars().count() == 1 { GroupKind::Typing } else { GroupKind::Other };
        self.replace(r.start, r.end, &text, kind);
    }

    /// Insert a newline, keeping the current line's indentation (and adding one
    /// level after an opening bracket or colon).
    pub fn newline(&mut self) {
        let pos = self.char_to_pos(self.sel.start());
        let line = self.line_text(pos.line);
        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let before: String = line.chars().take(pos.col).collect();
        let after: String = line.chars().skip(pos.col).collect();
        let opener = before.trim_end().chars().last();
        let mut text = String::from("\n");
        text.push_str(&indent);
        if matches!(opener, Some('{') | Some('[') | Some('(') | Some(':')) {
            let unit = self.indent_unit();
            text.push_str(&unit);
            let closer = after.trim_start().chars().next();
            if matches!((opener, closer), (Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')'))) {
                // Put the closing bracket on its own line, caret in between.
                let r = self.sel.range();
                let mut full = text.clone();
                full.push('\n');
                full.push_str(&indent);
                self.replace(r.start, r.end, &full, GroupKind::Other);
                let caret = r.start + text.chars().count();
                self.sel = Selection::cursor(caret);
                return;
            }
        }
        let r = self.sel.range();
        self.replace(r.start, r.end, &text, GroupKind::Other);
    }

    pub fn indent_unit(&self) -> String {
        if self.use_spaces {
            " ".repeat(self.tab_size)
        } else {
            "\t".to_string()
        }
    }

    /// Tab key: insert indentation at the caret, or indent selected lines.
    pub fn tab(&mut self) {
        if self.sel.is_empty() {
            let col = self.cursor_pos().col;
            let unit = if self.use_spaces { " ".repeat(self.tab_size - col % self.tab_size) } else { "\t".into() };
            self.insert(&unit);
        } else {
            self.indent_lines(true);
        }
    }

    pub fn indent_lines(&mut self, indent: bool) {
        let first = self.char_to_pos(self.sel.start()).line;
        let mut last = self.char_to_pos(self.sel.end()).line;
        // A selection ending at column 0 doesn't include that line.
        if last > first && self.char_to_pos(self.sel.end()).col == 0 {
            last -= 1;
        }
        let before = self.sel;
        let (mut anchor, mut head) = (self.sel.anchor as isize, self.sel.head as isize);
        let unit = self.indent_unit();
        self.history.seal();
        let mut recorded = 0;
        for line in first..=last {
            let start = self.rope.line_to_char(line);
            let delta: isize;
            if indent {
                if self.line_len(line) == 0 {
                    continue;
                }
                let e = self.raw_replace(start, start, &unit);
                self.history.record(e, before, before, GroupKind::Other);
                delta = unit.chars().count() as isize;
                recorded += 1;
            } else {
                let text = self.line_text(line);
                let n = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(self.tab_size).take_while(|c| *c == ' ').count()
                };
                if n == 0 {
                    continue;
                }
                let e = self.raw_replace(start, start + n, "");
                self.history.record(e, before, before, GroupKind::Other);
                delta = -(n as isize);
                recorded += 1;
            }
            let s = start as isize;
            if anchor >= s {
                anchor = (anchor + delta).max(s);
            }
            if head >= s {
                head = (head + delta).max(s);
            }
        }
        self.history.merge_last(recorded);
        self.sel = Selection::new(anchor.max(0) as usize, head.max(0) as usize);
        self.history.set_last_sel_after(self.sel);
        self.history.seal();
    }

    pub fn backspace(&mut self) {
        if !self.sel.is_empty() {
            let r = self.sel.range();
            self.replace(r.start, r.end, "", GroupKind::Other);
            return;
        }
        let head = self.sel.head;
        if head == 0 {
            return;
        }
        let pos = self.cursor_pos();
        let line = self.line_text(pos.line);
        let prefix: String = line.chars().take(pos.col).collect();
        // Delete a whole indent unit when the caret is inside leading spaces.
        let mut start = head - 1;
        if self.use_spaces && pos.col > 0 && prefix.chars().all(|c| c == ' ') {
            let n = ((pos.col - 1) % self.tab_size) + 1;
            start = head - n.min(pos.col);
        }
        // Delete auto-closed pair together.
        let mut end = head;
        if start + 1 == head && head < self.len_chars() {
            let (a, b) = (self.rope.char(head - 1), self.rope.char(head));
            if matches!((a, b), ('(', ')') | ('[', ']') | ('{', '}') | ('"', '"') | ('\'', '\'') | ('`', '`')) {
                end = head + 1;
            }
        }
        self.replace(start, end, "", GroupKind::Deleting);
    }

    pub fn delete(&mut self) {
        if !self.sel.is_empty() {
            let r = self.sel.range();
            self.replace(r.start, r.end, "", GroupKind::Other);
            return;
        }
        let head = self.sel.head;
        if head < self.len_chars() {
            self.replace(head, head + 1, "", GroupKind::Deleting);
        }
    }

    pub fn delete_word_left(&mut self) {
        if self.sel.is_empty() {
            let to = self.word_left(self.sel.head);
            self.sel.anchor = to;
        }
        self.backspace_selection();
    }

    pub fn delete_word_right(&mut self) {
        if self.sel.is_empty() {
            let to = self.word_right(self.sel.head);
            self.sel.anchor = to;
        }
        self.backspace_selection();
    }

    fn backspace_selection(&mut self) {
        let r = self.sel.range();
        if !r.is_empty() {
            self.replace(r.start, r.end, "", GroupKind::Other);
        }
    }

    /// Insert a char, auto-closing brackets and quotes and typing "over" closers.
    pub fn type_char(&mut self, c: char) {
        let next = (self.sel.head < self.len_chars()).then(|| self.rope.char(self.sel.head));
        let closers = [')', ']', '}', '"', '\'', '`'];
        if self.sel.is_empty() && closers.contains(&c) && next == Some(c) {
            self.move_right(false);
            return;
        }
        let pair = match c {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            '"' | '\'' | '`' => Some(c),
            _ => None,
        };
        if let Some(close) = pair {
            let prev = (self.sel.start() > 0).then(|| self.rope.char(self.sel.start() - 1));
            let is_quote = matches!(c, '"' | '\'' | '`');
            let next_ok = next.is_none_or(|n| n.is_whitespace() || closers.contains(&n) || n == ',' || n == ';');
            let prev_ok = !is_quote || prev.is_none_or(|p| !p.is_alphanumeric());
            if !self.sel.is_empty() {
                // Wrap selection.
                let inner = self.selected_text();
                let r = self.sel.range();
                let text = format!("{c}{inner}{close}");
                self.replace(r.start, r.end, &text, GroupKind::Other);
                self.sel = Selection::new(r.start + 1, r.start + 1 + inner.chars().count());
                return;
            }
            if next_ok && prev_ok {
                let h = self.sel.head;
                self.replace(h, h, &format!("{c}{close}"), GroupKind::Typing);
                self.sel = Selection::cursor(h + 1);
                return;
            }
        }
        let mut s = [0u8; 4];
        self.insert(c.encode_utf8(&mut s));
    }

    pub fn duplicate_line(&mut self) {
        let pos = self.cursor_pos();
        let text = self.line_text(pos.line);
        let end = self.rope.line_to_char(pos.line) + self.line_len(pos.line);
        let before = self.sel;
        let e = self.raw_replace(end, end, &format!("\n{text}"));
        let caret = self.pos_to_char(Position::new(pos.line + 1, pos.col));
        self.sel = Selection::cursor(caret);
        self.history.record(e, before, self.sel, GroupKind::Other);
    }

    pub fn move_line(&mut self, up: bool) {
        let pos = self.cursor_pos();
        if (up && pos.line == 0) || (!up && pos.line + 1 >= self.len_lines()) {
            return;
        }
        let other = if up { pos.line - 1 } else { pos.line + 1 };
        let (a, b) = (pos.line.min(other), pos.line.max(other));
        let ta = self.line_text(a);
        let tb = self.line_text(b);
        let start = self.rope.line_to_char(a);
        let end = self.rope.line_to_char(b) + self.line_len(b);
        let before = self.sel;
        let e = self.raw_replace(start, end, &format!("{tb}\n{ta}"));
        self.sel = Selection::cursor(self.pos_to_char(Position::new(other, pos.col)));
        self.history.record(e, before, self.sel, GroupKind::Other);
    }

    /// Toggle a line comment (`//`, `#`, `--` ...) on all selected lines.
    pub fn toggle_comment(&mut self, prefix: &str) {
        let first = self.char_to_pos(self.sel.start()).line;
        let last = self.char_to_pos(self.sel.end()).line;
        let lines: Vec<String> = (first..=last).map(|l| self.line_text(l)).collect();
        let all_commented = lines.iter().filter(|l| !l.trim().is_empty()).all(|l| l.trim_start().starts_with(prefix));
        let min_indent = lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
            .min()
            .unwrap_or(0);
        let new: Vec<String> = lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    l.clone()
                } else if all_commented {
                    let idx = l.find(prefix).unwrap();
                    let mut rest = &l[idx + prefix.len()..];
                    if let Some(r) = rest.strip_prefix(' ') {
                        rest = r;
                    }
                    format!("{}{}", &l[..idx], rest)
                } else {
                    let (ind, rest): (String, String) =
                        (l.chars().take(min_indent).collect(), l.chars().skip(min_indent).collect());
                    format!("{ind}{prefix} {rest}")
                }
            })
            .collect();
        let start = self.rope.line_to_char(first);
        let end = self.rope.line_to_char(last) + self.line_len(last);
        let before = self.sel;
        let joined = new.join("\n");
        let e = self.raw_replace(start, end, &joined);
        let head_pos = self.char_to_pos(before.head);
        let delta =
            new[head_pos.line - first].chars().count() as isize - lines[head_pos.line - first].chars().count() as isize;
        let col = (head_pos.col as isize + delta).max(0) as usize;
        self.sel = Selection::cursor(self.pos_to_char(Position::new(head_pos.line, col)));
        self.history.record(e, before, self.sel, GroupKind::Other);
        self.history.seal();
    }

    pub fn undo(&mut self) -> bool {
        self.extra.clear();
        let Some(t) = self.history.pop_undo() else { return false };
        for e in t.edits.iter().rev() {
            let end = e.pos + e.inserted.chars().count();
            self.raw_replace(e.pos, end, &e.removed);
        }
        self.sel = t.sel_before;
        self.sel.clamp(self.len_chars());
        true
    }

    pub fn redo(&mut self) -> bool {
        self.extra.clear();
        let Some(t) = self.history.pop_redo() else { return false };
        for e in &t.edits {
            let end = e.pos + e.removed.chars().count();
            self.raw_replace(e.pos, end, &e.inserted);
        }
        self.sel = t.sel_after;
        self.sel.clamp(self.len_chars());
        true
    }

    // ---------- movement ----------

    pub fn set_cursor(&mut self, idx: usize, extend: bool) {
        self.sel.move_to(idx.min(self.len_chars()), extend);
        self.preferred_col = None;
        self.history.seal();
    }

    pub fn move_left(&mut self, extend: bool) {
        let to = if !extend && !self.sel.is_empty() { self.sel.start() } else { self.sel.head.saturating_sub(1) };
        self.set_cursor(to, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        let to =
            if !extend && !self.sel.is_empty() { self.sel.end() } else { (self.sel.head + 1).min(self.len_chars()) };
        self.set_cursor(to, extend);
    }

    pub fn move_vertical(&mut self, lines: isize, extend: bool) {
        let pos = self.cursor_pos();
        let col = self.preferred_col.unwrap_or(pos.col);
        let target = pos.line as isize + lines;
        let idx = if target < 0 {
            0
        } else if target as usize >= self.len_lines() {
            self.len_chars()
        } else {
            self.pos_to_char(Position::new(target as usize, col))
        };
        self.sel.move_to(idx, extend);
        self.preferred_col = Some(col);
        self.history.seal();
    }

    /// Smart home: toggles between first non-blank char and column 0.
    pub fn move_home(&mut self, extend: bool) {
        let pos = self.cursor_pos();
        let text = self.line_text(pos.line);
        let first = text.chars().take_while(|c| c.is_whitespace()).count();
        let col = if pos.col == first { 0 } else { first };
        self.set_cursor(self.pos_to_char(Position::new(pos.line, col)), extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        let pos = self.cursor_pos();
        self.set_cursor(self.pos_to_char(Position::new(pos.line, usize::MAX)), extend);
    }

    pub fn word_left(&self, from: usize) -> usize {
        let mut i = from;
        while i > 0 && char_class(self.rope.char(i - 1)) == CharClass::Whitespace {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let cls = char_class(self.rope.char(i - 1));
        while i > 0 && char_class(self.rope.char(i - 1)) == cls {
            i -= 1;
        }
        i
    }

    pub fn word_right(&self, from: usize) -> usize {
        let len = self.len_chars();
        let mut i = from;
        while i < len && char_class(self.rope.char(i)) == CharClass::Whitespace {
            i += 1;
        }
        if i >= len {
            return len;
        }
        let cls = char_class(self.rope.char(i));
        while i < len && char_class(self.rope.char(i)) == cls {
            i += 1;
        }
        i
    }

    pub fn move_word_left(&mut self, extend: bool) {
        self.set_cursor(self.word_left(self.sel.head), extend);
    }

    pub fn move_word_right(&mut self, extend: bool) {
        self.set_cursor(self.word_right(self.sel.head), extend);
    }

    // ---------- multi-cursor ----------

    pub fn has_multi(&self) -> bool {
        !self.extra.is_empty()
    }

    /// All selections, primary first.
    pub fn selections(&self) -> Vec<Selection> {
        let mut v = vec![self.sel];
        v.extend(self.extra.iter().copied());
        v
    }

    pub fn clear_extra(&mut self) {
        self.extra.clear();
    }

    /// Add a selection and make it primary (Alt+click, Ctrl+D).
    pub fn add_selection(&mut self, s: Selection) {
        let len = self.len_chars();
        let mut s = s;
        s.clamp(len);
        let old = self.sel;
        self.extra.push(old);
        self.sel = s;
        self.normalize_selections();
    }

    /// Replace all selections (first becomes primary).
    pub fn set_selections(&mut self, sels: Vec<Selection>) {
        let mut it = sels.into_iter();
        if let Some(first) = it.next() {
            self.sel = first;
            self.extra = it.collect();
            self.normalize_selections();
        }
    }

    /// Sort, clamp and merge overlapping selections, keeping the primary.
    fn normalize_selections(&mut self) {
        let len = self.len_chars();
        self.sel.clamp(len);
        for s in &mut self.extra {
            s.clamp(len);
        }
        let primary = self.sel;
        let mut all = self.selections();
        all.sort_by_key(|s| (s.start(), s.end()));
        let mut merged: Vec<Selection> = vec![];
        for s in all {
            if let Some(last) = merged.last_mut() {
                let overlap = s.start() < last.end()
                    || (s.start() == last.end() && (s.is_empty() || last.is_empty()) && s.start() == last.start());
                if overlap || (s.start() == last.start() && s.end() == last.end()) {
                    let (a, b) = (last.start().min(s.start()), last.end().max(s.end()));
                    let forward = last.head >= last.anchor;
                    *last = if forward { Selection::new(a, b) } else { Selection::new(b, a) };
                    continue;
                }
            }
            merged.push(s);
        }
        let pi = merged
            .iter()
            .position(|s| s.start() <= primary.head && primary.head <= s.end())
            .unwrap_or(merged.len() - 1);
        self.sel = merged.remove(pi);
        self.extra = merged;
    }

    /// Run an editing operation at every cursor as a single undo step.
    /// Cursors are processed from the end of the document backwards so earlier
    /// edits don't invalidate later positions.
    pub fn edit_each(&mut self, mut f: impl FnMut(&mut Buffer)) {
        if self.extra.is_empty() {
            f(self);
            return;
        }
        self.normalize_selections();
        let mut sels: Vec<(Selection, bool)> = vec![(self.sel, true)];
        sels.extend(self.extra.iter().map(|s| (*s, false)));
        sels.sort_by_key(|(s, _)| std::cmp::Reverse(s.start()));
        self.extra.clear();
        self.history.seal();
        let before = self.history.undo_len();
        let mut done: Vec<(Selection, bool)> = vec![];
        for (s, primary) in sels {
            let len_before = self.len_chars() as isize;
            self.sel = s;
            f(self);
            let delta = self.len_chars() as isize - len_before;
            for (d, _) in &mut done {
                d.anchor = (d.anchor as isize + delta).max(0) as usize;
                d.head = (d.head as isize + delta).max(0) as usize;
            }
            done.push((self.sel, primary));
            self.history.seal();
        }
        let added = self.history.undo_len().saturating_sub(before);
        self.history.merge_last(added);
        let primary = done.iter().find(|(_, p)| *p).map(|(s, _)| *s).unwrap_or(self.sel);
        self.sel = primary;
        self.extra = done.into_iter().filter(|(_, p)| !p).map(|(s, _)| s).collect();
        self.normalize_selections();
        self.history.set_last_sel_after(self.sel);
    }

    /// Run a movement at every cursor.
    pub fn move_each(&mut self, mut f: impl FnMut(&mut Buffer)) {
        if self.extra.is_empty() {
            f(self);
            return;
        }
        let primary = self.sel;
        let extras = std::mem::take(&mut self.extra);
        let mut moved = vec![];
        for s in extras {
            self.sel = s;
            self.preferred_col = None;
            f(self);
            moved.push(self.sel);
        }
        self.sel = primary;
        self.preferred_col = None;
        f(self);
        self.extra = moved;
        self.normalize_selections();
    }

    /// Paste: one line per cursor when the counts match, otherwise everything everywhere.
    pub fn paste_multi(&mut self, text: &str) {
        let n = self.extra.len() + 1;
        let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        if n > 1 && lines.len() == n {
            // edit_each visits cursors back-to-front: hand out lines from the end.
            let mut i = lines.len();
            self.edit_each(|b| {
                i -= 1;
                b.insert(lines[i]);
            });
        } else {
            self.edit_each(|b| b.insert(text));
        }
    }

    /// Text of all selections joined by newlines (document order).
    pub fn selected_texts(&self) -> Vec<String> {
        let mut sels = self.selections();
        sels.sort_by_key(|s| s.start());
        sels.iter().map(|s| self.rope.slice(s.range()).to_string()).collect()
    }

    /// Add a cursor on the line above/below the primary (Ctrl+Alt+Up/Down).
    pub fn add_cursor_vertical(&mut self, down: bool) {
        let all = self.selections();
        let edge = if down { all.iter().max_by_key(|s| s.head) } else { all.iter().min_by_key(|s| s.head) };
        let Some(edge) = edge.copied() else { return };
        let pos = self.char_to_pos(edge.head);
        let target = if down {
            pos.line + 1
        } else {
            match pos.line.checked_sub(1) {
                Some(l) => l,
                None => return,
            }
        };
        if target >= self.len_lines() {
            return;
        }
        let idx = self.pos_to_char(Position::new(target, pos.col));
        self.add_selection(Selection::cursor(idx));
    }

    /// Ctrl+D: select the word under the caret, or add the next occurrence of the selection.
    pub fn add_next_occurrence(&mut self) -> bool {
        if self.sel.is_empty() {
            if let Some(r) = self.word_under_cursor() {
                self.sel = Selection::new(r.start, r.end);
                return true;
            }
            return false;
        }
        let needle = self.selected_text();
        let hits =
            crate::find_all(&self.rope, &needle, crate::SearchOptions { case_sensitive: true, whole_word: false });
        let taken: Vec<(usize, usize)> = self.selections().iter().map(|s| (s.start(), s.end())).collect();
        let end = self.sel.end();
        let next = hits
            .iter()
            .filter(|r| !taken.contains(&(r.start, r.end)))
            .find(|r| r.start >= end)
            .or_else(|| hits.iter().find(|r| !taken.contains(&(r.start, r.end))));
        match next {
            Some(r) => {
                self.add_selection(Selection::new(r.start, r.end));
                true
            }
            None => false,
        }
    }

    /// Select every occurrence of the selection / word under the caret.
    pub fn select_all_occurrences(&mut self) {
        if self.sel.is_empty() {
            if let Some(r) = self.word_under_cursor() {
                self.sel = Selection::new(r.start, r.end);
            } else {
                return;
            }
        }
        let needle = self.selected_text();
        let hits =
            crate::find_all(&self.rope, &needle, crate::SearchOptions { case_sensitive: true, whole_word: false });
        let primary = self.sel;
        let mut sels: Vec<Selection> = vec![primary];
        sels.extend(hits.iter().filter(|r| r.start != primary.start()).map(|r| Selection::new(r.start, r.end)));
        self.set_selections(sels);
    }

    pub fn select_all(&mut self) {
        self.sel = Selection::new(0, self.len_chars());
    }

    /// Select the word under a char index (double click).
    pub fn select_word_at(&mut self, idx: usize) {
        let len = self.len_chars();
        if len == 0 {
            return;
        }
        let idx = idx.min(len - 1);
        let cls = char_class(self.rope.char(idx));
        let mut s = idx;
        while s > 0 && char_class(self.rope.char(s - 1)) == cls && self.rope.char(s - 1) != '\n' {
            s -= 1;
        }
        let mut e = idx;
        while e < len && char_class(self.rope.char(e)) == cls && self.rope.char(e) != '\n' {
            e += 1;
        }
        self.sel = Selection::new(s, e);
    }

    pub fn select_line(&mut self, line: usize) {
        let s = self.rope.line_to_char(line.min(self.len_lines().saturating_sub(1)));
        let e = if line + 1 < self.len_lines() { self.rope.line_to_char(line + 1) } else { self.len_chars() };
        self.sel = Selection::new(s, e);
    }

    /// Word under the caret, if any (used for "select next occurrence").
    pub fn word_under_cursor(&self) -> Option<std::ops::Range<usize>> {
        let len = self.len_chars();
        let h = self.sel.head;
        let is_w = |i: usize| i < len && char_class(self.rope.char(i)) == CharClass::Word;
        if !is_w(h) && !(h > 0 && is_w(h - 1)) {
            return None;
        }
        let mut s = h;
        while s > 0 && is_w(s - 1) {
            s -= 1;
        }
        let mut e = h;
        while is_w(e) {
            e += 1;
        }
        Some(s..e)
    }
}

fn normalize(text: &str) -> (String, LineEnding) {
    if text.contains("\r\n") {
        (text.replace("\r\n", "\n"), LineEnding::CrLf)
    } else {
        (text.to_string(), LineEnding::Lf)
    }
}

fn detect_tabs(rope: &Rope) -> bool {
    let mut tabs = 0;
    let mut spaces = 0;
    for line in rope.lines().take(500) {
        match line.chars().next() {
            Some('\t') => tabs += 1,
            Some(' ') => spaces += 1,
            _ => {}
        }
    }
    tabs > spaces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_undo() {
        let mut b = Buffer::from_str("hello");
        b.set_cursor(5, false);
        for c in " world".chars() {
            b.type_char(c);
        }
        assert_eq!(b.text(), "hello world");
        assert!(b.is_dirty());
        b.undo();
        assert_eq!(b.text(), "hello");
        b.redo();
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn auto_close_and_type_over() {
        let mut b = Buffer::from_str("");
        b.type_char('(');
        assert_eq!(b.text(), "()");
        b.type_char('x');
        b.type_char(')');
        assert_eq!(b.text(), "(x)");
        assert_eq!(b.sel.head, 3);
    }

    #[test]
    fn newline_indents_between_braces() {
        let mut b = Buffer::from_str("fn a() {}");
        b.set_cursor(8, false);
        b.newline();
        assert_eq!(b.text(), "fn a() {\n    \n}");
        assert_eq!(b.cursor_pos(), Position::new(1, 4));
    }

    #[test]
    fn backspace_pair_and_indent() {
        let mut b = Buffer::from_str("");
        b.type_char('[');
        b.backspace();
        assert_eq!(b.text(), "");
        let mut b = Buffer::from_str("        x");
        b.set_cursor(8, false);
        b.backspace();
        assert_eq!(b.text(), "    x");
    }

    #[test]
    fn crlf_roundtrip() {
        let b = Buffer::from_str("a\r\nb\r\n");
        assert_eq!(b.line_ending, LineEnding::CrLf);
        assert_eq!(b.text(), "a\nb\n");
    }

    #[test]
    fn indent_outdent() {
        let mut b = Buffer::from_str("a\nb\nc");
        b.select_all();
        b.indent_lines(true);
        assert_eq!(b.text(), "    a\n    b\n    c");
        b.indent_lines(false);
        assert_eq!(b.text(), "a\nb\nc");
        b.undo();
        assert_eq!(b.text(), "    a\n    b\n    c");
    }

    #[test]
    fn comment_toggle() {
        let mut b = Buffer::from_str("  let x = 1;\n  let y = 2;");
        b.select_all();
        b.toggle_comment("//");
        assert_eq!(b.text(), "  // let x = 1;\n  // let y = 2;");
        b.select_all();
        b.toggle_comment("//");
        assert_eq!(b.text(), "  let x = 1;\n  let y = 2;");
    }

    #[test]
    fn word_movement() {
        let b = Buffer::from_str("foo.bar  baz");
        assert_eq!(b.word_right(0), 3);
        assert_eq!(b.word_right(3), 4);
        assert_eq!(b.word_right(7), 12);
        assert_eq!(b.word_left(12), 9);
    }

    #[test]
    fn vertical_keeps_column() {
        let mut b = Buffer::from_str("abcdef\nab\nabcdef");
        b.set_cursor(5, false);
        b.move_vertical(1, false);
        assert_eq!(b.cursor_pos(), Position::new(1, 2));
        b.move_vertical(1, false);
        assert_eq!(b.cursor_pos(), Position::new(2, 5));
    }

    #[test]
    fn multi_cursor_typing_and_undo() {
        let mut b = Buffer::from_str("a\nb\nc");
        b.set_cursor(0, false);
        b.add_cursor_vertical(true);
        b.add_cursor_vertical(true);
        assert_eq!(b.selections().len(), 3);
        b.edit_each(|b| b.type_char('-'));
        assert_eq!(b.text(), "-a\n-b\n-c");
        b.edit_each(|b| b.backspace());
        assert_eq!(b.text(), "a\nb\nc");
        b.edit_each(|b| b.insert("xy"));
        assert_eq!(b.text(), "xya\nxyb\nxyc");
        b.undo();
        assert_eq!(b.text(), "a\nb\nc");
        assert!(!b.has_multi());
    }

    #[test]
    fn ctrl_d_and_select_all_occurrences() {
        let mut b = Buffer::from_str("foo bar foo baz foo");
        b.set_cursor(1, false);
        assert!(b.add_next_occurrence()); // selects word
        assert!(b.add_next_occurrence());
        assert_eq!(b.selections().len(), 2);
        b.edit_each(|b| b.insert("qux"));
        assert_eq!(b.text(), "qux bar qux baz foo");
        let mut b = Buffer::from_str("x = x + x");
        b.select_all_occurrences();
        assert_eq!(b.selections().len(), 3);
        b.edit_each(|b| b.insert("y"));
        assert_eq!(b.text(), "y = y + y");
    }

    #[test]
    fn multi_paste_distributes_lines() {
        let mut b = Buffer::from_str("a\nb");
        b.set_cursor(1, false);
        b.add_selection(Selection::cursor(3));
        b.paste_multi("1\n2");
        assert_eq!(b.text(), "a1\nb2");
    }

    #[test]
    fn move_each_and_merge() {
        let mut b = Buffer::from_str("abc\nabc");
        b.set_cursor(1, false);
        b.add_selection(Selection::cursor(5));
        b.move_each(|b| b.move_home(false));
        assert_eq!(b.selections().len(), 2);
        b.move_each(|b| b.set_cursor(0, false));
        assert_eq!(b.selections().len(), 1);
    }

    #[test]
    fn move_and_duplicate_line() {
        let mut b = Buffer::from_str("one\ntwo");
        b.move_line(false);
        assert_eq!(b.text(), "two\none");
        b.duplicate_line();
        assert_eq!(b.text(), "two\none\none");
    }
}
