use std::ops::Range;

use ropey::Rope;

#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// Find all occurrences of `needle` in `rope`, returning char ranges.
/// Works line by line so it never allocates the whole document as one string.
pub fn find_all(rope: &Rope, needle: &str, opts: SearchOptions) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.contains('\n') {
        return out;
    }
    let needle_cmp = if opts.case_sensitive { needle.to_string() } else { needle.to_lowercase() };
    let needle_chars = needle.chars().count();
    for (line_idx, line) in rope.lines().enumerate() {
        let line_start = rope.line_to_char(line_idx);
        let text: String = line.chars().collect();
        let hay = if opts.case_sensitive { text.clone() } else { text.to_lowercase() };
        // Lowercasing can change byte lengths for some scripts; fall back to
        // char-wise compare when it does.
        if hay.len() != text.len() {
            let hay_chars: Vec<char> = hay.chars().collect();
            let nd: Vec<char> = needle_cmp.chars().collect();
            if nd.len() > hay_chars.len() {
                continue;
            }
            for i in 0..=hay_chars.len() - nd.len() {
                if hay_chars[i..i + nd.len()] == nd[..] {
                    out.push(line_start + i..line_start + i + needle_chars);
                }
            }
            continue;
        }
        let mut from = 0;
        while let Some(found) = hay[from..].find(&needle_cmp) {
            let byte = from + found;
            let char_off = text[..byte].chars().count();
            let ok = !opts.whole_word || {
                let before = text[..byte].chars().next_back();
                let after = text[byte + needle_cmp.len()..].chars().next();
                let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                !is_word(before) && !is_word(after)
            };
            if ok {
                out.push(line_start + char_off..line_start + char_off + needle_chars);
            }
            from = byte + needle_cmp.len().max(1);
            if from > hay.len() {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_case_insensitive() {
        let r = Rope::from_str("Foo foo\nbar FOO");
        let hits = find_all(&r, "foo", SearchOptions::default());
        assert_eq!(hits, vec![0..3, 4..7, 12..15]);
    }

    #[test]
    fn whole_word() {
        let r = Rope::from_str("foo food foo_bar foo");
        let hits = find_all(&r, "foo", SearchOptions { case_sensitive: true, whole_word: true });
        assert_eq!(hits, vec![0..3, 17..20]);
    }

    #[test]
    fn unicode() {
        let r = Rope::from_str("salom dunyo, Salom!");
        let hits = find_all(&r, "salom", SearchOptions::default());
        assert_eq!(hits, vec![0..5, 13..18]);
    }
}
