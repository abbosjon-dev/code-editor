//! Incremental syntax highlighting on top of syntect.
//!
//! Parser state is checkpointed at the start of every line, so an edit only
//! re-highlights from the edited line downward, and only as far as the screen
//! needs. Work per frame is time-boxed so huge files never stall the UI.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::Color32;
use syntect::highlighting::{FontStyle, HighlightState, Highlighter, RangedHighlightIterator, Theme};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use zenith_core::Buffer;

pub fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

pub fn theme_names() -> Vec<&'static str> {
    two_face::theme::EmbeddedLazyThemeSet::theme_names().iter().map(|n| n.as_name()).collect()
}

pub fn load_theme(name: &str) -> Arc<Theme> {
    static THEMES: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();
    let set = THEMES.get_or_init(two_face::theme::extra);
    let wanted = two_face::theme::EmbeddedLazyThemeSet::theme_names()
        .iter()
        .find(|n| n.as_name().eq_ignore_ascii_case(name))
        .copied()
        .unwrap_or(two_face::theme::EmbeddedThemeName::OneHalfDark);
    Arc::new(set.get(wanted).clone())
}

/// Pick a syntax from a file name, falling back to the first line (shebang).
pub fn detect_syntax(file_name: Option<&str>, first_line: &str) -> &'static SyntaxReference {
    let ss = syntax_set();
    if let Some(name) = file_name {
        let ext = name.rsplit('.').next().unwrap_or("");
        if let Some(s) = ss.find_syntax_by_extension(name).or_else(|| ss.find_syntax_by_extension(ext)) {
            return s;
        }
    }
    ss.find_syntax_by_first_line(first_line).unwrap_or_else(|| ss.find_syntax_plain_text())
}

pub fn syntax_by_name(name: &str) -> Option<&'static SyntaxReference> {
    syntax_set().find_syntax_by_name(name)
}

