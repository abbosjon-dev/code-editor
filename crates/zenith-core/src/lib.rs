//! Zenith core: UI-agnostic text editing primitives.
//!
//! Everything here is pure logic (no rendering, no IO except file load/save),
//! so it is easy to test and reuse from plugins or a future headless mode.

mod buffer;
mod history;
pub mod pathmap;
mod search;
mod selection;

pub use buffer::{Buffer, LineEnding};
pub use history::{Edit, History};
pub use ropey::Rope;
pub use search::{find_all, find_limited, SearchOptions};
pub use selection::Selection;

/// A (line, column) position, both zero-based, column counted in chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Character classes used for word-wise movement / double-click selection.
#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum CharClass {
    Whitespace,
    Word,
    Punct,
}

pub(crate) fn char_class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}
