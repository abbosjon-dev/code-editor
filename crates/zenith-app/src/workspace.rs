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
    /// Search every root; paths in extra roots are shown as `root-name/rel`.
    /// Files are walked and searched in parallel; each file's bytes are
    /// searched as a whole first, so files without a match cost one scan.
    pub fn start(roots: &[PathBuf], query: &str, opts: SearchOpts) -> Result<Self, String> {
        let re = build_regex(query, opts)?;
        let bytes_re = regex::bytes::RegexBuilder::new(re.as_str())
            .case_insensitive(!opts.case_sensitive)
            .multi_line(true)
            .build()
            .map_err(|e| e.to_string())?;
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let roots = roots.to_vec();
        let c2 = cancel.clone();
        thread::spawn(move || {
            let total = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for (ri, root) in roots.iter().enumerate() {
                let prefix = if ri == 0 {
                    String::new()
                } else {
                    format!("{}/", root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                };
                ignore::WalkBuilder::new(root).build_parallel().run(|| {
                    let (tx, re, bre, cancel, total, prefix) =
                        (tx.clone(), re.clone(), bytes_re.clone(), c2.clone(), total.clone(), prefix.clone());
                    let root = root.clone();
                    Box::new(move |e| {
                        use ignore::WalkState;
                        if cancel.load(Ordering::Relaxed) || total.load(Ordering::Relaxed) >= MAX_HITS {
                            return WalkState::Quit;
                        }
                        let Ok(e) = e else { return WalkState::Continue };
                        if !e.file_type().is_some_and(|t| t.is_file()) {
                            return WalkState::Continue;
                        }
                        if e.metadata().map(|m| m.len()).unwrap_or(0) > 4 * 1024 * 1024 {
                            return WalkState::Continue;
                        }
                        let Ok(bytes) = std::fs::read(e.path()) else { return WalkState::Continue };
                        if bytes.iter().take(8000).any(|b| *b == 0) || !bre.is_match(&bytes) {
                            return WalkState::Continue; // binary, or no match
                        }
                        let rel = format!(
                            "{prefix}{}",
                            e.path().strip_prefix(&root).unwrap_or(e.path()).to_string_lossy().replace('\\', "/")
                        );
                        let hits = file_hits(e.path(), &rel, &bytes, &re, &bre, &total);
                        if !hits.is_empty() && tx.send(hits).is_err() {
                            return WalkState::Quit;
                        }
                        WalkState::Continue
                    })
                });
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
                    // Files were searched in parallel: a stable order at the end.
                    into.truncate(MAX_HITS);
                    into.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
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

/// The matching lines of one file: line numbers are counted only up to
/// each match, and each matching line is searched once for its ranges.
fn file_hits(
    path: &Path,
    rel: &str,
    bytes: &[u8],
    re: &regex::Regex,
    bre: &regex::bytes::Regex,
    total: &std::sync::atomic::AtomicUsize,
) -> Vec<SearchHit> {
    let mut hits = vec![];
    let (mut line_no, mut counted_to, mut done_to) = (0usize, 0usize, 0usize);
    for m in bre.find_iter(bytes) {
        if m.start() < done_to {
            continue; // this line is already listed
        }
        line_no += bytes[counted_to..m.start()].iter().filter(|&&b| b == b'\n').count();
        counted_to = m.start();
        let ls = bytes[..m.start()].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let le = bytes[m.start()..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |i| m.start() + i);
        done_to = le + 1;
        let text = String::from_utf8_lossy(&bytes[ls..le]);
        let line = text.trim_end_matches('\r');
        let found: Vec<(usize, usize)> = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
        if found.is_empty() {
            continue; // e.g. a match spanning lines
        }
        hits.push(make_hit(path, rel, line_no, line, &found));
        if total.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_HITS {
            break;
        }
    }
    hits
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

    /// Timings on a large tree: `ZENITH_PERF_DIR=<dir> cargo test --release perf_workspace -- --ignored --nocapture`
    /// (defaults to the cargo registry's sources).
    #[test]
    #[ignore]
    fn perf_workspace() {
        use std::time::Instant;
        let dir = std::env::var("ZENITH_PERF_DIR").map(PathBuf::from).unwrap_or_else(|_| {
            let base = std::env::var("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cargo"));
            std::fs::read_dir(base.join("registry/src")).unwrap().flatten().next().unwrap().path()
        });
        let t = Instant::now();
        let mut idx = FileIndex::build(&dir);
        while idx.indexing {
            std::thread::sleep(std::time::Duration::from_millis(1));
            idx.poll();
        }
        println!("index: {} files in {:?}", idx.files.len(), t.elapsed());

        // Quick open: fuzzy-match every path (what the palette does per keystroke).
        use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
        let mut m = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths());
        for q in ["lib", "src/parser.rs", "srclibrs"] {
            let t = Instant::now();
            let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
            let mut buf = Vec::new();
            let n = idx
                .files
                .iter()
                .filter(|f| pattern.score(nucleo_matcher::Utf32Str::new(f, &mut buf), &mut m).is_some())
                .count();
            println!("fuzzy {q:?}: {n} matches in {:?}", t.elapsed());
        }

        for (q, regex) in [("ZenithNeverFound12345", false), ("unsafe", false), (r"fn\s+new\(", true)] {
            let t = Instant::now();
            let opts = SearchOpts { regex, ..Default::default() };
            let mut job = SearchJob::start(std::slice::from_ref(&dir), q, opts).unwrap();
            let mut hits = vec![];
            let mut first = None;
            while !job.done {
                job.poll(&mut hits);
                if first.is_none() && !hits.is_empty() {
                    first = Some(t.elapsed());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            println!("search {q:?}: {} hits, first after {first:?}, done in {:?}", hits.len(), t.elapsed());
        }
    }

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
    fn hits_have_right_lines() {
        let text = b"one\r\ntwo salom\n\nthree\nsalom salom\nend";
        let re = build_regex("salom", SearchOpts::default()).unwrap();
        let bre = regex::bytes::Regex::new("(?i)salom").unwrap();
        let total = std::sync::atomic::AtomicUsize::new(0);
        let h = file_hits(Path::new("a"), "a", text, &re, &bre, &total);
        assert_eq!(h.iter().map(|h| h.line).collect::<Vec<_>>(), [1, 4]);
        assert_eq!(h[1].matches, vec![(0, 5), (6, 11)]);
        assert_eq!(h[0].text, "two salom");
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