/// Best-guess line comment token for a syntax.
pub fn comment_token(syntax_name: &str) -> &'static str {
    let n = syntax_name.to_ascii_lowercase();
    if [
        "python",
        "ruby",
        "shell",
        "bash",
        "toml",
        "yaml",
        "perl",
        "r",
        "makefile",
        "dockerfile",
        "elixir",
        "nix",
        "powershell",
        "cmake",
        "graphql",
    ]
    .iter()
    .any(|k| n.contains(k))
    {
        "#"
    } else if ["sql", "lua", "haskell", "elm"].iter().any(|k| n.contains(k)) {
        "--"
    } else if n.contains("lisp") || n.contains("clojure") || n.contains("ini") {
        ";"
    } else if n.contains("latex") || n.contains("tex") || n.contains("erlang") {
        "%"
    } else if n.contains("vim") {
        "\""
    } else {
        "//"
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Span {
    /// Char range within the line.
    pub start: usize,
    pub end: usize,
    pub color: Color32,
    pub bold: bool,
    pub italic: bool,
}

pub struct HighlightCache {
    syntax: &'static SyntaxReference,
    theme: Arc<Theme>,
    /// `states[i]` = parser state at the start of line i (len = lines.len() + 1).
    states: Vec<(ParseState, HighlightState)>,
    /// Highlighted spans for lines [0, lines.len()).
    lines: Vec<Vec<Span>>,
    /// Lines from here on may be stale after an edit (they were shifted to
    /// their new positions); re-highlighting stops as soon as the parser state
    /// matches the cached one again.
    dirty_from: Option<usize>,
    /// Don't stop before this line (end of the edited region).
    dirty_until: usize,
    /// Approximate highlighting of a far-away window (first line, spans),
    /// used while the exact pass hasn't reached it yet.
    approx: Option<(usize, u64, Vec<Vec<Span>>)>,
    pub enabled: bool,
    /// Tree-sitter tree for languages with a bundled grammar.
    ts: Option<crate::treesitter::SyntaxTree>,
    /// The tree drove the last `ensure` (so `line` reads from it).
    ts_active: bool,
}

/// Use tree-sitter where available (Settings → Editor).
pub static TREE_SITTER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

const MAX_HIGHLIGHT_LINE: usize = 4_000;
const FRAME_BUDGET: Duration = Duration::from_millis(6);
/// How far ahead of the exact pass a window must be to use approximation.
const APPROX_DISTANCE: usize = 2_000;
/// Lines of context parsed before an approximate window.
const APPROX_CONTEXT: usize = 150;

impl HighlightCache {
    pub fn new(syntax: &'static SyntaxReference, theme: Arc<Theme>) -> Self {
        let mut c = Self {
            syntax,
            theme,
            states: vec![],
            lines: vec![],
            dirty_from: None,
            dirty_until: 0,
            approx: None,
            enabled: true,
            ts: None,
            ts_active: false,
        };
        c.reset();
        c
    }

    pub fn syntax_name(&self) -> &str {
        &self.syntax.name
    }

    pub fn set_syntax(&mut self, syntax: &'static SyntaxReference) {
        self.syntax = syntax;
        self.reset();
    }

    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme;
        self.reset();
    }

    /// Name of the tree-sitter grammar in use, if any.
    pub fn tree_sitter_name(&self) -> Option<&'static str> {
        self.ts.as_ref().filter(|_| self.ts_active).map(|t| t.lang_name())
    }

    /// Shifted fold ranges are in use; they'll be recomputed after a pause.
    pub fn folds_pending(&self) -> bool {
        self.ts.as_ref().is_some_and(|t| self.ts_active && t.folds_pending())
    }

    /// Fold ranges from the syntax tree, if it is current for `buf`.
    pub fn syntax_folds(&self, buf: &Buffer) -> Option<Arc<crate::folding::Ranges>> {
        self.ts.as_ref().filter(|t| self.ts_active && t.is_current(buf)).map(|t| t.folds())
    }

    fn initial_state(&self) -> (ParseState, HighlightState) {
        let hl = Highlighter::new(&self.theme);
        (ParseState::new(self.syntax), HighlightState::new(&hl, ScopeStack::new()))
    }

    fn reset(&mut self) {
        self.ts = if TREE_SITTER.load(std::sync::atomic::Ordering::Relaxed) {
            crate::treesitter::SyntaxTree::new(&self.syntax.name)
        } else {
            None
        };
        self.ts_active = false;
        self.states = vec![self.initial_state()];
        self.lines.clear();
        self.dirty_from = None;
        self.approx = None;
    }

    /// Drop cached results from `line` on.
    pub fn invalidate_from(&mut self, line: usize) {
        self.lines.truncate(line);
        self.states.truncate(line + 1);
        if self.states.is_empty() {
            self.reset();
        }
        self.dirty_from = None;
        self.approx = None;
    }

    /// Lines `first..=last` (new coordinates) were edited and the document
    /// gained `delta` lines. Cached lines after the edit are shifted, not dropped.
    pub fn edited(&mut self, first: usize, last: usize, delta: isize) {
        self.approx = None;
        if first >= self.lines.len() {
            return;
        }
        let at = (first + 1).min(self.lines.len());
        if delta > 0 {
            let filler = self.states[at].clone();
            self.lines.splice(at..at, std::iter::repeat_n(Vec::new(), delta as usize));
            self.states.splice(at..at, std::iter::repeat_n(filler, delta as usize));
        } else if delta < 0 {
            let n = ((-delta) as usize).min(self.lines.len() - at);
            self.lines.drain(at..at + n);
            self.states.drain(at..at + n);
        }
        self.dirty_from = Some(self.dirty_from.map_or(first, |d| d.min(first)));
        self.dirty_until = self.dirty_until.max(last + 1);
    }

    fn highlight_line(
        &self,
        line: &str,
        state: &mut (ParseState, HighlightState),
        hl: &Highlighter,
        default_fg: Color32,
    ) -> Vec<Span> {
        if line.len() > MAX_HIGHLIGHT_LINE {
            return vec![Span { start: 0, end: line.chars().count(), color: default_fg, bold: false, italic: false }];
        }
        let (ps, hs) = state;
        match ps.parse_line(line, syntax_set()) {
            Ok(ops) => {
                let mut out = Vec::new();
                let mut char_pos = 0;
                for (style, text, _) in RangedHighlightIterator::new(hs, &ops, line, hl) {
                    let n = text.chars().count();
                    out.push(Span {
                        start: char_pos,
                        end: char_pos + n,
                        color: to_color(style.foreground),
                        bold: style.font_style.contains(FontStyle::BOLD),
                        italic: style.font_style.contains(FontStyle::ITALIC),
                    });
                    char_pos += n;
                }
                out
            }
            Err(_) => vec![],
        }
    }

    /// Make sure lines `first..upto` are highlighted (exactly or, far ahead of
    /// the exact pass, approximately). Returns false if the time budget ran out
    /// (caller should repaint again soon).
    pub fn ensure(&mut self, buf: &Buffer, first: usize, upto: usize) -> bool {
        if !self.enabled {
            return true;
        }
        let n = buf.len_lines();
        // Tree-sitter: exact, incremental, and cheap for the visible lines.
        if let Some(ts) = &mut self.ts {
            if n <= crate::treesitter::MAX_LINES {
                ts.set_theme(&self.theme);
                ts.update(buf);
                ts.ensure(first, upto.min(n));
                self.ts_active = true;
                return true;
            }
        }
        self.ts_active = false;
        let upto = upto.min(n);
        let start = Instant::now();
        let theme = self.theme.clone();
        let hl = Highlighter::new(&theme);
        let default_fg = theme.settings.foreground.map(to_color).unwrap_or(Color32::LIGHT_GRAY);
        let rope = buf.rope();
        let text_of = |i: usize| -> String { rope.line(i).chars().collect() };

        // 1. Repair the edited region until the parser state converges.
        if let Some(mut i) = self.dirty_from {
            loop {
                if i >= self.lines.len() || i >= n {
                    self.lines.truncate(i.min(n));
                    self.states.truncate(i.min(n) + 1);
                    self.dirty_from = None;
                    break;
                }
                let mut state = self.states[i].clone();
                self.lines[i] = self.highlight_line(&text_of(i), &mut state, &hl, default_fg);
                let converged = i + 1 >= self.dirty_until && self.states.get(i + 1) == Some(&state);
                self.states[i + 1] = state;
                i += 1;
                if converged {
                    self.dirty_from = None;
                    self.dirty_until = 0;
                    break;
                }
                if start.elapsed() > FRAME_BUDGET {
                    self.dirty_from = Some(i);
                    return false;
                }
            }
        }

        // 2. Far-away window: highlight approximately from a nearby fresh state.
        if first > self.lines.len() + APPROX_DISTANCE {
            let from = first.saturating_sub(APPROX_CONTEXT);
            let key = buf.version();
            let fresh = !matches!(&self.approx, Some((f, v, l)) if *f == from && *v == key && from + l.len() >= upto);
            if fresh {
                let mut state = self.initial_state();
                let mut out = Vec::with_capacity(upto - from);
                for i in from..upto {
                    out.push(self.highlight_line(&text_of(i), &mut state, &hl, default_fg));
                }
                self.approx = Some((from, key, out));
            }
            // Keep making exact progress in the background.
            while start.elapsed() < FRAME_BUDGET && self.lines.len() < n {
                self.extend_one(buf, &hl, default_fg);
            }
            return self.lines.len() >= upto;
        }

        // 3. Extend the exact pass.
        while self.lines.len() < upto {
            self.extend_one(buf, &hl, default_fg);
            if start.elapsed() > FRAME_BUDGET {
                return self.lines.len() >= upto;
            }
        }
        if self.lines.len() >= upto {
            self.approx = None;
        }
        true
    }

    fn extend_one(&mut self, buf: &Buffer, hl: &Highlighter, default_fg: Color32) {
        let i = self.lines.len();
        let mut state = self.states[i].clone();
        let text: String = buf.rope().line(i).chars().collect();
        let spans = self.highlight_line(&text, &mut state, hl, default_fg);
        self.lines.push(spans);
        self.states.push(state);
    }

    pub fn line(&self, i: usize) -> Option<&[Span]> {
        if self.ts_active {
            return self.ts.as_ref().and_then(|t| t.line(i));
        }
        if let Some(l) = self.lines.get(i) {
            return Some(l.as_slice());
        }
        match &self.approx {
            Some((from, _, lines)) if i >= *from => lines.get(i - from).map(|v| v.as_slice()),
            _ => None,
        }
    }
}

