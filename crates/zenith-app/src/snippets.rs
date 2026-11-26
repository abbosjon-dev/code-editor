//! Snippets: type a prefix and press Tab to expand it; Tab / Shift+Tab then
//! move between its fields (`$1`, `${2:default}`, … and `$0` last).
//! Built-in ones for common languages, plus your own in
//! `<config>/snippets/<language>.toml` (or `all.toml` for every language):
//!
//! ```toml
//! [[snippet]]
//! prefix = "todo"
//! description = "TODO comment"
//! body = "// TODO(${1:me}): $0"
//! ```

use std::path::Path;
use std::sync::RwLock;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Snippet {
    pub prefix: String,
    #[serde(default)]
    pub description: String,
    pub body: String,
    /// Lowercase language names it applies to (empty: all).
    #[serde(skip)]
    pub langs: Vec<String>,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    snippet: Vec<Snippet>,
}

const RUST: &[&str] = &["rust"];
const PY: &[&str] = &["python"];
const JS: &[&str] = &["javascript", "typescript", "typescriptreact", "javascript (babel)", "jsx", "tsx"];
const GO: &[&str] = &["go"];

/// (languages, prefix, description, body); `\t` is one indent level.
const BUILTIN: &[(&[&str], &str, &str, &str)] = &[
    (RUST, "fn", "function", "fn ${1:name}(${2}) {\n\t$0\n}"),
    (RUST, "pfn", "public function", "pub fn ${1:name}(${2}) -> ${3:()} {\n\t$0\n}"),
    (RUST, "main", "main function", "fn main() {\n\t$0\n}"),
    (RUST, "test", "test function", "#[test]\nfn ${1:it_works}() {\n\t$0\n}"),
    (
        RUST,
        "tmod",
        "test module",
        "#[cfg(test)]\nmod tests {\n\tuse super::*;\n\n\t#[test]\n\tfn ${1:it_works}() {\n\t\t$0\n\t}\n}",
    ),
    (RUST, "struct", "struct", "#[derive(Debug, Clone)]\npub struct ${1:Name} {\n\t${2:field}: ${3:Type},\n}"),
    (RUST, "enum", "enum", "#[derive(Debug, Clone, PartialEq)]\npub enum ${1:Name} {\n\t${2:A},\n}"),
    (RUST, "impl", "impl block", "impl ${1:Type} {\n\t$0\n}"),
    (RUST, "match", "match", "match ${1:value} {\n\t${2:pattern} => ${3:todo!()},\n\t_ => $0,\n}"),
    (RUST, "for", "for loop", "for ${1:item} in ${2:iter} {\n\t$0\n}"),
    (RUST, "iflet", "if let", "if let Some(${1:x}) = ${2:option} {\n\t$0\n}"),
    (RUST, "pl", "println!", "println!(\"${1:{\\}}\", ${2});"),
    (PY, "def", "function", "def ${1:name}(${2}):\n\t${0:pass}"),
    (PY, "class", "class", "class ${1:Name}:\n\tdef __init__(self${2}):\n\t\t${0:pass}"),
    (PY, "main", "if __name__ == '__main__'", "if __name__ == \"__main__\":\n\t${0:main()}"),
    (PY, "for", "for loop", "for ${1:item} in ${2:items}:\n\t${0:pass}"),
    (PY, "try", "try / except", "try:\n\t${1:pass}\nexcept ${2:Exception} as e:\n\t${0:raise}"),
    (PY, "with", "with open", "with open(${1:path}) as ${2:f}:\n\t${0:pass}"),
    (PY, "test", "pytest function", "def test_${1:name}():\n\t${0:assert True}"),
    (JS, "fn", "function", "function ${1:name}(${2}) {\n\t$0\n}"),
    (JS, "af", "arrow function", "const ${1:name} = (${2}) => {\n\t$0\n};"),
    (JS, "for", "for … of", "for (const ${1:item} of ${2:items}) {\n\t$0\n}"),
    (JS, "log", "console.log", "console.log(${1});"),
    (JS, "cls", "class", "class ${1:Name} {\n\tconstructor(${2}) {\n\t\t$0\n\t}\n}"),
    (JS, "try", "try / catch", "try {\n\t$1\n} catch (${2:err}) {\n\t$0\n}"),
    (JS, "imp", "import", "import { ${2} } from \"${1:module}\";"),
    (GO, "func", "function", "func ${1:name}(${2}) ${3:error} {\n\t$0\n}"),
    (GO, "main", "main package", "package main\n\nfunc main() {\n\t$0\n}"),
    (GO, "iferr", "if err != nil", "if err != nil {\n\treturn ${1:err}\n}"),
    (GO, "for", "for range", "for ${1:_}, ${2:v} := range ${3:items} {\n\t$0\n}"),
    (GO, "test", "test function", "func Test${1:Name}(t *testing.T) {\n\t$0\n}"),
    (GO, "struct", "struct type", "type ${1:Name} struct {\n\t${2:Field} ${3:string}\n}"),
];

