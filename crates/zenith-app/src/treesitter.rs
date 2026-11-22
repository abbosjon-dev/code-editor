//! Tree-sitter syntax trees: accurate highlighting and syntactic folding
//! for the languages with a bundled grammar (other files use syntect).
//!
//! The tree is updated incrementally: the edit is found by comparing the
//! previous and current text (common prefix / suffix) and passed to
//! `Tree::edit`, so reparsing after a keystroke only touches what changed.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use eframe::egui::Color32;
use streaming_iterator::StreamingIterator;
use syntect::highlighting::{FontStyle, Highlighter, Theme};
use syntect::parsing::{Scope, ScopeStack};
use tree_sitter::{InputEdit, Language, Parser, Point, Query, QueryCursor, Tree};
use zenith_core::Buffer;

use crate::highlight::Span;

/// Larger files keep the line-based highlighter.
pub const MAX_LINES: usize = 20_000;

pub struct TsLang {
    pub name: &'static str,
    language: Language,
    query: Query,
}

/// Grammar for a syntect syntax name, if one is bundled.
pub fn lang_for(syntax_name: &str) -> Option<&'static TsLang> {
    static LANGS: OnceLock<HashMap<&'static str, Option<&'static TsLang>>> = OnceLock::new();
    let langs = LANGS.get_or_init(|| {
        let defs: Vec<(&[&str], &'static str, Language, String)> = vec![
            (&["Rust"], "rust", tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY.into()),
            (&["Python"], "python", tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY.into()),
            (
                &["JavaScript", "JavaScript (Babel)", "JSX"],
                "javascript",
                tree_sitter_javascript::LANGUAGE.into(),
                format!("{}\n{}", tree_sitter_javascript::JSX_HIGHLIGHT_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY),
            ),
            (
                &["TypeScript"],
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                format!("{}\n{}", tree_sitter_typescript::HIGHLIGHTS_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY),
            ),
            (
                &["TypeScriptReact", "TSX"],
                "tsx",
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                format!(
                    "{}\n{}\n{}",
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    tree_sitter_javascript::HIGHLIGHT_QUERY
                ),
            ),
            (&["Go"], "go", tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY.into()),
            (&["C"], "c", tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY.into()),
            (
                &["C++"],
                "cpp",
                tree_sitter_cpp::LANGUAGE.into(),
                format!("{}\n{}", tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY),
            ),
            (&["JSON"], "json", tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY.into()),
            (
                &["Bourne Again Shell (bash)", "Shell-Unix-Generic", "Bash"],
                "bash",
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY.into(),
            ),
            (&["HTML"], "html", tree_sitter_html::LANGUAGE.into(), tree_sitter_html::HIGHLIGHTS_QUERY.into()),
            (&["CSS"], "css", tree_sitter_css::LANGUAGE.into(), tree_sitter_css::HIGHLIGHTS_QUERY.into()),
        ];
        let mut map = HashMap::new();
        for (names, name, language, source) in defs {
            let lang = match Query::new(&language, &source) {
                Ok(query) => Some(&*Box::leak(Box::new(TsLang { name, language, query }))),
                Err(e) => {
                    log::warn!("tree-sitter {name}: bad highlight query: {e}");
                    None
                }
            };
            for n in names {
                map.insert(*n, lang);
            }
        }
        map
    });
    langs.get(syntax_name).copied().flatten()
}

/// TextMate scope used to color a tree-sitter capture (longest prefix wins).
fn scope_for_capture(name: &str) -> Option<&'static str> {
    const MAP: &[(&str, &str)] = &[
        ("comment", "comment.line"),
        ("string.escape", "constant.character.escape"),
        ("string.special", "string.regexp"),
        ("string", "string.quoted"),
        ("escape", "constant.character.escape"),
        ("character", "constant.character"),
        ("number", "constant.numeric"),
        ("float", "constant.numeric"),
        ("boolean", "constant.language"),
        ("constant.builtin", "constant.language"),
        ("constant", "constant.other"),
        ("keyword.function", "storage.type.function"),
        ("keyword.operator", "keyword.operator"),
        ("keyword", "keyword.control"),
        ("conditional", "keyword.control"),
        ("repeat", "keyword.control"),
        ("include", "keyword.control.import"),
        ("storageclass", "storage.modifier"),
        ("operator", "keyword.operator"),
        ("function.macro", "entity.name.function.macro"),
        ("function.builtin", "support.function"),
        ("function.method", "entity.name.function"),
        ("function", "entity.name.function"),
        ("method", "entity.name.function"),
        ("constructor", "entity.name.type"),
        ("type.builtin", "storage.type"),
        ("type", "entity.name.type"),
        ("attribute", "entity.other.attribute-name"),
        ("tag", "entity.name.tag"),
        ("property", "variable.other.member"),
        ("field", "variable.other.member"),
        ("variable.parameter", "variable.parameter"),
        ("variable.builtin", "variable.language"),
        ("parameter", "variable.parameter"),
        ("label", "entity.name.label"),
        ("module", "entity.name.namespace"),
        ("namespace", "entity.name.namespace"),
        ("punctuation.special", "punctuation.section.embedded"),
        ("embedded", "punctuation.section.embedded"),
    ];
    MAP.iter()
        .filter(|(k, _)| name == *k || (name.starts_with(k) && name.as_bytes().get(k.len()) == Some(&b'.')))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, v)| *v)
}

