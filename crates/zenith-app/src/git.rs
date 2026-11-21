//! Git integration using the `git` command line (no libgit2 build needed):
//! status, staging, commits, and per-line change markers against HEAD.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineChange {
    Added,
    Modified,
    /// Lines were deleted just above this line.
    Deleted,
}

/// Per-line change markers for the gutter: (first line, last line exclusive, kind).
pub type Hunks = Vec<(usize, usize, LineChange)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub rel: String,
    pub path: PathBuf,
    /// Index (staged) status letter, e.g. 'M', 'A', 'D', ' ', '?'.
    pub staged: char,
    /// Working tree status letter.
    pub unstaged: char,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        self.staged != ' ' && self.staged != '?'
    }

    pub fn is_unstaged(&self) -> bool {
        self.unstaged != ' '
    }

    /// Single letter shown in the UI.
    pub fn letter(&self) -> char {
        match (self.staged, self.unstaged) {
            ('?', _) => 'U',
            (_, 'D') | ('D', _) => 'D',
            ('A', _) => 'A',
            ('R', _) => 'R',
            _ => 'M',
        }
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `git init` in `dir`.
pub fn init(dir: &Path) -> Result<(), String> {
    git(dir, &["init", "-q"]).map(|_| ())
}

pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .map(|p| p.canonicalize().unwrap_or(p))
}

