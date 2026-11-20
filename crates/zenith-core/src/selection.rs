use std::ops::Range;

/// A selection in char indices. `anchor` is where it started, `head` is where
/// the caret is. When `anchor == head` it is just a cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn cursor(pos: usize) -> Self {
        Self { anchor: pos, head: pos }
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    /// Move the head; when `extend` is false the selection collapses.
    pub fn move_to(&mut self, pos: usize, extend: bool) {
        self.head = pos;
        if !extend {
            self.anchor = pos;
        }
    }

    pub fn clamp(&mut self, len: usize) {
        self.anchor = self.anchor.min(len);
        self.head = self.head.min(len);
    }
}
