//! Test coverage in the gutter: which lines the tests ran (green) and which
//! they didn't (red). Reads the reports test tools write — lcov
//! (cargo llvm-cov, flutter test --coverage, jest / vitest / c8,
//! `coverage lcov`), Cobertura XML (coverage.py, many others) and Go's
//! coverprofile — and reloads them when they change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Hits per line (0-based) of one file.
pub type FileCoverage = HashMap<usize, u64>;

#[derive(Default)]
pub struct Coverage {
    /// Absolute path → line hits.
    pub files: HashMap<PathBuf, FileCoverage>,
    /// The report read, and when it was written.
    pub source: Option<(PathBuf, SystemTime)>,
    pub error: Option<String>,
}

/// Report files looked for, in order.
pub const REPORTS: &[&str] = &[
    "lcov.info",
    "coverage/lcov.info",
    "coverage.lcov",
    "target/llvm-cov/lcov.info",
    "coverage.xml",
    "coverage/cobertura-coverage.xml",
    "cobertura.xml",
    "coverage.out",
    "cover.out",
];

/// The newest coverage report in `root`.
pub fn find_report(root: &Path) -> Option<(PathBuf, SystemTime)> {
    REPORTS
        .iter()
        .map(|r| root.join(r))
        .filter_map(|p| Some((p.clone(), std::fs::metadata(&p).ok()?.modified().ok()?)))
        .max_by_key(|(_, t)| *t)
}

impl Coverage {
    /// Read the newest report if it changed; true when something changed.
    pub fn refresh(&mut self, root: &Path) -> bool {
        let found = find_report(root);
        if found == self.source {
            return false;
        }
        self.source = found.clone();
        self.files.clear();
        self.error = None;
        let Some((path, _)) = found else { return true };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let raw = if name.ends_with(".xml") {
                    parse_cobertura(&text)
                } else if text.starts_with("mode:") {
                    parse_go(&text)
                } else {
                    parse_lcov(&text)
                };
                self.files = resolve(raw, root);
                if self.files.is_empty() {
                    self.error = Some(format!("{name}: no files of this folder in it"));
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        true
    }

    pub fn for_file(&self, path: &Path) -> Option<&FileCoverage> {
        self.files.get(path)
    }

    /// Covered and total executable lines of a file.
    pub fn summary(&self, path: &Path) -> Option<(usize, usize)> {
        let f = self.for_file(path)?;
        Some((f.values().filter(|&&h| h > 0).count(), f.len()))
    }

    pub fn total(&self) -> (usize, usize) {
        let all = self.files.values().flat_map(|f| f.values());
        let (mut c, mut t) = (0, 0);
        for &h in all {
            t += 1;
            if h > 0 {
                c += 1;
            }
        }
        (c, t)
    }
}

/// Report paths can be absolute, relative to the folder, or (Go) module
/// paths: keep those that point to files of `root`.
fn resolve(raw: Vec<(String, FileCoverage)>, root: &Path) -> HashMap<PathBuf, FileCoverage> {
    let mut out: HashMap<PathBuf, FileCoverage> = HashMap::new();
    for (p, lines) in raw {
        let path = Path::new(&p);
        let full = if path.is_absolute() {
            Some(path.to_path_buf())
        } else {
            // Go: "example.com/mod/pkg/x.go" — try dropping leading parts.
            let parts: Vec<&str> = p.split('/').collect();
            (0..parts.len()).map(|i| root.join(parts[i..].join("/"))).find(|c| c.is_file())
        };
        let Some(full) = full.filter(|f| f.starts_with(root) || f.is_file()) else { continue };
        let full = full.canonicalize().unwrap_or(full);
        let entry = out.entry(full).or_default();
        for (l, h) in lines {
            *entry.entry(l).or_insert(0) += h;
        }
    }
    out
}

/// lcov: `SF:path`, `DA:line,hits`, `end_of_record`.
pub fn parse_lcov(text: &str) -> Vec<(String, FileCoverage)> {
    let mut out = vec![];
    let mut cur: Option<(String, FileCoverage)> = None;
    for l in text.lines() {
        let l = l.trim();
        if let Some(p) = l.strip_prefix("SF:") {
            cur = Some((p.to_string(), FileCoverage::new()));
        } else if let Some(d) = l.strip_prefix("DA:") {
            let mut it = d.split(',');
            if let (Some(line), Some(hits), Some((_, f))) = (it.next(), it.next(), cur.as_mut()) {
                if let (Ok(line), Ok(hits)) = (line.parse::<usize>(), hits.parse::<u64>()) {
                    *f.entry(line.saturating_sub(1)).or_insert(0) += hits;
                }
            }
        } else if l == "end_of_record" {
            out.extend(cur.take());
        }
    }
    out.extend(cur);
    out
}

/// Cobertura XML: `<class filename="…">` … `<line number="12" hits="3"/>`.
/// Filenames are relative to a `<source>`.
pub fn parse_cobertura(text: &str) -> Vec<(String, FileCoverage)> {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let key = format!("{name}=\"");
        let i = tag.find(&key)? + key.len();
        Some(tag[i..].split('"').next()?.to_string())
    };
    let sources: Vec<String> = text
        .split("<source>")
        .skip(1)
        .filter_map(|s| s.split("</source>").next())
        .map(|s| s.trim().to_string())
        .collect();
    let mut out: Vec<(String, FileCoverage)> = vec![];
    for chunk in text.split("<class ").skip(1) {
        let tag = chunk.split('>').next().unwrap_or("");
        let Some(file) = attr(tag, "filename") else { continue };
        let mut f = FileCoverage::new();
        let body = chunk.split("</class>").next().unwrap_or(chunk);
        for line in body.split("<line ").skip(1) {
            let t = line.split('>').next().unwrap_or("");
            if let (Some(n), Some(h)) = (attr(t, "number"), attr(t, "hits")) {
                if let (Ok(n), Ok(h)) = (n.parse::<usize>(), h.parse::<u64>()) {
                    *f.entry(n.saturating_sub(1)).or_insert(0) += h;
                }
            }
        }
        // With a <source>, the filename is relative to it.
        let file = match sources.first().filter(|s| !s.is_empty()) {
            Some(src) if !Path::new(&file).is_absolute() && !file.starts_with('/') => {
                format!("{}/{file}", src.trim_end_matches(['/', '\\']))
            }
            _ => file,
        };
        out.push((file, f));
    }
    out
}