pub fn parse_status(root: &Path, porcelain_z: &str) -> Vec<StatusEntry> {
    let mut out = vec![];
    let mut parts = porcelain_z.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let staged = chars.next().unwrap_or(' ');
        let unstaged = chars.next().unwrap_or(' ');
        let rel = entry[3..].to_string();
        if staged == 'R' || staged == 'C' {
            parts.next(); // original path follows renames
        }
        if rel.ends_with('/') {
            continue;
        }
        out.push(StatusEntry { path: root.join(&rel), rel, staged, unstaged });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// Line-level diff of `current` against `base` for gutter markers.
pub fn diff_hunks(base: &str, current: &str) -> Hunks {
    use similar::{DiffOp, TextDiff};
    let diff = TextDiff::configure().timeout(Duration::from_millis(50)).diff_lines(base, current);
    let mut out = vec![];
    for op in diff.ops() {
        match *op {
            DiffOp::Insert { new_index, new_len, .. } => out.push((new_index, new_index + new_len, LineChange::Added)),
            DiffOp::Replace { new_index, new_len, .. } => {
                out.push((new_index, new_index + new_len, LineChange::Modified))
            }
            DiffOp::Delete { new_index, .. } => out.push((new_index, new_index + 1, LineChange::Deleted)),
            DiffOp::Equal { .. } => {}
        }
    }
    out
}

pub struct Git {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub status: Vec<StatusEntry>,
    status_rx: Option<Receiver<(Vec<StatusEntry>, Option<String>)>>,
    last_refresh: Option<Instant>,
}

impl Git {
    pub fn open(dir: &Path) -> Option<Self> {
        let root = repo_root(dir)?;
        let mut g = Self { root, branch: None, status: vec![], status_rx: None, last_refresh: None };
        g.refresh();
        Some(g)
    }

    /// Refresh status in the background.
    pub fn refresh(&mut self) {
        if self.status_rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let root = self.root.clone();
        thread::spawn(move || {
            // --no-optional-locks: never take index.lock in the background,
            // so it can't race with stage/commit.
            let status =
                git(&root, &["--no-optional-locks", "status", "--porcelain=v1", "-z", "--untracked-files=all"])
                    .map(|s| parse_status(&root, &s))
                    .unwrap_or_default();
            let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok().map(|s| s.trim().to_string());
            let _ = tx.send((status, branch));
        });
        self.status_rx = Some(rx);
        self.last_refresh = Some(Instant::now());
    }

    /// Poll background work; refreshes every few seconds.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.status_rx {
            if let Ok((status, branch)) = rx.try_recv() {
                changed = status != self.status || branch != self.branch;
                self.status = status;
                self.branch = branch;
                self.status_rx = None;
            }
        }
        if self.last_refresh.is_none_or(|t| t.elapsed() > Duration::from_secs(4)) {
            self.refresh();
        }
        changed
    }

    fn rel(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.root).ok().map(|p| p.to_string_lossy().replace('\\', "/"))
    }

    /// File content at HEAD (None if untracked / not in a repo).
    pub fn head_text(&self, path: &Path) -> Option<String> {
        let rel = self.rel(path)?;
        git(&self.root, &["show", &format!("HEAD:{rel}")]).ok()
    }

    pub fn stage(&mut self, rel: &str) -> Result<(), String> {
        git(&self.root, &["add", "--", rel]).map(|_| self.refresh_now())
    }

    pub fn unstage(&mut self, rel: &str) -> Result<(), String> {
        git(&self.root, &["reset", "-q", "HEAD", "--", rel])
            .or_else(|_| git(&self.root, &["rm", "--cached", "-q", "--", rel]))
            .map(|_| self.refresh_now())
    }

    pub fn stage_all(&mut self) -> Result<(), String> {
        git(&self.root, &["add", "-A"]).map(|_| self.refresh_now())
    }

    pub fn commit(&mut self, message: &str) -> Result<String, String> {
        let staged_any = self.status.iter().any(|e| e.is_staged());
        if !staged_any {
            git(&self.root, &["add", "-A"])?;
        }
        let out = git(&self.root, &["commit", "-m", message])?;
        self.refresh_now();
        Ok(out.lines().next().unwrap_or("").to_string())
    }

    pub fn push(&self) -> Result<String, String> {
        git(&self.root, &["push"]).map(|s| if s.trim().is_empty() { "Pushed".into() } else { s })
    }

    pub fn pull(&self) -> Result<String, String> {
        git(&self.root, &["pull", "--ff-only"])
    }

    fn refresh_now(&mut self) {
        self.status_rx = None;
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let root = Path::new("/r");
        let s = " M src/a.rs\0A  b.rs\0?? new.txt\0R  c.rs\0old.rs\0";
        let st = parse_status(root, s);
        assert_eq!(st.len(), 4);
        assert_eq!(st[0].rel, "b.rs");
        assert!(st[0].is_staged());
        let new = st.iter().find(|e| e.rel == "new.txt").unwrap();
        assert_eq!(new.letter(), 'U');
        let a = st.iter().find(|e| e.rel == "src/a.rs").unwrap();
        assert!(a.is_unstaged() && !a.is_staged());
        assert_eq!(st.iter().find(|e| e.rel == "c.rs").unwrap().letter(), 'R');
    }

    #[test]
    fn hunks() {
        let base = "a\nb\nc\nd\n";
        let cur = "a\nB\nc\nnew\nd\n";
        let h = diff_hunks(base, cur);
        assert!(h.contains(&(1, 2, LineChange::Modified)));
        assert!(h.contains(&(3, 4, LineChange::Added)));
        let h = diff_hunks(base, "a\nd\n");
        assert_eq!(h, vec![(1, 2, LineChange::Deleted)]);
    }

    #[test]
    fn real_repo_roundtrip() {
        let dir = std::env::temp_dir().join(format!("zenith-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir, &["init", "-q"]).is_err() {
            return; // git not available
        }
        let _ = git(&dir, &["config", "user.email", "t@example.com"]);
        let _ = git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let mut g = Git::open(&dir).unwrap();
        g.commit("init").unwrap();
        assert_eq!(g.head_text(&g.root.join("a.txt")).as_deref(), Some("one\n"));
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        g.refresh_now();
        let deadline = Instant::now() + Duration::from_secs(5);
        while g.status.is_empty() && Instant::now() < deadline {
            g.poll();
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(g.status[0].letter(), 'M');
        let _ = std::fs::remove_dir_all(&dir);
    }
}
