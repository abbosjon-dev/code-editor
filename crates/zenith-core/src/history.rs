use std::time::{Duration, Instant};

use crate::Selection;

/// A single reversible edit: at char index `pos`, `removed` was replaced by `inserted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub pos: usize,
    pub removed: String,
    pub inserted: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Transaction {
    pub edits: Vec<Edit>,
    pub sel_before: Selection,
    pub sel_after: Selection,
    kind: GroupKind,
    at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupKind {
    Typing,
    Deleting,
    Other,
}

/// Undo/redo history. Consecutive typing (or deleting) within a short window
/// is merged into one transaction, like in VS Code.
#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    /// Bumped on every new transaction; lets callers detect "saved" state.
    pub(crate) seal_next: bool,
}

const GROUP_WINDOW: Duration = Duration::from_millis(800);
const MAX_UNDO: usize = 10_000;

impl History {
    pub(crate) fn record(&mut self, edit: Edit, sel_before: Selection, sel_after: Selection, kind: GroupKind) {
        self.redo.clear();
        let now = Instant::now();
        if !self.seal_next && kind != GroupKind::Other {
            if let Some(last) = self.undo.last_mut() {
                let contiguous = last.edits.last().is_some_and(|prev| match kind {
                    GroupKind::Typing => {
                        prev.pos + prev.inserted.chars().count() == edit.pos && !edit.inserted.contains('\n')
                    }
                    GroupKind::Deleting => edit.pos + edit.removed.chars().count() == prev.pos || edit.pos == prev.pos,
                    GroupKind::Other => false,
                });
                if last.kind == kind && contiguous && now.duration_since(last.at) < GROUP_WINDOW {
                    last.edits.push(edit);
                    last.sel_after = sel_after;
                    last.at = now;
                    return;
                }
            }
        }
        self.seal_next = false;
        self.undo.push(Transaction { edits: vec![edit], sel_before, sel_after, kind, at: now });
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    /// Force the next edit into a fresh transaction.
    pub fn seal(&mut self) {
        self.seal_next = true;
    }

    pub(crate) fn pop_undo(&mut self) -> Option<Transaction> {
        let t = self.undo.pop()?;
        self.redo.push(t.clone());
        self.seal_next = true;
        Some(t)
    }

    pub(crate) fn pop_redo(&mut self) -> Option<Transaction> {
        let t = self.redo.pop()?;
        self.undo.push(t.clone());
        self.seal_next = true;
        Some(t)
    }

    /// Merge the last `n` transactions into one (multi-line operations).
    pub(crate) fn merge_last(&mut self, n: usize) {
        if n < 2 || self.undo.len() < n {
            return;
        }
        let tail: Vec<Transaction> = self.undo.drain(self.undo.len() - n..).collect();
        let mut merged = tail[0].clone();
        for t in &tail[1..] {
            merged.edits.extend(t.edits.iter().cloned());
            merged.sel_after = t.sel_after;
        }
        self.undo.push(merged);
    }

    pub(crate) fn set_last_sel_after(&mut self, sel: Selection) {
        if let Some(t) = self.undo.last_mut() {
            t.sel_after = sel;
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}
