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
    pub text: String,
}

pub struct SearchJob {
    rx: Receiver<Vec<SearchHit>>,
    cancel: Arc<AtomicBool>,
    pub done: bool,
}

pub const MAX_HITS: usize = 5_000;

impl SearchJob {
    pub fn start(root: &Path, query: &str, case_sensitive: bool) -> Self {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let root = root.to_path_buf();
        let q = if case_sensitive { query.to_string() } else { query.to_lowercase() };
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
                let mut hits = vec![];
                for (i, line) in text.lines().enumerate() {
                    let found = if case_sensitive { line.contains(&q) } else { line.to_lowercase().contains(&q) };
                    if found {
                        let rel = e.path().strip_prefix(&root).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
                        let trimmed: String = line.trim().chars().take(200).collect();
                        hits.push(SearchHit { path: e.path().to_path_buf(), rel, line: i, text: trimmed });
                        total += 1;
                        if total >= MAX_HITS {
                            break;
                        }
                    }
                }
                if !hits.is_empty() && tx.send(hits).is_err() {
                    break;
                }
            }
        });
        Self { rx, cancel, done: false }
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

/// Current git branch, read straight from `.git/HEAD` (no git binary needed).
pub fn git_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(b) => b.to_string(),
        None => head.chars().take(7).collect(),
    })
}
