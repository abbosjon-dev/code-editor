//! Text helpers: URIs, UTF-16 columns, snippets, markdown cleanup.

use std::path::{Path, PathBuf};

pub fn path_to_uri(path: &Path) -> String {
    url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", path.display()))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

/// Convert a char column on `line` to a UTF-16 column.
pub fn char_col_to_utf16(line: &str, col: usize) -> u32 {
    line.chars().take(col).map(|c| c.len_utf16() as u32).sum()
}

/// Convert a UTF-16 column on `line` to a char column.
pub fn utf16_to_char_col(line: &str, units: u32) -> usize {
    let mut acc = 0u32;
    for (i, c) in line.chars().enumerate() {
        if acc >= units {
            return i;
        }
        acc += c.len_utf16() as u32;
    }
    line.chars().count()
}

/// Turn an LSP snippet into plain text: `foo(${1:x}, $2)$0` → `foo(x, )`.
pub fn strip_snippet(s: &str) -> String {
    parse_snippet(s).0
}

/// Like [`strip_snippet`], also returning the char range of the first tab
/// stop (`$1`, falling back to `$0`) so the editor can select it.
pub fn parse_snippet(s: &str) -> (String, Option<(usize, usize)>) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut n = 0usize; // chars written
    let mut stops: Vec<(u32, usize, usize)> = vec![];
    let mut i = 0;
    let read_num = |i: &mut usize| -> u32 {
        let mut v = 0u32;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            v = v * 10 + chars[*i].to_digit(10).unwrap();
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
        } else if c == '$' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            i += 1;
            let num = read_num(&mut i);
            stops.push((num, n, n));
        } else if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            // ${N:default} or ${N|a,b|}
            i += 2;
            let num = read_num(&mut i);
            let mut depth = 1;
            let mut inner = String::new();
            if i < chars.len() && (chars[i] == ':' || chars[i] == '|') {
                let choice = chars[i] == '|';
                i += 1;
                while i < chars.len() {
                    match chars[i] {
                        '{' => depth += 1,
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
                if choice {
                    inner = inner.trim_end_matches('|').split(',').next().unwrap_or("").to_string();
                } else {
                    inner = strip_snippet(&inner);
                }
            } else {
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
            }
            let len = inner.chars().count();
            stops.push((num, n, n + len));
            out.push_str(&inner);
            n += len;
            i += 1; // closing brace
        } else {
            out.push(c);
            n += 1;
            i += 1;
        }
    }
    let first = stops
        .iter()
        .filter(|s| s.0 > 0)
        .min_by_key(|s| s.0)
        .or_else(|| stops.iter().find(|s| s.0 == 0))
        .map(|s| (s.1, s.2));
    // A lone `$0` at the very end is just "caret at end" — no selection needed.
    let first = first.filter(|&(a, b)| !(a == b && a == n));
    (out, first)
}

/// Make hover markdown readable as plain text.
pub fn clean_markdown(s: &str) -> String {
    let mut out: Vec<&str> = vec![];
    for line in s.lines() {
        let t = line.trim_end();
        if t.starts_with("```") {
            continue;
        }
        if t.chars().all(|c| c == '-' || c == '_' || c == '*') && t.len() >= 3 {
            out.push("");
            continue;
        }
        out.push(t);
    }
    let mut text = out.join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns() {
        let line = "a😀b";
        assert_eq!(char_col_to_utf16(line, 2), 3);
        assert_eq!(utf16_to_char_col(line, 3), 2);
        assert_eq!(utf16_to_char_col(line, 99), 3);
    }

    #[test]
    fn snippets() {
        assert_eq!(strip_snippet("foo(${1:x}, $2)$0"), "foo(x, )");
        assert_eq!(strip_snippet("${1|a,b|}"), "a");
        assert_eq!(strip_snippet("if ${1:cond} {\n\t$0\n}"), "if cond {\n\t\n}");
        assert_eq!(strip_snippet("\\$x"), "$x");
        assert_eq!(parse_snippet("push(${1:value})$0"), ("push(value)".into(), Some((5, 10))));
        assert_eq!(parse_snippet("new()$0"), ("new()".into(), None));
        assert_eq!(parse_snippet("foo($0)"), ("foo()".into(), Some((4, 4))));
    }

    #[test]
    fn uri_roundtrip() {
        let p = std::env::temp_dir().join("dir with space").join("ф.rs");
        let uri = path_to_uri(&p);
        assert!(uri.starts_with("file://"));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }
}
