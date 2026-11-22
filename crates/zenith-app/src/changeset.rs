//! Everything an AI agent changed in the workspace, across all files.
//!
//! When an agent starts, the whole working tree is recorded as a git tree
//! object (via a temporary index, so the user's staging area is untouched).
//! Later the current tree is compared with it: every added, modified and
//! deleted file can be reviewed side by side and kept or reverted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::thread;

fn git(root: &Path, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn git_str(root: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    git(root, args, index).map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

/// Record the working tree (tracked + untracked, not ignored) as a tree
/// object; returns its hash. Starts from a copy of the real index so only
/// changed files are hashed.
pub fn snapshot(root: &Path) -> Result<String, String> {
    let git_dir = PathBuf::from(git_str(root, &["rev-parse", "--absolute-git-dir"], None)?);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = std::env::temp_dir().join(format!("zenith-index-{}-{nanos}", std::process::id()));
    if git_dir.join("index").exists() {
        let _ = std::fs::copy(git_dir.join("index"), &tmp);
    }
    let r = git(root, &["add", "-A", "--", "."], Some(&tmp)).and_then(|_| git_str(root, &["write-tree"], Some(&tmp)));
    let _ = std::fs::remove_file(&tmp);
    r
}

/// One changed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub rel: String,
    /// 'A' added, 'M' modified, 'D' deleted.
    pub status: char,
    /// Blob of the new version (empty for deletions).
    pub new_blob: String,
}

/// Parse `git diff-tree -r --raw -z` output.
pub fn parse_raw(out: &[u8]) -> Vec<Change> {
    let text = String::from_utf8_lossy(out);
    let mut parts = text.split('\0').filter(|s| !s.is_empty());
    let mut changes = vec![];
    while let Some(meta) = parts.next() {
        // ":100644 100644 <old> <new> M"
        let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let Some(path) = parts.next() else { break };
        if fields.len() < 5 {
            continue;
        }
        let status = fields[4].chars().next().unwrap_or('M');
        let new_blob = if status == 'D' { String::new() } else { fields[3].to_string() };
        changes.push(Change { rel: path.to_string(), status, new_blob });
    }
    changes
}

/// Files that differ between `base` and the working tree now.
pub fn changes(root: &Path, base: &str) -> Result<Vec<Change>, String> {
    let now = snapshot(root)?;
    let out = git(root, &["diff-tree", "-r", "--raw", "-z", "--no-renames", base, &now], None)?;
    Ok(parse_raw(&out))
}

/// File content in the checkpoint (None if it didn't exist).
pub fn base_bytes(root: &Path, base: &str, rel: &str) -> Option<Vec<u8>> {
    git(root, &["cat-file", "blob", &format!("{base}:{rel}")], None).ok()
}

/// Put a file back the way it was at the checkpoint.
pub fn revert(root: &Path, base: &str, c: &Change) -> std::io::Result<()> {
    let path = root.join(&c.rel);
    match c.status {
        'A' => std::fs::remove_file(&path),
        _ => {
            let bytes = base_bytes(root, base, &c.rel)
                .ok_or_else(|| std::io::Error::other(format!("{} is not in the checkpoint", c.rel)))?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, bytes)
        }
    }
}

/// Background result of taking a checkpoint or listing changes.
pub enum Job {
    Snapshot(Receiver<Result<String, String>>),
    Changes(Receiver<Result<Vec<Change>, String>>),
}

/// Review state for one workspace.
#[derive(Default)]
pub struct ChangeSet {
    pub root: Option<PathBuf>,
    /// Tree hash of the checkpoint.
    pub base: Option<String>,
    /// Changes kept by the user: rel → blob that was accepted.
    pub kept: HashMap<String, String>,
    pub changes: Vec<Change>,
    pub selected: usize,
    pub error: Option<String>,
    job: Option<Job>,
    /// Cached diff of the selected file: (rel, blob, rows, old, new).
    pub diff: Option<(String, String, std::rc::Rc<Vec<crate::git::DiffRow>>, String, String)>,
}