#[derive(Clone, Copy)]
struct Style {
    color: Color32,
    bold: bool,
    italic: bool,
}

/// A document's syntax tree and its highlighted lines.
pub struct SyntaxTree {
    lang: &'static TsLang,
    parser: Parser,
    tree: Option<Tree>,
    /// Text the tree was built from.
    text: String,
    version: Option<u64>,
    /// Style per capture index for the current theme.
    styles: Vec<Option<Style>>,
    default_fg: Color32,
    theme_key: usize,
    /// Highlighted lines for `version`.
    lines: HashMap<usize, Vec<Span>>,
    /// Fold ranges (header line → last hidden line), computed on demand.
    /// After an edit the old ranges are shifted and reused while typing;
    /// they are recomputed once the text has been still for a moment.
    folds: std::cell::RefCell<Option<Arc<BTreeMap<usize, usize>>>>,
    folds_fresh: std::cell::Cell<bool>,
    edited_at: std::time::Instant,
}

/// How long typing must pause before fold ranges are recomputed.
const FOLD_REFRESH: std::time::Duration = std::time::Duration::from_millis(500);

impl SyntaxTree {
    pub fn new(syntax_name: &str) -> Option<Self> {
        let lang = lang_for(syntax_name)?;
        let mut parser = Parser::new();
        parser.set_language(&lang.language).ok()?;
        Some(Self {
            lang,
            parser,
            tree: None,
            text: String::new(),
            version: None,
            styles: vec![],
            default_fg: Color32::LIGHT_GRAY,
            theme_key: 0,
            lines: HashMap::new(),
            folds: Default::default(),
            folds_fresh: std::cell::Cell::new(false),
            edited_at: std::time::Instant::now(),
        })
    }

