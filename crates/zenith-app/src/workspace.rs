//! Background workspace services: file index (for quick open) and
//! find-in-files. Both run on worker threads and respect .gitignore.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;

const MAX_FILES: usize = 200_000;

pub struct FileIndex {
    pub files: Vec<String>,
    rx: Option<Receiver<Vec<String>>>,
    pub indexing: bool,
}

impl FileIndex {
    pub fn build(root: &Path) -> Self {
        let (tx, rx) = channel();
        let root = root.to_path_buf();
        thread::spawn(move || {
            let mut out = Vec::new();
            let walker =
                ignore::WalkBuilder::new(&root).hidden(false).filter_entry(|e| e.file_name() != ".git").build();
            for e in walker.flatten() {
                if e.file_type().is_some_and(|t| t.is_file()) {
                    if let Ok(rel) = e.path().strip_prefix(&root) {
                        out.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                    if out.len() >= MAX_FILES {
                        break;
                    }
                }
            }
            out.sort();
            let _ = tx.send(out);
        });
        Self { files: vec![], rx: Some(rx), indexing: true }
    }

    pub fn empty() -> Self {
        Self { files: vec![], rx: None, indexing: false }
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            if let Ok(files) = rx.try_recv() {
                self.files = files;
                self.indexing = false;
                self.rx = None;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub path: PathBuf,
    pub rel: String,
    pub line: usize,
    /// Line text (leading whitespace removed, truncated).
    pub text: String,
    /// Char ranges of the matches within `text`.
    pub matches: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOpts {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// Compile the search query into a regex (escaped unless `opts.regex`).
pub fn build_regex(query: &str, opts: SearchOpts) -> Result<regex::Regex, String> {
    let mut pat = if opts.regex { query.to_string() } else { regex::escape(query) };
    if opts.whole_word {
        pat = format!(r"\b(?:{pat})\b");
    }
    regex::RegexBuilder::new(&pat)
        .case_insensitive(!opts.case_sensitive)
        .multi_line(true)
        .build()
        .map_err(|e| e.to_string().lines().last().unwrap_or("invalid regex").trim().to_string())
}

/// Replace every match in `text`. In regex mode `$1`/`${name}` refer to
/// capture groups; otherwise the replacement is literal. Returns the new text
/// and the number of replacements.
pub fn replace_in_text(text: &str, re: &regex::Regex, replacement: &str, opts: SearchOpts) -> (String, usize) {
    let count = re.find_iter(text).count();
    if count == 0 {
        return (text.to_string(), 0);
    }
    let out = if opts.regex {
        re.replace_all(text, replacement).into_owned()
    } else {
        re.replace_all(text, regex::NoExpand(replacement)).into_owned()
    };
    (out, count)
}

pub struct SearchJob {
    rx: Receiver<Vec<SearchHit>>,
    cancel: Arc<AtomicBool>,
    pub done: bool,
}

pub const MAX_HITS: usize = 5_000;
const MAX_LINE_SHOWN: usize = 240;

impl SearchJob {
    pub fn start(root: &Path, query: &str, opts: SearchOpts) -> Result<Self, String> {
        let re = build_regex(query, opts)?;
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let root = root.to_path_buf();
        let c2 = cancel.clone();
        thread::spawn(move || {
            let mut total = 0;
            let walker = ignore::WalkBuilder::new(&root).build();
            for e in walker.flatten() {
                if c2.load(Ordering::Relaxed) || total >= MAX_HITS {
                    break;
                }
                if !e.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if meta.len() > 4 * 1024 * 1024 {
                    continue;
                }
                let Ok(bytes) = std::fs::read(e.path()) else { continue };
                if bytes.iter().take(8000).any(|b| *b == 0) {
                    continue; // binary
                }
                let text = String::from_utf8_lossy(&bytes);
                let rel = e.path().strip_prefix(&root).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
                let mut hits = vec![];
                for (i, line) in text.lines().enumerate() {
                    let found: Vec<(usize, usize)> = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
                    if found.is_empty() {
                        continue;
                    }
                    hits.push(make_hit(e.path(), &rel, i, line, &found));
                    total += 1;
                    if total >= MAX_HITS {
                        break;
                    }
                }
                if !hits.is_empty() && tx.send(hits).is_err() {
                    break;
                }
            }
        });
        Ok(Self { rx, cancel, done: false })
    }

    /// Collect new hits; sets `done` when the worker finished.
    pub fn poll(&mut self, into: &mut Vec<SearchHit>) {
        loop {
            match self.rx.try_recv() {
                Ok(h) => into.extend(h),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
            }
        }
    }
}

impl Drop for SearchJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Build a result row: trim leading whitespace, truncate, convert byte
/// ranges to char ranges in the shown text.
fn make_hit(path: &Path, rel: &str, line_no: usize, line: &str, found: &[(usize, usize)]) -> SearchHit {
    let lead = line.len() - line.trim_start().len();
    let shown = &line[lead..];
    let to_char = |b: usize| shown[..b.min(shown.len())].chars().count();
    let text: String = shown.chars().take(MAX_LINE_SHOWN).collect();
    let n = text.chars().count();
    let matches = found
        .iter()
        .filter(|(s, _)| *s >= lead)
        .map(|&(s, e)| (to_char(s - lead).min(n), to_char(e - lead).min(n)))
        .filter(|(s, e)| s < e)
        .collect();
    SearchHit { path: path.to_path_buf(), rel: rel.to_string(), line: line_no, text, matches }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_options() {
        let o = |c, w, r| SearchOpts { case_sensitive: c, whole_word: w, regex: r };
        let re = build_regex("foo", o(false, false, false)).unwrap();
        assert_eq!(re.find_iter("Foo food foo").count(), 3);
        let re = build_regex("foo", o(true, true, false)).unwrap();
        assert_eq!(re.find_iter("Foo food foo").count(), 1);
        let re = build_regex("a.c", o(true, false, false)).unwrap();
        assert!(!re.is_match("abc") && re.is_match("a.c"));
        let re = build_regex(r"(\w+)@(\w+)", o(true, false, true)).unwrap();
        assert_eq!(
            replace_in_text("ali@uz vali@com", &re, "$2:$1", o(true, false, true)),
            ("uz:ali com:vali".into(), 2)
        );
        // Literal mode must not expand `$1`.
        let re = build_regex("x", o(true, false, false)).unwrap();
        assert_eq!(replace_in_text("x x", &re, "$1", o(true, false, false)).0, "$1 $1");
        assert!(build_regex("(", o(true, false, true)).is_err());
    }

    #[test]
    fn hit_ranges_are_chars_after_trim() {
        let line = "    salom dunyo salom";
        let re = build_regex("salom", SearchOpts::default()).unwrap();
        let found: Vec<_> = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
        let h = make_hit(Path::new("a"), "a", 0, line, &found);
        assert_eq!(h.text, "salom dunyo salom");
        assert_eq!(h.matches, vec![(0, 5), (12, 17)]);
        let line = "ёж ёж";
        let re = build_regex("ж", SearchOpts::default()).unwrap();
        let found: Vec<_> = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
        assert_eq!(make_hit(Path::new("a"), "a", 0, line, &found).matches, vec![(1, 2), (4, 5)]);
    }
}