fn to_color(c: syntect::highlighting::Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_incrementally() {
        let mut buf = Buffer::from_str("fn main() {\n    let x = 1;\n}\n");
        let syn = detect_syntax(Some("main.rs"), "");
        assert_eq!(syn.name, "Rust");
        let mut c = HighlightCache::new(syn, load_theme("OneHalfDark"));
        assert!(c.ensure(&buf, 0, 10));
        let first = c.line(0).unwrap();
        assert!(first.len() > 1, "keyword should be a separate span");
        buf.set_cursor(buf.len_chars(), false);
        buf.insert("// done");
        c.invalidate_from(buf.take_changed_line().unwrap());
        assert!(c.ensure(&buf, 0, 10));
        assert!(c.line(3).is_some());
    }

    /// Incremental repair must give the same result as highlighting from scratch.
    #[test]
    fn incremental_matches_full() {
        let src = "fn a() {\n    let s = \"x\";\n}\n/* block\ncomment */\nfn b() {}\n".repeat(30);
        let syn = detect_syntax(Some("x.rs"), "");
        let theme = load_theme("OneHalfDark");
        let mut buf = Buffer::from_str(&src);
        let mut c = HighlightCache::new(syn, theme.clone());
        buf.take_changed_lines();
        while !c.ensure(&buf, 0, buf.len_lines()) {}
        let check = |buf: &mut Buffer, c: &mut HighlightCache, edit: &dyn Fn(&mut Buffer)| {
            let before = buf.len_lines();
            edit(buf);
            let (a, b) = buf.take_changed_lines().unwrap();
            c.edited(a, b, buf.len_lines() as isize - before as isize);
            while !c.ensure(buf, 0, buf.len_lines()) {}
            let mut fresh = HighlightCache::new(syn, theme.clone());
            while !fresh.ensure(buf, 0, buf.len_lines()) {}
            for i in 0..buf.len_lines() {
                let x: Vec<_> = c.line(i).unwrap().iter().map(|s| (s.start, s.end, s.color)).collect();
                let y: Vec<_> = fresh.line(i).unwrap().iter().map(|s| (s.start, s.end, s.color)).collect();
                assert_eq!(x, y, "line {i} differs");
            }
        };
        // Typing on one line.
        check(&mut buf, &mut c, &|b| {
            b.set_cursor(b.pos_to_char(zenith_core::Position::new(10, 4)), false);
            b.insert("zz");
        });
        // Opening a block comment changes everything after it.
        check(&mut buf, &mut c, &|b| {
            b.set_cursor(b.pos_to_char(zenith_core::Position::new(20, 0)), false);
            b.insert("/* ");
        });
        // ...and closing it again.
        check(&mut buf, &mut c, &|b| {
            b.set_cursor(b.pos_to_char(zenith_core::Position::new(20, 0)), false);
            b.delete();
            b.delete();
            b.delete();
        });
        // Inserting and deleting lines.
        check(&mut buf, &mut c, &|b| {
            b.set_cursor(b.pos_to_char(zenith_core::Position::new(5, 0)), false);
            b.insert("let q = 1;\nlet r = \"s\";\n");
        });
        check(&mut buf, &mut c, &|b| {
            let s = b.pos_to_char(zenith_core::Position::new(3, 0));
            let e = b.pos_to_char(zenith_core::Position::new(9, 0));
            b.replace_range(s, e, "");
        });
    }

    #[test]
    fn far_window_is_approximated() {
        let src = "let x = 1; // c\n".repeat(20_000);
        let buf = Buffer::from_str(&src);
        let mut c = HighlightCache::new(detect_syntax(Some("x.rs"), ""), load_theme("OneHalfDark"));
        c.ensure(&buf, 15_000, 15_050);
        assert!(c.line(15_010).is_some_and(|l| l.len() > 1));
    }

    #[test]
    fn detects_by_shebang_and_extensions() {
        assert_eq!(detect_syntax(None, "#!/usr/bin/env python3").name, "Python");
        assert_eq!(detect_syntax(Some("a.ts"), "").name, "TypeScript");
        assert_eq!(detect_syntax(Some("Cargo.toml"), "").name, "TOML");
    }
}