    pub fn lang_name(&self) -> &'static str {
        self.lang.name
    }

    pub fn is_current(&self, buf: &Buffer) -> bool {
        self.version == Some(buf.version())
    }

    pub fn set_theme(&mut self, theme: &Arc<Theme>) {
        let key = Arc::as_ptr(theme) as usize;
        if key == self.theme_key && !self.styles.is_empty() {
            return;
        }
        self.theme_key = key;
        let hl = Highlighter::new(theme);
        self.default_fg = theme.settings.foreground.map(to_color).unwrap_or(Color32::LIGHT_GRAY);
        self.styles = self
            .lang
            .query
            .capture_names()
            .iter()
            .map(|name| {
                let scope = scope_for_capture(name)?;
                let s = hl.style_for_stack(ScopeStack::from_vec(vec![Scope::new(scope).ok()?]).as_slice());
                Some(Style {
                    color: to_color(s.foreground),
                    bold: s.font_style.contains(FontStyle::BOLD),
                    italic: s.font_style.contains(FontStyle::ITALIC),
                })
            })
            .collect();
        self.lines.clear();
    }

    /// Bring the tree up to date with the buffer (incremental reparse).
    pub fn update(&mut self, buf: &Buffer) {
        if self.is_current(buf) {
            return;
        }
        let new = buf.text();
        if let Some(tree) = &mut self.tree {
            let edit = diff_edit(&self.text, &new);
            tree.edit(&edit);
            // Keep the fold ranges, moved by the edit, until they're recomputed.
            let shifted = self.folds.get_mut().take().map(|f| Arc::new(shift_ranges(&f, &edit)));
            *self.folds.get_mut() = shifted;
            self.folds_fresh.set(false);
            self.edited_at = std::time::Instant::now();
        }
        self.tree = self.parser.parse(&new, self.tree.as_ref());
        self.text = new;
        self.version = Some(buf.version());
        self.lines.clear();
    }

    /// Highlight lines `first..upto` (cached until the next edit).
    pub fn ensure(&mut self, first: usize, upto: usize) {
        let Some(tree) = &self.tree else { return };
        let missing: Vec<usize> = (first..upto).filter(|l| !self.lines.contains_key(l)).collect();
        let (Some(&a), Some(&b)) = (missing.first(), missing.last()) else { return };
        let line_starts = line_starts(&self.text);
        let n = line_starts.len();
        if a >= n {
            return;
        }
        let start_byte = line_starts[a];
        let end_byte = line_starts.get(b + 1).copied().unwrap_or(self.text.len());
        // Capture index per byte of the range; inner (smaller) nodes win.
        let mut caps: Vec<(usize, usize, usize, u32)> = vec![];
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(start_byte..end_byte);
        let mut it = cursor.captures(&self.lang.query, tree.root_node(), self.text.as_bytes());
        while let Some((m, i)) = it.next() {
            let c = m.captures[*i];
            if self.styles.get(c.index as usize).copied().flatten().is_none() {
                continue;
            }
            caps.push((c.node.start_byte(), c.node.end_byte(), m.pattern_index, c.index));
        }
        caps.sort_by(|x, y| (y.1 - y.0).cmp(&(x.1 - x.0)).then(y.2.cmp(&x.2)));
        let mut by_byte: Vec<Option<u32>> = vec![None; end_byte - start_byte];
        for (s, e, _, idx) in caps {
            let (s, e) = (s.max(start_byte) - start_byte, e.min(end_byte).saturating_sub(start_byte));
            for slot in by_byte.iter_mut().take(e).skip(s) {
                *slot = Some(idx);
            }
        }
        for line in a..=b.min(n - 1) {
            let ls = line_starts[line];
            let le = line_starts.get(line + 1).copied().unwrap_or(self.text.len());
            let text = &self.text[ls..le];
            let mut spans: Vec<Span> = vec![];
            for (char_i, (bi, ch)) in text.char_indices().enumerate() {
                if ch == '\n' {
                    break;
                }
                let style = by_byte
                    .get(ls + bi - start_byte)
                    .copied()
                    .flatten()
                    .and_then(|i| self.styles.get(i as usize).copied().flatten());
                let (color, bold, italic) =
                    style.map_or((self.default_fg, false, false), |s| (s.color, s.bold, s.italic));
                match spans.last_mut() {
                    Some(sp) if sp.color == color && sp.bold == bold && sp.italic == italic => sp.end = char_i + 1,
                    _ => spans.push(Span { start: char_i, end: char_i + 1, color, bold, italic }),
                }
            }
            self.lines.insert(line, spans);
        }
    }

    pub fn line(&self, i: usize) -> Option<&[Span]> {
        self.lines.get(&i).map(Vec::as_slice)
    }

    /// Fold ranges of the current tree.
    pub fn folds(&self) -> Arc<BTreeMap<usize, usize>> {
        let reuse = self.folds_fresh.get() || self.edited_at.elapsed() < FOLD_REFRESH;
        if let (true, Some(f)) = (reuse, self.folds.borrow().as_ref()) {
            return f.clone();
        }
        let f = Arc::new(self.compute_folds());
        *self.folds.borrow_mut() = Some(f.clone());
        self.folds_fresh.set(true);
        f
    }

    /// Fold ranges are being reused from before the last edit.
    pub fn folds_pending(&self) -> bool {
        !self.folds_fresh.get() && self.folds.borrow().is_some()
    }

    /// Multi-line syntax nodes become fold ranges. A closing line that only
    /// holds the end bracket stays visible.
    fn compute_folds(&self) -> BTreeMap<usize, usize> {
        let mut out = BTreeMap::new();
        let Some(tree) = &self.tree else { return out };
        let starts = line_starts(&self.text);
        let line_text = |l: usize| -> &str {
            let s = starts.get(l).copied().unwrap_or(self.text.len());
            let e = starts.get(l + 1).copied().unwrap_or(self.text.len());
            &self.text[s..e]
        };
        let mut cursor = tree.walk();
        // Ancestors of the cursor's node: (start row, end byte).
        let mut stack: Vec<(usize, usize)> = vec![];
        'walk: loop {
            let node = cursor.node();
            let s = node.start_position().row;
            let mut e = node.end_position().row;
            // A node ending at column 0 really ended on the line before.
            if node.end_position().column == 0 && e > s {
                e -= 1;
            }
            let multi_line = e > s;
            if multi_line && !stack.is_empty() && node.is_named() {
                // A body that starts below its parent and ends with it (Python's
                // `block`) belongs to the parent's fold, not to its first line.
                let trailing_body = stack.last().is_some_and(|&(ps, pe)| ps < s && pe == node.end_byte());
                let closing = line_text(e).trim_start();
                let end = if closing.starts_with(['}', ')', ']'])
                    || closing.starts_with("</")
                    || closing.starts_with("end")
                {
                    e - 1
                } else {
                    e
                };
                // The largest node starting on a line decides its fold.
                if end > s && !trailing_body {
                    let entry = out.entry(s).or_insert(end);
                    if end > *entry {
                        *entry = end;
                    }
                }
            }
            // Single-line nodes can't contain a fold: don't descend.
            if multi_line && cursor.goto_first_child() {
                stack.push((s, node.end_byte()));
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    continue 'walk;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
                stack.pop();
            }
        }
        out
    }
}

