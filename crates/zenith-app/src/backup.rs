//! Backups of unsaved changes ("hot exit"): every dirty document is copied
//! to `<config>/backups/` a moment after it changes, and the copies are
//! removed when it is saved, closed or Zenith exits normally. After a crash
//! (or a killed process) the next start reopens them as unsaved changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::document::Document;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Backup {
    /// The file it belongs to (`None`: an untitled document).
    pub path: Option<PathBuf>,
    pub title: String,
    pub text: String,
}

pub fn dir(config: &Path) -> PathBuf {
    config.join("backups")
}

/// A stable file name for a document's backup.
fn file_name(doc: &Document) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match doc.path() {
        Some(p) => p.hash(&mut h),
        None => (doc.title(), doc.id).hash(&mut h),
    }
    format!("{:016x}.json", h.finish())
}

/// Keeps the backup folder in step with the open documents.
#[derive(Default)]
pub struct Backups {
    /// Document id → (version backed up, backup file).
    written: HashMap<u64, (u64, PathBuf)>,
}

impl Backups {
    /// Back up documents that changed since the last call; remove backups
    /// of documents that are clean or closed. Writing happens on a thread
    /// (the rope is cloned, which is cheap).
    pub fn sync(&mut self, docs: &[Document], dir: &Path) {
        let mut keep = std::collections::HashSet::new();
        for d in docs {
            if !d.buffer.is_dirty() {
                continue;
            }
            keep.insert(d.id);
            let v = d.buffer.version();
            if self.written.get(&d.id).is_some_and(|(wv, _)| *wv == v) {
                continue;
            }
            let file = dir.join(file_name(d));
            self.written.insert(d.id, (v, file.clone()));
            let (path, title, rope) = (d.path().map(Path::to_path_buf), d.title(), d.buffer.rope().clone());
            let dir = dir.to_path_buf();
            std::thread::spawn(move || {
                let b = Backup { path, title, text: rope.to_string() };
                let Ok(json) = serde_json::to_vec(&b) else { return };
                let tmp = file.with_extension("tmp");
                if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&tmp, json).is_ok() {
                    let _ = std::fs::rename(&tmp, &file);
                }
            });
        }
        let gone: Vec<u64> = self.written.keys().filter(|id| !keep.contains(id)).copied().collect();
        for id in gone {
            if let Some((_, file)) = self.written.remove(&id) {
                let _ = std::fs::remove_file(file);
            }
        }
    }
}

/// Backups left by a session that didn't exit normally.
pub fn load_all(dir: &Path) -> Vec<Backup> {
    let mut out: Vec<Backup> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

/// Remove every backup (a normal exit).
pub fn clear(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_up_dirty_documents_only() {
        let root = std::env::temp_dir().join(format!("zenith-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d = dir(&root);
        let theme = crate::highlight::load_theme("OneHalfDark");
        let mut a = Document::new_untitled(1, 1, theme.clone());
        let b = Document::new_untitled(2, 2, theme);
        a.buffer.insert("unsaved work");
        let mut bk = Backups::default();
        let mut docs = vec![a, b];
        bk.sync(&docs, &d);
        let wait = |n: usize| {
            for _ in 0..200 {
                if load_all(&d).len() == n {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait(1);
        let all = load_all(&d);
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].title.as_str(), all[0].text.as_str()), ("Untitled-1", "unsaved work"));
        // Saved (clean) → its backup goes.
        docs[0].buffer.mark_clean();
        bk.sync(&docs, &d);
        assert!(load_all(&d).is_empty());
        clear(&d);
        let _ = std::fs::remove_dir_all(&root);
    }
}