impl ChangeSet {
    /// Take a new checkpoint in the background (forgets earlier changes).
    pub fn start(&mut self, root: PathBuf) {
        let (tx, rx) = channel();
        let r = root.clone();
        thread::spawn(move || {
            let _ = tx.send(snapshot(&r));
        });
        self.root = Some(root);
        self.base = None;
        self.kept.clear();
        self.changes.clear();
        self.diff = None;
        self.job = Some(Job::Snapshot(rx));
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Re-list changes in the background.
    pub fn refresh(&mut self) {
        let (Some(root), Some(base)) = (self.root.clone(), self.base.clone()) else { return };
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = channel();
        thread::spawn(move || {
            let _ = tx.send(changes(&root, &base));
        });
        self.job = Some(Job::Changes(rx));
    }

    /// Collect background results; true when something changed.
    pub fn poll(&mut self) -> bool {
        let done = match &self.job {
            Some(Job::Snapshot(rx)) => match rx.try_recv() {
                Ok(r) => {
                    match r {
                        Ok(tree) => {
                            self.base = Some(tree);
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e),
                    }
                    true
                }
                Err(_) => false,
            },
            Some(Job::Changes(rx)) => match rx.try_recv() {
                Ok(r) => {
                    match r {
                        Ok(c) => {
                            let was = self.changes.get(self.selected).map(|c| c.rel.clone());
                            self.changes =
                                c.into_iter().filter(|c| self.kept.get(&c.rel) != Some(&c.new_blob)).collect();
                            // Keep the same file selected when the list changes.
                            self.selected = was
                                .and_then(|r| self.changes.iter().position(|c| c.rel == r))
                                .unwrap_or(self.selected)
                                .min(self.changes.len().saturating_sub(1));
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e),
                    }
                    true
                }
                Err(_) => false,
            },
            None => false,
        };
        if done {
            self.job = None;
        }
        done
    }

    pub fn keep(&mut self, i: usize) {
        if i < self.changes.len() {
            let c = self.changes.remove(i);
            self.kept.insert(c.rel, c.new_blob);
            self.selected = self.selected.min(self.changes.len().saturating_sub(1));
        }
    }

    pub fn revert(&mut self, i: usize) -> Result<PathBuf, String> {
        let (Some(root), Some(base)) = (self.root.clone(), self.base.clone()) else {
            return Err("no checkpoint".into());
        };
        let c = self.changes.get(i).cloned().ok_or("no such change")?;
        revert(&root, &base, &c).map_err(|e| format!("{}: {e}", c.rel))?;
        self.changes.remove(i);
        self.kept.remove(&c.rel);
        self.selected = self.selected.min(self.changes.len().saturating_sub(1));
        Ok(root.join(&c.rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_diff() {
        let out = b":100644 100644 aaa bbb M\0src/a.rs\0:000000 100644 000 ccc A\0new.txt\0:100644 000000 ddd 000 D\0gone.rs\0";
        let c = parse_raw(out);
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].rel.as_str(), c[0].status, c[0].new_blob.as_str()), ("src/a.rs", 'M', "bbb"));
        assert_eq!(c[1].status, 'A');
        assert_eq!((c[2].status, c[2].new_blob.as_str()), ('D', ""));
    }

    #[test]
    fn snapshot_diff_and_revert() {
        let dir = std::env::temp_dir().join(format!("zenith-cs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir, &["init", "-q"], None).is_err() {
            return; // git not available
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "keep\n").unwrap();
        let base = snapshot(&dir).unwrap();
        // The user's index is untouched (nothing staged).
        assert!(git_str(&dir, &["diff", "--cached", "--name-only"], None).unwrap().is_empty());
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("c.txt"), "new\n").unwrap();
        std::fs::remove_file(dir.join("b.txt")).unwrap();
        let ch = changes(&dir, &base).unwrap();
        let st: Vec<(String, char)> = ch.iter().map(|c| (c.rel.clone(), c.status)).collect();
        assert_eq!(st, vec![("a.txt".into(), 'M'), ("b.txt".into(), 'D'), ("c.txt".into(), 'A')]);
        for c in &ch {
            revert(&dir, &base, c).unwrap();
        }
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "keep\n");
        assert!(!dir.join("c.txt").exists());
        assert!(changes(&dir, &base).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
