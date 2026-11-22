//! `@file` mentions in prompts for AI agents: find the token being typed,
//! rank workspace files for it and insert the chosen path.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// A file that can be mentioned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Shown in the list (workspace-relative, extra folders prefixed).
    pub label: String,
    /// Inserted after `@` (relative to the agent's working directory, or absolute).
    pub insert: String,
}

/// The `@query` under the caret: (char index of `@`, query).
pub fn mention_at(text: &str, caret: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let caret = caret.min(chars.len());
    let mut i = caret;
    while i > 0 {
        let c = chars[i - 1];
        if c == '@' {
            let at = i - 1;
            if at == 0 || chars[at - 1].is_whitespace() || "([{\"'`".contains(chars[at - 1]) {
                return Some((at, chars[i..caret].iter().collect()));
            }
            return None;
        }
        if c.is_whitespace() {
            return None;
        }
        i -= 1;
    }
    None
}

/// Best matches for `query` (candidates keep their order when it is empty).
pub fn rank(candidates: &[Candidate], query: &str, limit: usize) -> Vec<Candidate> {
    if query.is_empty() {
        return candidates.iter().take(limit).cloned().collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let hay = nucleo_matcher::Utf32Str::new(&c.label, &mut buf);
            pattern.score(hay, &mut matcher).map(|s| (i, s))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(limit).map(|(i, _)| candidates[i].clone()).collect()
}

/// Replace `@query` (from char `at` to `caret`) with `@insert `; returns the
/// new text and caret.
pub fn apply(text: &str, at: usize, caret: usize, insert: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let caret = caret.min(chars.len());
    let mut out: String = chars[..at].iter().collect();
    out.push('@');
    out.push_str(insert);
    let tail: String = chars[caret..].iter().collect();
    if !tail.starts_with(' ') {
        out.push(' ');
    }
    let new_caret = out.chars().count() + usize::from(tail.starts_with(' '));
    out.push_str(&tail);
    (out, new_caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_token_under_caret() {
        assert_eq!(mention_at("explain @src/ma", 15), Some((8, "src/ma".into())));
        assert_eq!(mention_at("@", 1), Some((0, String::new())));
        assert_eq!(mention_at("mail a@b.uz", 11), None); // e-mail, not a mention
        assert_eq!(mention_at("see @a.rs and", 13), None);
        assert_eq!(mention_at("(@x", 3), Some((1, "x".into())));
        assert_eq!(mention_at("сўз @фа", 7), Some((4, "фа".into())));
    }

    #[test]
    fn ranks_and_applies() {
        let c = |l: &str| Candidate { label: l.into(), insert: l.into() };
        let all = vec![c("README.md"), c("src/main.rs"), c("src/lib.rs")];
        assert_eq!(rank(&all, "main", 5)[0].label, "src/main.rs");
        assert_eq!(rank(&all, "", 2).len(), 2);
        assert!(rank(&all, "zzz", 5).is_empty());
        assert_eq!(apply("why @ma fails", 4, 7, "src/main.rs"), ("why @src/main.rs fails".into(), 17));
        assert_eq!(apply("look @ma", 5, 8, "src/main.rs"), ("look @src/main.rs ".into(), 18));
    }
}
