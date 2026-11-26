//! Outline: the active file's symbols as a tree in the sidebar, from the
//! language server (document symbols) or, without one, a quick scan for
//! common declarations. The symbol around the caret is highlighted; a click
//! jumps to it.

use std::time::{Duration, Instant};

use zenith_lsp::Symbol;

// LSP symbol kinds.
const MODULE: u32 = 2;
const CLASS: u32 = 5;
const METHOD: u32 = 6;
const ENUM: u32 = 10;
const INTERFACE: u32 = 11;
const FUNCTION: u32 = 12;
const CONSTANT: u32 = 14;
const STRUCT: u32 = 23;

/// Short glyph per symbol kind.
pub fn kind_glyph(kind: u32) -> &'static str {
    match kind {
        MODULE | 3 | 4 => "▣",
        CLASS | STRUCT => "C",
        METHOD | FUNCTION | 9 => "ƒ",
        ENUM | 22 => "E",
        INTERFACE => "I",
        CONSTANT => "K",
        7 | 8 | 13 => "v",
        26 => "T",
        _ => "•",
    }
}

/// Declarations found without a language server: (kind, name) per line,
/// nested by indentation.
pub fn scan(text: &str, language: &str) -> Vec<Symbol> {
    let lang = language.to_lowercase();
    let mut out = vec![];
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        let words: Vec<&str> = t
            .split(|c: char| c.is_whitespace() || c == '(' || c == '<' || c == ':' || c == '{')
            .filter(|w| !w.is_empty())
            .collect();
        // Skip leading modifiers.
        let mods = [
            "pub",
            "pub(crate)",
            "pub(super)",
            "async",
            "unsafe",
            "export",
            "default",
            "static",
            "const",
            "extern",
            "abstract",
            "public",
            "private",
            "protected",
            "final",
        ];
        let mut k = 0;
        while k < words.len() && (mods.contains(&words[k]) || words[k].starts_with("pub(")) {
            // `const NAME` / `static NAME` in Rust are declarations themselves.
            if lang == "rust" && (words[k] == "const" || words[k] == "static") && k + 1 < words.len() {
                break;
            }
            k += 1;
        }
        let Some(&kw) = words.get(k) else { continue };
        let name = words.get(k + 1).map(|n| n.trim_end_matches([';', '=', ',']));
        let kind = match (lang.as_str(), kw) {
            ("rust", "fn") => Some(FUNCTION),
            ("rust", "struct") => Some(STRUCT),
            ("rust", "enum") => Some(ENUM),
            ("rust", "trait") => Some(INTERFACE),
            ("rust", "mod") => Some(MODULE),
            ("rust", "impl") => Some(CLASS),
            ("rust", "const" | "static") => Some(CONSTANT),
            ("python", "def") => Some(FUNCTION),
            ("python", "class") => Some(CLASS),
            ("go", "func") => Some(FUNCTION),
            ("go", "type") => Some(STRUCT),
            (_, "function") => Some(FUNCTION),
            (_, "class") => Some(CLASS),
            (_, "interface") => Some(INTERFACE),
            _ => None,
        };
        let (Some(kind), Some(name)) = (kind, name) else { continue };
        // `impl Trait for Type`: show the whole header.
        let name = if kw == "impl" {
            t.trim_start_matches("impl").trim().trim_end_matches('{').trim().to_string()
        } else if lang == "go" && kw == "func" && t.starts_with("func (") {
            // Method: func (r *T) Name(
            t.split(')').nth(1).and_then(|r| r.trim().split('(').next()).unwrap_or(name).trim().to_string()
        } else {
            name.to_string()
        };
        if name.is_empty() {
            continue;
        }
        out.push(Symbol {
            name,
            detail: String::new(),
            kind,
            line: i as u32,
            character: indent as u32,
            depth: indent / 4,
            path: None,
        });
    }
    // Depth from indentation, relative to the shallowest level.
    let min = out.iter().map(|s| s.depth).min().unwrap_or(0);
    for s in &mut out {
        s.depth -= min;
    }
    out
}

/// The symbol the caret is in: the last one starting at or before its line,
/// preferring deeper ones.
pub fn current(symbols: &[Symbol], line: usize) -> Option<usize> {
    symbols.iter().enumerate().filter(|(_, s)| s.line as usize <= line).map(|(i, _)| i).last()
}

#[derive(Default)]
pub struct Outline {
    pub doc_id: u64,
    pub version: u64,
    pub symbols: Vec<Symbol>,
    /// A document-symbols request in flight.
    pub pending: Option<u64>,
    pub requested_at: Option<Instant>,
    pub from_lsp: bool,
    pub filter: String,
}

impl Outline {
    /// Whether the outline should be asked again for this document state.
    pub fn stale(&self, doc_id: u64, version: u64) -> bool {
        let fresh = self.doc_id == doc_id && self.version == version;
        let waited = self.requested_at.is_none_or(|t| t.elapsed() > Duration::from_millis(600));
        !fresh && waited && (self.pending.is_none() || self.doc_id != doc_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_rust_and_python() {
        let rust =
            "pub struct A {\n    x: i32,\n}\n\nimpl A {\n    pub fn new() -> Self {\n        A { x: 0 }\n    }\n    \
                    async fn go(&self) {}\n}\n\nconst MAX: usize = 3;\nfn main() {}\n";
        let s = scan(rust, "Rust");
        let names: Vec<(&str, usize)> = s.iter().map(|s| (s.name.as_str(), s.depth)).collect();
        assert_eq!(names, [("A", 0), ("A", 0), ("new", 1), ("go", 1), ("MAX", 0), ("main", 0)]);
        assert_eq!(s[0].kind, STRUCT);
        assert_eq!(s[1].kind, CLASS);
        assert_eq!(current(&s, 7), Some(2)); // inside `new`
        let py = "class Shop:\n    def add(self, x):\n        pass\n\nasync def main():\n    pass\n";
        let s = scan(py, "Python");
        let names: Vec<(&str, usize)> = s.iter().map(|s| (s.name.as_str(), s.depth)).collect();
        assert_eq!(names, [("Shop", 0), ("add", 1), ("main", 0)]);
        let go = "type Server struct {}\nfunc (s *Server) Start(port int) {}\nfunc main() {}\n";
        let s = scan(go, "Go");
        assert_eq!(s.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Server", "Start", "main"]);
    }
}
