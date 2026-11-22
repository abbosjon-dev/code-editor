//! Git merge conflict markers: detection and resolution.
//!
//! ```text
//! <<<<<<< HEAD          start
//! current (ours)
//! ||||||| base          base (optional, diff3 style)
//! original
//! =======               mid
//! incoming (theirs)
//! >>>>>>> branch        end
//! ```

/// Line numbers of one conflict's markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub start: usize,
    pub base: Option<usize>,
    pub mid: usize,
    pub end: usize,
}

impl Conflict {
    /// Lines of the "current" (ours) side.
    pub fn current(&self) -> std::ops::Range<usize> {
        self.start + 1..self.base.unwrap_or(self.mid)
    }

    pub fn incoming(&self) -> std::ops::Range<usize> {
        self.mid + 1..self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Current,
    Incoming,
    Both,
}

fn is_marker(line: &str, c: char) -> bool {
    let mut it = line.chars();
    (0..7).all(|_| it.next() == Some(c)) && it.next().is_none_or(|ch| ch == ' ' || ch == '\r' || ch == '\n')
}

/// Find well-formed conflicts in `lines`.
pub fn find<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Conflict> {
    let mut out = vec![];
    let (mut start, mut base, mut mid) = (None, None, None);
    for (i, l) in lines.enumerate() {
        if is_marker(l, '<') {
            (start, base, mid) = (Some(i), None, None);
        } else if start.is_some() && mid.is_none() && is_marker(l, '|') {
            base = Some(i);
        } else if start.is_some() && mid.is_none() && is_marker(l, '=') {
            mid = Some(i);
        } else if is_marker(l, '>') {
            if let (Some(s), Some(m)) = (start, mid) {
                out.push(Conflict { start: s, base, mid: m, end: i });
            }
            (start, base, mid) = (None, None, None);
        }
    }
    out
}

/// Text replacing lines `start..=end` (inclusive, with their newlines).
pub fn resolve(lines: &[String], c: &Conflict, r: Resolution) -> String {
    let take = |range: std::ops::Range<usize>| -> String { lines[range].concat() };
    let mut text = match r {
        Resolution::Current => take(c.current()),
        Resolution::Incoming => take(c.incoming()),
        Resolution::Both => {
            let mut cur = take(c.current());
            if !cur.is_empty() && !cur.ends_with('\n') {
                cur.push('\n');
            }
            cur + &take(c.incoming())
        }
    };
    // The `>>>>>>>` line had no newline (end of file): keep it that way.
    if !lines[c.end].ends_with('\n') && text.ends_with('\n') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split_inclusive('\n').map(str::to_string).collect()
    }

    const TEXT: &str = "a\n<<<<<<< HEAD\nours\n=======\ntheirs\nmore\n>>>>>>> feature\nz\n";

    #[test]
    fn finds_and_resolves() {
        let l = lines(TEXT);
        let c = find(l.iter().map(String::as_str));
        assert_eq!(c, vec![Conflict { start: 1, base: None, mid: 3, end: 6 }]);
        assert_eq!(resolve(&l, &c[0], Resolution::Current), "ours\n");
        assert_eq!(resolve(&l, &c[0], Resolution::Incoming), "theirs\nmore\n");
        assert_eq!(resolve(&l, &c[0], Resolution::Both), "ours\ntheirs\nmore\n");
    }

    #[test]
    fn diff3_and_malformed() {
        let l = lines("<<<<<<< a\nx\n||||||| base\no\n=======\ny\n>>>>>>> b");
        let c = find(l.iter().map(String::as_str));
        assert_eq!(c[0].base, Some(2));
        assert_eq!(resolve(&l, &c[0], Resolution::Current), "x");
        assert_eq!(resolve(&l, &c[0], Resolution::Incoming), "y");
        // Not markers: too long, or no separator.
        assert!(find(["<<<<<<<< x", "=======", ">>>>>>> y"].into_iter()).is_empty());
        assert!(find(["<<<<<<< x", ">>>>>>> y"].into_iter()).is_empty());
    }
}