static USER: RwLock<Vec<Snippet>> = RwLock::new(Vec::new());

/// Load the user's snippets from `<config>/snippets/*.toml`; returns errors.
pub fn reload(config: &Path) -> Vec<String> {
    let mut all = vec![];
    let mut errors = vec![];
    for e in std::fs::read_dir(config.join("snippets")).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "toml") {
            continue;
        }
        let lang = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        match std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|t| toml::from_str::<File>(&t).map_err(|e| e.to_string()))
        {
            Ok(f) => all.extend(f.snippet.into_iter().map(|mut s| {
                s.langs = if lang == "all" { vec![] } else { vec![lang.clone()] };
                s
            })),
            Err(e) => errors.push(format!("{}: {e}", p.display())),
        }
    }
    if let Ok(mut u) = USER.write() {
        *u = all;
    }
    errors
}

/// Snippets for a language (the user's first, so they can replace built-ins).
pub fn for_language(language: &str) -> Vec<Snippet> {
    let lang = language.to_lowercase();
    let applies = |langs: &[String]| langs.is_empty() || langs.contains(&lang);
    let mut out: Vec<Snippet> =
        USER.read().map(|u| u.iter().filter(|s| applies(&s.langs)).cloned().collect()).unwrap_or_default();
    for (langs, prefix, description, body) in BUILTIN {
        if langs.contains(&lang.as_str()) && !out.iter().any(|s| s.prefix == *prefix) {
            out.push(Snippet {
                prefix: prefix.to_string(),
                description: description.to_string(),
                body: body.to_string(),
                langs: langs.iter().map(|l| l.to_string()).collect(),
            });
        }
    }
    out
}

/// Expand a body: the text, and its fields as char ranges in field order
/// (`$1`, `$2`, …, then `$0`, the final caret; added at the end if missing).
pub fn expand(body: &str) -> (String, Vec<(usize, usize)>) {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut n = 0usize;
    let mut stops: Vec<(u32, usize, usize)> = vec![];
    let mut i = 0;
    let num = |i: &mut usize| -> u32 {
        let mut v = 0;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            v = v * 10 + chars[*i].to_digit(10).unwrap_or(0);
            *i += 1;
        }
        v
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            n += 1;
            i += 2;
        } else if c == '$' && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
            i += 1;
            let k = num(&mut i);
            stops.push((k, n, n));
        } else if c == '$' && chars.get(i + 1) == Some(&'{') && chars.get(i + 2).is_some_and(char::is_ascii_digit) {
            i += 2;
            let k = num(&mut i);
            let mut inner = String::new();
            if chars.get(i) == Some(&':') {
                i += 1;
                let mut depth = 1;
                while i < chars.len() {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            inner.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        // Only a nested `${` opens a level; a lone `{` is text.
                        '$' if chars.get(i + 1) == Some(&'{') => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    inner.push(chars[i]);
                    i += 1;
                }
            }
            // Skip to the closing brace.
            while i < chars.len() && chars[i] != '}' {
                i += 1;
            }
            i += 1;
            let len = inner.chars().count();
            stops.push((k, n, n + len));
            out.push_str(&inner);
            n += len;
        } else {
            out.push(c);
            n += 1;
            i += 1;
        }
    }
    // $1, $2, … in order, $0 last (the end of the text when absent).
    stops.sort_by_key(|&(k, s, _)| (if k == 0 { u32::MAX } else { k }, s));
    if !stops.iter().any(|s| s.0 == 0) {
        stops.push((0, n, n));
    }
    (out, stops.into_iter().map(|(_, s, e)| (s, e)).collect())
}

/// An expanded snippet whose fields Tab moves through.
#[derive(Clone, Debug)]
pub struct Session {
    pub stops: Vec<(usize, usize)>,
    pub idx: usize,
    /// Document length when the current field was selected (typing in it
    /// shifts the fields after it).
    pub len_at: usize,
}

/// Indent `body` (after its first line) like the line it's inserted on;
/// `\t` becomes one indent unit.
fn indent(body: &str, line_indent: &str, unit: &str) -> String {
    let mut out = String::new();
    for (i, l) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !l.is_empty() {
                out.push_str(line_indent);
            }
        }
        let tabs = l.chars().take_while(|&c| c == '\t').count();
        out.push_str(&unit.repeat(tabs));
        out.push_str(&l[tabs..]);
    }
    out
}