/// Move fold ranges by an edit: lines after it shift, ranges around it stretch.
fn shift_ranges(r: &BTreeMap<usize, usize>, e: &InputEdit) -> BTreeMap<usize, usize> {
    let (start, old_end, new_end) = (e.start_position.row, e.old_end_position.row, e.new_end_position.row);
    let delta = new_end as isize - old_end as isize;
    let map = |l: usize| if l > old_end { (l as isize + delta) as usize } else { l };
    r.iter()
        .filter(|(&h, _)| h <= start || h > old_end)
        .map(|(&h, &end)| {
            // An edit inside the range, or on its closing line, stretches it.
            let end =
                if h <= start && end + 1 >= start { (end as isize + delta).max(h as isize) as usize } else { map(end) };
            (map(h), end)
        })
        .filter(|(h, e)| e > h)
        .collect()
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
    v
}

fn point_at(text: &str, byte: usize) -> Point {
    let before = &text.as_bytes()[..byte];
    let row = before.iter().filter(|b| **b == b'\n').count();
    let col = byte - before.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    Point { row, column: col }
}

/// The single edit turning `old` into `new` (common prefix and suffix kept).
pub fn diff_edit(old: &str, new: &str) -> InputEdit {
    let (o, n) = (old.as_bytes(), new.as_bytes());
    let mut start = o.iter().zip(n).take_while(|(a, b)| a == b).count();
    while start > 0 && !(old.is_char_boundary(start) && new.is_char_boundary(start)) {
        start -= 1;
    }
    let max_suffix = o.len().min(n.len()) - start;
    let mut suffix = o.iter().rev().zip(n.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while suffix > 0 && !(old.is_char_boundary(o.len() - suffix) && new.is_char_boundary(n.len() - suffix)) {
        suffix -= 1;
    }
    let (old_end, new_end) = (o.len() - suffix, n.len() - suffix);
    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old, start),
        old_end_position: point_at(old, old_end),
        new_end_position: point_at(new, new_end),
    }
}

fn to_color(c: syntect::highlighting::Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_queries_compile() {
        for n in
            ["Rust", "Python", "JavaScript", "TypeScript", "TypeScriptReact", "Go", "C", "C++", "JSON", "HTML", "CSS"]
        {
            assert!(lang_for(n).is_some(), "{n}");
        }
        assert!(lang_for("Bourne Again Shell (bash)").is_some());
        assert!(lang_for("Markdown").is_none());
    }

    #[test]
    fn edit_detection() {
        let e = diff_edit("let x = 1;\nfoo();\n", "let xy = 1;\nfoo();\n");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (5, 5, 6));
        assert_eq!(e.start_position, Point { row: 0, column: 5 });
        let e = diff_edit("a\nb\nc", "a\nc");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (2, 4, 2));
        let e = diff_edit("ёж", "ёжик");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (4, 4, 8));
    }

    #[test]
    fn highlights_and_folds_rust() {
        let mut buf =
            Buffer::from_str("// hi\nfn main() {\n    let s = \"x\";\n    if true {\n        call(1);\n    }\n}\n");
        let mut t = SyntaxTree::new("Rust").unwrap();
        t.set_theme(&crate::highlight::load_theme("OneHalfDark"));
        t.update(&buf);
        t.ensure(0, 7);
        let color_at = |t: &SyntaxTree, line: usize, col: usize| {
            t.line(line).unwrap().iter().find(|s| s.start <= col && col < s.end).map(|s| s.color)
        };
        let comment = color_at(&t, 0, 0);
        let keyword = color_at(&t, 1, 0);
        let string = color_at(&t, 2, 13);
        let plain = color_at(&t, 2, 8);
        assert!(
            comment != keyword && keyword != string && string != plain,
            "{comment:?} {keyword:?} {string:?} {plain:?}"
        );
        // Function body and the `if` block fold; closing braces stay visible.
        assert_eq!(t.folds().get(&1), Some(&5));
        assert_eq!(t.folds().get(&3), Some(&4));
        // Incremental edit: a new line inside the block.
        buf.set_cursor(buf.pos_to_char(zenith_core::Position::new(4, 16)), false);
        buf.insert("\n        more();");
        t.update(&buf);
        assert_eq!(t.folds().get(&1), Some(&6));
        t.ensure(0, 8);
        assert_eq!(color_at(&t, 1, 0), keyword);
    }

    #[test]
    fn python_folds_blocks() {
        let buf = Buffer::from_str("def f(x):\n    if x:\n        return 1\n    return 2\n\nprint(f(3))\n");
        let mut t = SyntaxTree::new("Python").unwrap();
        t.update(&buf);
        assert_eq!(t.folds().get(&0), Some(&3));
        assert_eq!(t.folds().get(&1), Some(&2));
    }
}
