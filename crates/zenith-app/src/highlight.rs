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
    /// `states[i]` = parser state at the start of line i.
    states: Vec<(ParseState, HighlightState)>,
    /// Highlighted spans for lines [0, lines.len()).
    lines: Vec<Vec<Span>>,
    pub enabled: bool,
}

const MAX_HIGHLIGHT_LINE: usize = 4_000;
const FRAME_BUDGET: Duration = Duration::from_millis(6);

impl HighlightCache {
    pub fn new(syntax: &'static SyntaxReference, theme: Arc<Theme>) -> Self {
        let mut c = Self { syntax, theme, states: vec![], lines: vec![], enabled: true };
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

    fn reset(&mut self) {
        let hl = Highlighter::new(&self.theme);
        self.states = vec![(ParseState::new(self.syntax), HighlightState::new(&hl, ScopeStack::new()))];
        self.lines.clear();
    }

    /// Drop cached results from `line` on.
    pub fn invalidate_from(&mut self, line: usize) {
        self.lines.truncate(line);
        self.states.truncate(line + 1);
        if self.states.is_empty() {
            self.reset();
        }
    }

    /// Make sure lines up to `upto` (exclusive) are highlighted. Returns false
    /// if the time budget ran out (caller should repaint again soon).
    pub fn ensure(&mut self, buf: &Buffer, upto: usize) -> bool {
        if !self.enabled {
            return true;
        }
        let upto = upto.min(buf.len_lines());
        let start = Instant::now();
        let ss = syntax_set();
        let hl = Highlighter::new(&self.theme);
        let default_fg = self.theme.settings.foreground.map(to_color).unwrap_or(Color32::LIGHT_GRAY);
        while self.lines.len() < upto {
            let i = self.lines.len();
            let (mut ps, mut hs) = self.states[i].clone();
            let rope = buf.rope();
            let line: String = rope.line(i).chars().collect();
            let spans = if line.len() > MAX_HIGHLIGHT_LINE {
                vec![Span { start: 0, end: line.chars().count(), color: default_fg, bold: false, italic: false }]
            } else {
                match ps.parse_line(&line, ss) {
                    Ok(ops) => {
                        let mut out = Vec::new();
                        let mut char_pos = 0;
                        for (style, text, _) in RangedHighlightIterator::new(&mut hs, &ops, &line, &hl) {
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
            };
            self.lines.push(spans);
            self.states.push((ps, hs));
            if start.elapsed() > FRAME_BUDGET {
                return self.lines.len() >= upto;
            }
        }
        true
    }

    pub fn line(&self, i: usize) -> Option<&[Span]> {
        self.lines.get(i).map(|v| v.as_slice())
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
        assert!(c.ensure(&buf, 10));
        let first = c.line(0).unwrap();
        assert!(first.len() > 1, "keyword should be a separate span");
        buf.set_cursor(buf.len_chars(), false);
        buf.insert("// done");
        c.invalidate_from(buf.take_changed_line().unwrap());
        assert!(c.ensure(&buf, 10));
        assert!(c.line(3).is_some());
    }

    #[test]
    fn detects_by_shebang_and_extensions() {
        assert_eq!(detect_syntax(None, "#!/usr/bin/env python3").name, "Python");
        assert_eq!(detect_syntax(Some("a.ts"), "").name, "TypeScript");
        assert_eq!(detect_syntax(Some("Cargo.toml"), "").name, "TOML");
    }
}
