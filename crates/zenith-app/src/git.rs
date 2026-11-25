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
        self.staged != ' ' && self.staged != '?' && !self.is_conflicted()
    }

    pub fn is_unstaged(&self) -> bool {
        self.unstaged != ' ' && !self.is_conflicted()
    }

    /// Unmerged path (merge / rebase conflict).
    pub fn is_conflicted(&self) -> bool {
        matches!((self.staged, self.unstaged), ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D'))
    }

    /// Single letter shown in the UI.
    pub fn letter(&self) -> char {
        if self.is_conflicted() {
            return '!';
        }
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

/// `git` with `input` on stdin.
fn git_input(root: &Path, args: &[&str], input: &[u8]) -> Result<String, String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("git: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Replace hunk `h` of `base` (lines `old_start..+old_lines`) with `with`.
fn splice(base: &str, h: &crate::review::Hunk, with: &[String]) -> String {
    let lines: Vec<&str> = base.split_inclusive('\n').collect();
    let mut out = String::with_capacity(base.len());
    for l in &lines[..h.old_start.min(lines.len())] {
        out.push_str(l);
    }
    for l in with {
        out.push_str(l);
    }
    for l in lines.iter().skip(h.old_start + h.old_lines.len()) {
        out.push_str(l);
    }
    out
}

/// Replace the new side of hunk `h` in `text` (lines `new_start..+new_lines`) with `with`.
fn splice_new(text: &str, h: &crate::review::Hunk, with: &[String]) -> String {
    let flipped = crate::review::Hunk {
        old_start: h.new_start,
        old_lines: h.new_lines.clone(),
        new_start: h.old_start,
        new_lines: h.old_lines.clone(),
    };
    splice(text, &flipped, with)
}

/// The hunk of `hunks` that covers `line` (a line of the "new" side).
fn hunk_at(hunks: &[crate::review::Hunk], line: usize) -> Option<&crate::review::Hunk> {
    hunks.iter().find(|h| h.new_start <= line && line < h.new_start + h.new_lines.len().max(1))
}

/// Where git commands run: here, or in the folder on an SSH host.
#[derive(Clone)]
struct Runner {
    root: PathBuf,
    remote: Option<crate::remote::RemoteSpec>,
}

impl Runner {
    fn git(&self, args: &[&str]) -> Result<String, String> {
        match &self.remote {
            None => git(&self.root, args),
            Some(spec) => crate::remote::git(spec, args),
        }
    }

    fn git_input(&self, args: &[&str], input: &[u8]) -> Result<String, String> {
        match &self.remote {
            None => git_input(&self.root, args, input),
            Some(spec) => crate::remote::git_input(spec, args, input),
        }
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

/// Status of a folder on a host from [`crate::remote::git_state`]: paths
/// are made relative to the folder (it may be inside the repository).
fn remote_state(
    cache: &Path,
    (status, branch, stashes, ops, prefix): (String, String, usize, Vec<String>, String),
) -> RepoState {
    let status = parse_status(cache, &status)
        .into_iter()
        .filter_map(|mut e| {
            let rel = e.rel.strip_prefix(&prefix)?.to_string();
            e.path = cache.join(&rel);
            e.rel = rel;
            Some(e)
        })
        .collect();
    let has = |n: &str| ops.iter().any(|o| o == n);
    let operation = if has("rebase-merge") || has("rebase-apply") {
        Some(Operation::Rebase)
    } else if has("MERGE_HEAD") {
        Some(Operation::Merge)
    } else if has("CHERRY_PICK_HEAD") {
        Some(Operation::CherryPick)
    } else if has("REVERT_HEAD") {
        Some(Operation::Revert)
    } else {
        None
    };
    RepoState { status, branch: (!branch.is_empty()).then_some(branch), operation, stashes }
}

/// Line-level diff of `current` against `base` for gutter markers.
/// Who last changed a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    pub hash: String,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub summary: String,
}

impl BlameLine {
    pub fn is_uncommitted(&self) -> bool {
        self.hash.chars().all(|c| c == '0')
    }
}

/// Parse `git blame --porcelain` output into one entry per line.
pub fn parse_blame(out: &str) -> Vec<BlameLine> {
    use std::collections::HashMap;
    let mut commits: HashMap<String, BlameLine> = HashMap::new();
    let mut lines = vec![];
    let mut cur: Option<String> = None;
    for l in out.lines() {
        if let Some(text) = l.strip_prefix('\t') {
            let _ = text;
            if let Some(h) = cur.take() {
                lines.push(commits.get(&h).cloned().unwrap_or(BlameLine {
                    hash: h,
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                }));
            }
            continue;
        }
        let mut parts = l.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        if key.len() == 40 && key.chars().all(|c| c.is_ascii_hexdigit()) {
            cur = Some(key.to_string());
            commits.entry(key.to_string()).or_insert_with(|| BlameLine {
                hash: key.to_string(),
                author: String::new(),
                time: 0,
                summary: String::new(),
            });
            continue;
        }
        if let Some(h) = &cur {
            if let Some(c) = commits.get_mut(h) {
                match key {
                    "author" => c.author = val.to_string(),
                    "author-time" => c.time = val.parse().unwrap_or(0),
                    "summary" => c.summary = val.to_string(),
                    _ => {}
                }
            }
        }
    }
    lines
}

/// "3 days ago" style relative time.
pub fn relative_time(then: i64, now: i64) -> String {
    let d = (now - then).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86_400 => (d / 3600, "hour"),
        86_400..2_592_000 => (d / 86_400, "day"),
        2_592_000..31_536_000 => (d / 2_592_000, "month"),
        _ => (d / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// One row of a side-by-side diff: (old line number, new line number), either
/// may be missing for pure deletions/insertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRow {
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub changed: bool,
}

/// Align two versions of a file into side-by-side rows.
pub fn side_by_side(old: &str, new: &str) -> Vec<DiffRow> {
    use similar::{DiffOp, TextDiff};
    let diff = TextDiff::configure().timeout(Duration::from_millis(200)).diff_lines(old, new);
    let mut rows = vec![];
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, new_index, len } => rows.extend((0..len).map(|k| DiffRow {
                old: Some(old_index + k),
                new: Some(new_index + k),
                changed: false,
            })),
            DiffOp::Delete { old_index, old_len, .. } => {
                rows.extend((0..old_len).map(|k| DiffRow { old: Some(old_index + k), new: None, changed: true }))
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                rows.extend((0..new_len).map(|k| DiffRow { old: None, new: Some(new_index + k), changed: true }))
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                for k in 0..old_len.max(new_len) {
                    rows.push(DiffRow {
                        old: (k < old_len).then_some(old_index + k),
                        new: (k < new_len).then_some(new_index + k),
                        changed: true,
                    });
                }
            }
        }
    }
    rows
}

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

/// A multi-step operation in progress (stopped for conflicts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Operation::Merge => "Merging",
            Operation::Rebase => "Rebasing",
            Operation::CherryPick => "Cherry-picking",
            Operation::Revert => "Reverting",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
        }
    }

    fn detect(git_dir: &Path) -> Option<Self> {
        let has = |n: &str| git_dir.join(n).exists();
        if has("rebase-merge") || has("rebase-apply") {
            Some(Operation::Rebase)
        } else if has("MERGE_HEAD") {
            Some(Operation::Merge)
        } else if has("CHERRY_PICK_HEAD") {
            Some(Operation::CherryPick)
        } else if has("REVERT_HEAD") {
            Some(Operation::Revert)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RepoState {
    status: Vec<StatusEntry>,
    branch: Option<String>,
    operation: Option<Operation>,
    stashes: usize,
}

pub struct Git {
    pub root: PathBuf,
    git_dir: PathBuf,
    run: Runner,
    /// Set after commands that may change files (pull, checkout, stash) on a
    /// remote host: the mirror should fetch them.
    pub host_files_changed: bool,
    pub branch: Option<String>,
    pub status: Vec<StatusEntry>,
    pub operation: Option<Operation>,
    /// Number of stash entries.
    pub stashes: usize,
    status_rx: Option<Receiver<RepoState>>,
    last_refresh: Option<Instant>,
}

impl Git {
    pub fn open(dir: &Path) -> Option<Self> {
        let root = repo_root(dir)?;
        let git_dir = git(&root, &["rev-parse", "--absolute-git-dir"])
            .map(|s| PathBuf::from(s.trim()))
            .unwrap_or_else(|_| root.join(".git"));
        let mut g = Self {
            run: Runner { root: root.clone(), remote: None },
            host_files_changed: false,
            root,
            git_dir,
            branch: None,
            status: vec![],
            operation: None,
            stashes: 0,
            status_rx: None,
            last_refresh: None,
        };
        g.refresh();
        Some(g)
    }

    /// The repository of a folder on an SSH host; `cache` is its local mirror.
    /// Every git command runs on the host.
    pub fn open_remote(cache: &Path, spec: &crate::remote::RemoteSpec) -> Option<Self> {
        let state = crate::remote::git_state(spec).ok()?;
        let mut g = Self {
            run: Runner { root: cache.to_path_buf(), remote: Some(spec.clone()) },
            host_files_changed: false,
            root: cache.to_path_buf(),
            git_dir: PathBuf::new(),
            branch: None,
            status: vec![],
            operation: None,
            stashes: 0,
            status_rx: None,
            last_refresh: None,
        };
        g.apply_state(remote_state(cache, state));
        g.refresh();
        Some(g)
    }

    pub fn is_remote(&self) -> bool {
        self.run.remote.is_some()
    }

    fn apply_state(&mut self, st: RepoState) -> bool {
        let changed = st.status != self.status
            || st.branch != self.branch
            || st.operation != self.operation
            || st.stashes != self.stashes;
        self.status = st.status;
        self.branch = st.branch;
        self.operation = st.operation;
        self.stashes = st.stashes;
        changed
    }

    /// Refresh status in the background.
    pub fn refresh(&mut self) {
        if self.status_rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let root = self.root.clone();
        let git_dir = self.git_dir.clone();
        if let Some(spec) = self.run.remote.clone() {
            thread::spawn(move || {
                if let Ok(st) = crate::remote::git_state(&spec) {
                    let _ = tx.send(remote_state(&root, st));
                }
            });
            self.status_rx = Some(rx);
            self.last_refresh = Some(Instant::now());
            return;
        }
        thread::spawn(move || {
            // --no-optional-locks: never take index.lock in the background,
            // so it can't race with stage/commit.
            let status =
                git(&root, &["--no-optional-locks", "status", "--porcelain=v1", "-z", "--untracked-files=all"])
                    .map(|s| parse_status(&root, &s))
                    .unwrap_or_default();
            let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok().map(|s| s.trim().to_string());
            let stashes = git(&root, &["--no-optional-locks", "stash", "list"]).map(|s| s.lines().count()).unwrap_or(0);
            let operation = Operation::detect(&git_dir);
            let _ = tx.send(RepoState { status, branch, operation, stashes });
        });
        self.status_rx = Some(rx);
        self.last_refresh = Some(Instant::now());
    }

    /// Poll background work; refreshes every few seconds.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.status_rx {
            match rx.try_recv() {
                Ok(st) => {
                    changed = self.apply_state(st);
                    self.status_rx = None;
                }
                // A failed remote refresh (offline) sends nothing.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.status_rx = None,
                Err(_) => {}
            }
        }
        let every = Duration::from_secs(if self.is_remote() { 5 } else { 4 });
        if self.last_refresh.is_none_or(|t| t.elapsed() > every) {
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
        self.run.git(&["show", &format!("HEAD:./{rel}")]).ok()
    }

    pub fn stage(&mut self, rel: &str) -> Result<(), String> {
        self.run.git(&["add", "--", rel]).map(|_| self.refresh_now())
    }

    /// Write `content` as the staged version of `rel`.
    fn set_index(&mut self, rel: &str, content: &str) -> Result<(), String> {
        let mode = self
            .run
            .git(&["ls-files", "-s", "--", rel])?
            .split_whitespace()
            .next()
            .map(str::to_string)
            .unwrap_or_else(|| "100644".into());
        let sha =
            self.run.git_input(&["hash-object", "-w", "--stdin", &format!("--path={rel}")], content.as_bytes())?;
        self.run.git(&["update-index", "--cacheinfo", &format!("{mode},{},{rel}", sha.trim())])?;
        self.refresh_now();
        Ok(())
    }

    /// Stage only the change at `line` of `text` (the editor's content).
    pub fn stage_hunk(&mut self, path: &Path, text: &str, line: usize) -> Result<String, String> {
        let rel = self.rel(path).ok_or("not in this repository")?;
        let Ok(index) = self.run.git(&["show", &format!(":./{rel}")]) else {
            // Not in the index yet (a new file): stage it whole.
            self.stage(&rel)?;
            return Ok(format!("Staged {rel}"));
        };
        let index = index.replace("\r\n", "\n");
        let hunks = crate::review::hunks(&index, text);
        let h = hunk_at(&hunks, line).ok_or("This change is already staged")?;
        let staged = splice(&index, h, &h.new_lines);
        self.set_index(&rel, &staged)?;
        Ok(format!("Staged the change at line {}", line + 1))
    }

    /// Take the staged change at `line` (of `text`) back out of the index.
    pub fn unstage_hunk(&mut self, path: &Path, text: &str, line: usize) -> Result<String, String> {
        let rel = self.rel(path).ok_or("not in this repository")?;
        let index = self.run.git(&["show", &format!(":./{rel}")]).map_err(|_| "Nothing staged in this file")?;
        let index = index.replace("\r\n", "\n");
        let head = self.run.git(&["show", &format!("HEAD:./{rel}")]).unwrap_or_default().replace("\r\n", "\n");
        // The editor's line in the index: shift by the unstaged changes above it.
        let unstaged = crate::review::hunks(&index, text);
        let mut index_line = line as isize;
        for h in &unstaged {
            if h.new_start + h.new_lines.len() <= line {
                index_line += h.old_lines.len() as isize - h.new_lines.len() as isize;
            } else if h.new_start <= line {
                return Err("This change isn't staged".into());
            }
        }
        let staged = crate::review::hunks(&head, &index);
        let h = hunk_at(&staged, index_line.max(0) as usize).ok_or("No staged change here")?;
        let back = splice_new(&index, h, &h.old_lines);
        self.set_index(&rel, &back)?;
        Ok(format!("Unstaged the change at line {}", line + 1))
    }

    pub fn unstage(&mut self, rel: &str) -> Result<(), String> {
        self.run
            .git(&["reset", "-q", "HEAD", "--", rel])
            .or_else(|_| self.run.git(&["rm", "--cached", "-q", "--", rel]))
            .map(|_| self.refresh_now())
    }

    pub fn stage_all(&mut self) -> Result<(), String> {
        self.run.git(&["add", "-A"]).map(|_| self.refresh_now())
    }

    pub fn commit(&mut self, message: &str) -> Result<String, String> {
        if self.status.iter().any(|e| e.is_conflicted()) {
            return Err("Resolve the merge conflicts first (stage each resolved file with +)".into());
        }
        let staged_any = self.status.iter().any(|e| e.is_staged());
        if !staged_any {
            self.run.git(&["add", "-A"])?;
        }
        let out = self.run.git(&["commit", "-m", message])?;
        self.refresh_now();
        Ok(out.lines().next().unwrap_or("").to_string())
    }

    pub fn push(&self) -> Result<String, String> {
        self.run.git(&["push"]).map(|s| if s.trim().is_empty() { "Pushed".into() } else { s })
    }

    pub fn pull(&mut self) -> Result<String, String> {
        let r = self.run.git(&["pull", "--ff-only"]);
        self.refresh_now();
        r
    }

    /// Blame a file in the background; the result arrives on the returned channel.
    pub fn blame_async(&self, path: &Path) -> Option<Receiver<Vec<BlameLine>>> {
        let rel = self.rel(path)?;
        let run = self.run.clone();
        let (tx, rx) = channel();
        thread::spawn(move || {
            let out = run.git(&["--no-optional-locks", "blame", "--porcelain", "--", &rel]).unwrap_or_default();
            let _ = tx.send(parse_blame(&out));
        });
        Some(rx)
    }

    /// Local branches, most recently committed first.
    pub fn branches(&self) -> Vec<String> {
        self.run
            .git(&["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads/"])
            .map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default()
    }

    pub fn checkout(&mut self, branch: &str) -> Result<(), String> {
        self.run.git(&["checkout", "-q", branch])?;
        self.refresh_now();
        Ok(())
    }

    pub fn create_branch(&mut self, branch: &str) -> Result<(), String> {
        self.run.git(&["checkout", "-q", "-b", branch])?;
        self.refresh_now();
        Ok(())
    }

    /// Abort the merge / rebase / cherry-pick in progress.
    pub fn abort_operation(&mut self) -> Result<String, String> {
        let op = self.operation.ok_or("Nothing to abort")?;
        let r = self.run.git(&[op.command(), "--abort"]);
        self.refresh_now();
        r.map(|_| format!("{} aborted", op.label()))
    }

    /// Continue after the conflicts were resolved and staged.
    pub fn continue_operation(&mut self) -> Result<String, String> {
        let op = self.operation.ok_or("Nothing to continue")?;
        if self.status.iter().any(|e| e.is_conflicted()) {
            return Err("Resolve the merge conflicts first (stage each resolved file with +)".into());
        }
        // Keep the prepared commit message instead of opening an editor.
        let r = match op {
            Operation::Merge => self.run.git(&["commit", "--no-edit"]),
            _ => self.run.git(&["-c", "core.editor=true", op.command(), "--continue"]),
        };
        self.refresh_now();
        r.map(|s| s.lines().next().unwrap_or("").to_string())
    }

    /// Stash all changes (including untracked files).
    pub fn stash(&mut self, message: &str) -> Result<String, String> {
        let mut args = vec!["stash", "push", "-u"];
        if !message.trim().is_empty() {
            args.extend(["-m", message.trim()]);
        }
        let r = self.run.git(&args);
        self.refresh_now();
        r.map(|s| s.lines().next().unwrap_or("Stashed").to_string())
    }

    pub fn stash_pop(&mut self) -> Result<String, String> {
        let r = self.run.git(&["stash", "pop"]);
        self.refresh_now();
        r.map(|_| "Stash applied".to_string())
    }

    fn refresh_now(&mut self) {
        self.status_rx = None;
        // Commands that touch the working tree changed files on the host.
        self.host_files_changed |= self.is_remote();
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_and_unstage_one_hunk() {
        let dir = std::env::temp_dir().join(format!("zenith-hunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir, &["init", "-q"]).is_err() {
            return;
        }
        let _ = git(&dir, &["config", "user.email", "t@example.com"]);
        let _ = git(&dir, &["config", "user.name", "Test"]);
        let base: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &base).unwrap();
        git(&dir, &["add", "-A"]).unwrap();
        git(&dir, &["commit", "-qm", "init"]).unwrap();
        // Two separate changes: line 2 and line 18.
        let text = base.replace("line 2\n", "line two\n").replace("line 18\n", "line 18\nadded\n");
        std::fs::write(dir.join("a.txt"), &text).unwrap();
        let mut g = Git::open(&dir).unwrap();
        let path = g.root.join("a.txt");
        g.stage_hunk(&path, &text, 18).unwrap();
        let staged = git(&g.root, &["diff", "--cached"]).unwrap();
        assert!(staged.contains("+added") && !staged.contains("line two"), "{staged}");
        assert!(g.stage_hunk(&path, &text, 18).unwrap_err().contains("already"));
        g.stage_hunk(&path, &text, 1).unwrap();
        assert!(git(&g.root, &["diff", "--cached"]).unwrap().contains("+line two"));
        // Unstage the second one again.
        g.unstage_hunk(&path, &text, 18).unwrap();
        let staged = git(&g.root, &["diff", "--cached"]).unwrap();
        assert!(!staged.contains("+added") && staged.contains("+line two"), "{staged}");
        // The working tree is untouched.
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), text);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn conflicts_are_their_own_group() {
        let st = parse_status(Path::new("/r"), "UU a.rs\0AA b.rs\0M  c.rs\0");
        let a = &st[0];
        assert!(a.is_conflicted() && !a.is_staged() && !a.is_unstaged());
        assert_eq!(a.letter(), '!');
        assert!(st[1].is_conflicted());
        assert!(!st[2].is_conflicted() && st[2].is_staged());
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
    fn blame_parsing() {
        let h1 = "a".repeat(40);
        let h2 = "0".repeat(40);
        let out = format!(
            "{h1} 1 1 2\nauthor Ali\nauthor-time 1700000000\nsummary first commit\nfilename a.rs\n\tline one\n{h1} 2 2\n\tline two\n{h2} 3 3 1\nauthor Not Committed Yet\nauthor-time 1700000100\nsummary Version of a.rs from a.rs\n\tline three\n"
        );
        let b = parse_blame(&out);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].author, "Ali");
        assert_eq!(b[1].summary, "first commit");
        assert!(b[2].is_uncommitted());
        assert_eq!(relative_time(0, 3 * 86_400), "3 days ago");
        assert_eq!(relative_time(0, 3600), "1 hour ago");
    }

    #[test]
    fn side_by_side_alignment() {
        let rows = side_by_side("a\nb\nc\n", "a\nB\nB2\nc\nd\n");
        let pairs: Vec<_> = rows.iter().map(|r| (r.old, r.new, r.changed)).collect();
        assert_eq!(
            pairs,
            vec![
                (Some(0), Some(0), false),
                (Some(1), Some(1), true),
                (None, Some(2), true),
                (Some(2), Some(3), false),
                (None, Some(4), true),
            ]
        );
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
        g.commit("two").unwrap();
        g.create_branch("feature").unwrap();
        assert!(g.branches().contains(&"feature".to_string()));
        let main = g.branches().into_iter().find(|b| b != "feature").unwrap();
        g.checkout(&main).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