/// Go coverprofile: `file:startLine.col,endLine.col statements count`.
pub fn parse_go(text: &str) -> Vec<(String, FileCoverage)> {
    let mut by_file: HashMap<String, FileCoverage> = HashMap::new();
    for l in text.lines().skip(1) {
        let Some((file, rest)) = l.rsplit_once(':') else { continue };
        let mut parts = rest.split_whitespace();
        let (Some(span), _, Some(count)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(count) = count.parse::<u64>() else { continue };
        let Some((a, b)) = span.split_once(',') else { continue };
        let line = |s: &str| s.split('.').next().and_then(|n| n.parse::<usize>().ok());
        let (Some(a), Some(b)) = (line(a), line(b)) else { continue };
        let f = by_file.entry(file.to_string()).or_default();
        for n in a..=b {
            let e = f.entry(n.saturating_sub(1)).or_insert(0);
            *e = (*e).max(count);
        }
    }
    by_file.into_iter().collect()
}

/// The command that runs the tests with coverage for this folder.
pub fn command(root: &Path) -> Option<(&'static str, String)> {
    if root.join("Cargo.toml").exists() {
        return Some(if which::which("cargo-llvm-cov").is_ok() {
            ("Rust", "cargo llvm-cov --lcov --output-path lcov.info".into())
        } else {
            ("Rust", "cargo install cargo-llvm-cov && cargo llvm-cov --lcov --output-path lcov.info".into())
        });
    }
    if std::fs::read_to_string(root.join("pubspec.yaml")).is_ok_and(|p| p.contains("flutter")) {
        return Some(("Flutter", "flutter test --coverage".into()));
    }
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
        let cmd = if pkg.contains("\"vitest\"") {
            "npx vitest run --coverage --coverage.reporter=lcov"
        } else {
            "npx jest --coverage --coverageReporters=lcov"
        };
        return Some(("Node", cmd.into()));
    }
    if root.join("go.mod").exists() {
        return Some(("Go", "go test -coverprofile=coverage.out ./...".into()));
    }
    let py = ["pyproject.toml", "setup.py", "pytest.ini", "requirements.txt"].iter().any(|f| root.join(f).exists());
    if py {
        let python = if cfg!(windows) { "python" } else { "python3" };
        return Some(("Python", format!("{python} -m coverage run -m pytest; {python} -m coverage lcov")));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reports() {
        let lcov =
            "TN:\nSF:/w/src/lib.rs\nDA:1,3\nDA:2,0\nDA:5,1\nend_of_record\nSF:src/b.rs\nDA:10,0\nend_of_record\n";
        let r = parse_lcov(lcov);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1.get(&0), Some(&3));
        assert_eq!(r[0].1.get(&1), Some(&0));
        assert_eq!(r[1].0, "src/b.rs");
        let xml = r#"<coverage><sources><source>/w</source></sources><packages><package><classes>
            <class name="app" filename="app.py" line-rate="0.5"><lines><line number="1" hits="1"/><line number="3" hits="0"/></lines></class>
            </classes></package></packages></coverage>"#;
        let c = parse_cobertura(xml);
        assert_eq!(c[0].0, "/w/app.py");
        let rel = parse_cobertura(&xml.replace("<source>/w</source>", "<source>src</source>"));
        assert_eq!(rel[0].0, "src/app.py");
        assert_eq!((c[0].1.get(&0), c[0].1.get(&2)), (Some(&1), Some(&0)));
        let go = "mode: set\nexample.com/m/pkg/x.go:3.14,5.2 2 1\nexample.com/m/pkg/x.go:7.2,7.20 1 0\n";
        let g = parse_go(go);
        assert_eq!(g[0].1.len(), 4);
        assert_eq!(g[0].1.get(&6), Some(&0));
    }

    #[test]
    fn resolves_and_summarizes() {
        let d = std::env::temp_dir().join(format!("zenith-cov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("pkg")).unwrap();
        std::fs::write(d.join("pkg/x.go"), "package pkg\n").unwrap();
        std::fs::write(d.join("coverage.out"), "mode: set\nexample.com/m/pkg/x.go:1.1,2.1 1 1\nexample.com/m/pkg/x.go:3.1,3.5 1 0\nother/gone.go:1.1,1.2 1 1\n").unwrap();
        let mut c = Coverage::default();
        assert!(c.refresh(&d));
        assert!(!c.refresh(&d)); // unchanged
        let x = d.join("pkg/x.go").canonicalize().unwrap();
        assert_eq!(c.summary(&x), Some((2, 3)));
        assert_eq!(c.files.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}

// ------------------------------------------------------------------ changed lines

/// Lines (0-based) added or changed per file in a `git diff -U0` (paths
/// relative to the repository).
pub fn changed_lines(diff: &str) -> HashMap<String, Vec<usize>> {
    let mut out: HashMap<String, Vec<usize>> = HashMap::new();
    let mut file: Option<String> = None;
    for l in diff.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            file = p.strip_prefix("b/").map(str::to_string).filter(|_| p != "/dev/null");
        } else if let (Some(h), Some(f)) = (l.strip_prefix("@@ "), &file) {
            // @@ -a,b +c,d @@
            let Some(new) = h.split_whitespace().find(|w| w.starts_with('+')) else { continue };
            let new = &new[1..];
            let (start, count) = match new.split_once(',') {
                Some((s, c)) => (s.parse::<usize>().unwrap_or(0), c.parse::<usize>().unwrap_or(0)),
                None => (new.parse::<usize>().unwrap_or(0), 1),
            };
            if count > 0 && start > 0 {
                out.entry(f.clone()).or_default().extend((start - 1)..(start - 1 + count));
            }
        }
    }
    out
}

/// Coverage of the changed lines of one file.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub covered: usize,
    /// Changed lines the tests don't run (0-based).
    pub uncovered: Vec<usize>,
}

/// Changed lines that are executable (in the report), covered or not.
/// Files the report doesn't know are skipped (not code, or not tested).
pub fn changes_coverage(cov: &Coverage, changed: &HashMap<String, Vec<usize>>, repo: &Path) -> Vec<ChangedFile> {
    let mut out: Vec<ChangedFile> = changed
        .iter()
        .filter_map(|(rel, lines)| {
            let p = repo.join(rel);
            let p = p.canonicalize().unwrap_or(p);
            let f = cov.for_file(&p)?;
            let mut covered = 0;
            let mut uncovered = vec![];
            for l in lines {
                match f.get(l) {
                    Some(&h) if h > 0 => covered += 1,
                    Some(_) => uncovered.push(*l),
                    None => {}
                }
            }
            (covered + uncovered.len() > 0).then_some(ChangedFile { path: p, covered, uncovered })
        })
        .collect();
    out.sort_by(|a, b| b.uncovered.len().cmp(&a.uncovered.len()).then(a.path.cmp(&b.path)));
    out
}

#[cfg(test)]
mod changed_tests {
    use super::*;

    #[test]
    fn diff_lines_and_their_coverage() {
        let diff = "diff --git a/calc.py b/calc.py\n--- a/calc.py\n+++ b/calc.py\n@@ -2 +2,2 @@ def add\n-x\n+y\n+z\n@@ -9,0 +11 @@\n+w\ndiff --git a/gone.py b/gone.py\n--- a/gone.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        let c = changed_lines(diff);
        assert_eq!(c.get("calc.py"), Some(&vec![1, 2, 10]));
        assert!(!c.contains_key("gone.py"));
        let d = std::env::temp_dir().join(format!("zenith-covdiff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("calc.py"), "x\n").unwrap();
        let mut cov = Coverage::default();
        let p = d.join("calc.py").canonicalize().unwrap();
        cov.files.insert(p.clone(), FileCoverage::from([(1, 3), (2, 0), (5, 1)]));
        let r = changes_coverage(&cov, &c, &d);
        assert_eq!(r, [ChangedFile { path: p, covered: 1, uncovered: vec![2] }]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