/// Insert `body` replacing chars `start..end`, select the first field and
/// keep the rest for Tab.
pub fn insert(doc: &mut crate::document::Document, body: &str, start: usize, end: usize) {
    let b = &mut doc.buffer;
    let line = b.char_to_pos(start).line;
    let text = b.line_text(line);
    let line_indent: String = text.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let unit = b.indent_unit();
    // Expand after indenting so field positions match the inserted text.
    let indented_body = indent(body, &line_indent, &unit);
    let (text, stops) = expand(&indented_body);
    b.clear_extra();
    b.apply_edits(vec![(start, end, text)]);
    let stops: Vec<(usize, usize)> = stops.into_iter().map(|(s, e)| (start + s, start + e)).collect();
    let (s, e) = stops[0];
    b.set_cursor(s, false);
    b.set_cursor(e, true);
    doc.snippet = (stops.len() > 1).then(|| Session { stops, idx: 0, len_at: doc.buffer.len_chars() });
}

/// Tab in the editor: move to the next snippet field, or expand the prefix
/// before the caret. Returns whether Tab was used.
pub fn handle_tab(doc: &mut crate::document::Document, back: bool) -> bool {
    if let Some(mut s) = doc.snippet.take() {
        let len = doc.buffer.len_chars();
        let delta = len as isize - s.len_at as isize;
        // The current field grew/shrank by what was typed in it.
        let shift = |x: usize| (x as isize + delta).max(0) as usize;
        let (cs, ce) = s.stops[s.idx];
        let ce = shift(ce).max(cs);
        let head = doc.buffer.sel.head;
        if head < cs || head > ce {
            return false; // the caret left the snippet
        }
        s.stops[s.idx].1 = ce;
        for st in s.stops.iter_mut().skip(s.idx + 1) {
            *st = (shift(st.0), shift(st.1));
        }
        let next = if back { s.idx.checked_sub(1) } else { Some(s.idx + 1).filter(|&i| i < s.stops.len()) };
        let Some(next) = next else {
            return true;
        };
        s.idx = next;
        let (a, b) = s.stops[next];
        doc.buffer.set_cursor(a, false);
        doc.buffer.set_cursor(b.min(len), true);
        s.len_at = len;
        // The last field ($0) ends the session.
        if next + 1 < s.stops.len() {
            doc.snippet = Some(s);
        }
        return true;
    }
    if back || doc.buffer.has_multi() || doc.buffer.sel.start() != doc.buffer.sel.end() {
        return false;
    }
    let head = doc.buffer.sel.head;
    let pos = doc.buffer.char_to_pos(head);
    let line: Vec<char> = doc.buffer.line_text(pos.line).chars().collect();
    let col = pos.col.min(line.len());
    let from = (0..col).rev().take_while(|&i| line[i].is_alphanumeric() || line[i] == '_').last().unwrap_or(col);
    if from == col {
        return false;
    }
    let word: String = line[from..col].iter().collect();
    let Some(sn) = for_language(doc.language()).into_iter().find(|s| s.prefix == word) else { return false };
    insert(doc, &sn.body, head - (col - from), head);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_fields_in_order() {
        let (t, s) = expand("fn ${1:name}(${2}) {\n\t$0\n}");
        assert_eq!(t, "fn name() {\n\t\n}");
        assert_eq!(s, [(3, 7), (8, 8), (13, 13)]);
        let (t, s) = expand("println!(\"${1:{\\}}\", $2);");
        assert_eq!(t, "println!(\"{}\", );");
        assert_eq!(s[0], (10, 12));
        // No $0: the end is the last stop.
        let (_, s) = expand("a$1b");
        assert_eq!(s, [(1, 1), (2, 2)]);
    }

    #[test]
    fn tab_expands_and_moves() {
        let mut doc = crate::document::Document::new_untitled(1, 1, crate::highlight::load_theme("OneHalfDark"));
        doc.buffer.set_text("    fn");
        doc.highlight.set_syntax(crate::highlight::syntax_by_name("Rust").unwrap());
        let end = doc.buffer.len_chars();
        doc.buffer.set_cursor(end, false);
        assert!(handle_tab(&mut doc, false));
        assert_eq!(doc.buffer.text(), "    fn name() {\n        \n    }");
        assert_eq!(doc.buffer.selected_text(), "name");
        // Type over the first field, Tab to the parameters, then to the body.
        doc.buffer.insert("add");
        assert!(handle_tab(&mut doc, false));
        assert_eq!(doc.buffer.cursor_pos().col, 11);
        doc.buffer.insert("x: i32");
        assert!(handle_tab(&mut doc, false));
        assert_eq!(doc.buffer.text(), "    fn add(x: i32) {\n        \n    }");
        assert_eq!(doc.buffer.cursor_pos(), zenith_core::Position::new(1, 8));
        assert!(doc.snippet.is_none());
        // No snippet named like the word: Tab isn't used.
        doc.buffer.insert("zz");
        assert!(!handle_tab(&mut doc, false));
    }
}
