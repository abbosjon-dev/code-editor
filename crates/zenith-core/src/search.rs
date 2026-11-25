use std::ops::Range;

use ropey::Rope;

#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// Find all occurrences of `needle` in `rope`, returning char ranges.
/// The rope is searched chunk by chunk (whole lines at a time, never the
/// whole document as one string) with a literal regex.
pub fn find_all(rope: &Rope, needle: &str, opts: SearchOptions) -> Vec<Range<usize>> {
    find_limited(rope, needle, opts, usize::MAX)
}

/// [`find_all`], stopping after `limit` matches.
pub fn find_limited(rope: &Rope, needle: &str, opts: SearchOptions, limit: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.contains('\n') {
        return out;
    }
    let mut pat = regex::escape(needle);
    if opts.whole_word {
        // Word = letters, digits and `_` (not a regex \b, which treats e.g.
        // combining marks differently).
        pat = format!(r"(?:^|[^\p{{Alphabetic}}\p{{Nd}}_])({pat})(?:$|[^\p{{Alphabetic}}\p{{Nd}}_])");
    } else {
        pat = format!("({pat})");
    }
    let Ok(re) = regex::RegexBuilder::new(&pat).case_insensitive(!opts.case_sensitive).multi_line(true).build() else {
        return out;
    };
    // `region` holds whole lines: the rest of the previous chunk's last line
    // plus this chunk up to its last newline.
    let mut carry = String::new();
    let mut region_start = 0usize; // char index of the region's first char
    let scan = |region: &str, start: usize, out: &mut Vec<Range<usize>>| {
        let (mut at_byte, mut at_char) = (0usize, start);
        let mut from = 0;
        while let Some(c) = re.captures_at(region, from) {
            let m = c.get(1).unwrap();
            at_char += region[at_byte..m.start()].chars().count();
            at_byte = m.start();
            let n = m.as_str().chars().count();
            out.push(at_char..at_char + n);
            if out.len() >= limit {
                break;
            }
            // Next search from the end of the needle (a whole-word match may
            // need the separator after it as the next one's leading boundary).
            from = m.end();
            if from >= region.len() {
                break;
            }
        }
        start + region.chars().count()
    };
    for chunk in rope.chunks() {
        if out.len() >= limit {
            return out;
        }
        match chunk.rfind('\n') {
            Some(nl) => {
                if carry.is_empty() {
                    region_start = scan(&chunk[..=nl], region_start, &mut out);
                } else {
                    carry.push_str(&chunk[..=nl]);
                    region_start = scan(&carry, region_start, &mut out);
                    carry.clear();
                }
                carry.push_str(&chunk[nl + 1..]);
            }
            None => carry.push_str(chunk),
        }
    }
    if !carry.is_empty() && out.len() < limit {
        scan(&carry, region_start, &mut out);
    }
    out.truncate(limit);
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
    fn across_chunks_and_boundaries() {
        // Long enough for several rope chunks; matches near chunk edges.
        let line = "abc foo xyz foo_bar\n";
        let text = line.repeat(500);
        let r = Rope::from_str(&text);
        let hits = find_all(&r, "foo", SearchOptions { case_sensitive: true, whole_word: false });
        assert_eq!(hits.len(), 1000);
        assert!(hits.iter().all(|h| r.slice(h.clone()) == "foo"));
        let words = find_all(&r, "foo", SearchOptions { case_sensitive: true, whole_word: true });
        assert_eq!(words.len(), 500);
        assert_eq!(words[1].start, line.chars().count() + 4);
        // Adjacent whole-word matches share the separator.
        let r = Rope::from_str("foo foo foo");
        assert_eq!(find_all(&r, "foo", SearchOptions { case_sensitive: true, whole_word: true }).len(), 3);
        let r = Rope::from_str("SÖZ söz");
        assert_eq!(find_all(&r, "söz", SearchOptions::default()), vec![0..3, 4..7]);
    }

    #[test]
    fn limit() {
        let r = Rope::from_str(&"a ".repeat(1000));
        assert_eq!(find_limited(&r, "a", SearchOptions::default(), 10).len(), 10);
    }

    #[test]
    fn unicode() {
        let r = Rope::from_str("salom dunyo, Salom!");
        let hits = find_all(&r, "salom", SearchOptions::default());
        assert_eq!(hits, vec![0..5, 13..18]);
    }
}
