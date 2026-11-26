//! Terminal emulation for Zenith: spawns a process in a pseudo-terminal
//! (ConPTY on Windows, openpty on Unix) and parses its output with `vt100`.
//!
//! The UI only has to read the screen grid and send bytes back; this crate
//! does not depend on any GUI toolkit.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub use vt100::{Cell, Color, MouseProtocolEncoding, MouseProtocolMode, Screen};

/// What to run inside the terminal.
#[derive(Debug, Clone, Default)]
pub struct CommandSpec {
    /// Program to run. `None` = the user's default login shell.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    pub fn shell() -> Self {
        Self::default()
    }

    pub fn program(program: impl Into<String>, args: Vec<String>) -> Self {
        Self { program: Some(program.into()), args, ..Default::default() }
    }

    pub fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }
}

/// Collects replies to terminal queries (cursor position, device attributes)
/// that full-screen CLIs like Claude Code rely on.
#[derive(Default)]
pub struct Callbacks {
    replies: Vec<u8>,
    title: Option<String>,
    bell: bool,
}

impl vt100::Callbacks for Callbacks {
    fn audible_bell(&mut self, _: &mut Screen) {
        self.bell = true;
    }

    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_csi(&mut self, screen: &mut Screen, i1: Option<u8>, _i2: Option<u8>, params: &[&[u16]], c: char) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c) {
            // Device status report / cursor position report.
            (None, 'n') if p0 == 5 => self.replies.extend_from_slice(b"\x1b[0n"),
            (None, 'n') if p0 == 6 => {
                let (r, c) = screen.cursor_position();
                self.replies.extend_from_slice(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
            }
            // Primary device attributes: VT220 with ANSI color.
            (None, 'c') => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            // Secondary device attributes.
            (Some(b'>'), 'c') => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            // Window size in chars.
            (None, 't') if p0 == 18 => {
                let (rows, cols) = screen.size();
                self.replies.extend_from_slice(format!("\x1b[8;{rows};{cols}t").as_bytes());
            }
            _ => {}
        }
    }
}

type Parser = vt100::Parser<Callbacks>;

pub struct Terminal {
    parser: Arc<Mutex<Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    last_output: Arc<Mutex<Option<Instant>>>,
    size: (u16, u16),
    scroll_offset: usize,
    pub title: String,
}

pub const SCROLLBACK: usize = 10_000;

impl Terminal {
    /// Spawn `spec` in a new PTY. `on_output` is invoked (from a background
    /// thread) whenever new output arrives — typically to request a repaint.
    pub fn spawn(spec: &CommandSpec, rows: u16, cols: u16, on_output: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let rows = rows.max(2);
        let cols = cols.max(10);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("failed to open pseudo terminal")?;

        let mut cmd = match &spec.program {
            Some(p) => {
                let mut c = CommandBuilder::new(resolve_program(p));
                c.args(&spec.args);
                c
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "zenith");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to start `{}`", spec.program.as_deref().unwrap_or("shell")))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer()?));
        let parser =
            Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Callbacks::default())));
        let alive = Arc::new(AtomicBool::new(true));
        let last_output = Arc::new(Mutex::new(None));

        {
            let last_output = last_output.clone();
            let parser = parser.clone();
            let writer = writer.clone();
            let alive = alive.clone();
            thread::Builder::new().name("zenith-pty-reader".into()).spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let replies = {
                                let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
                                p.process(&buf[..n]);
                                std::mem::take(&mut p.callbacks_mut().replies)
                            };
                            if !replies.is_empty() {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = w.write_all(&replies);
                                    let _ = w.flush();
                                }
                            }
                            if let Ok(mut t) = last_output.lock() {
                                *t = Some(Instant::now());
                            }
                            on_output();
                        }
                    }
                }
                alive.store(false, Ordering::SeqCst);
                on_output();
            })?;
        }

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            alive,
            last_output,
            size: (rows, cols),
            scroll_offset: 0,
            title: spec.program.clone().unwrap_or_else(|| "shell".into()),
        })
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Paste text, honoring bracketed-paste mode if the app enabled it.
    pub fn paste(&mut self, text: &str) {
        self.scroll_to_bottom();
        let bracketed = self.lock().screen().bracketed_paste();
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            self.write(b"\x1b[200~");
            self.write(text.as_bytes());
            self.write(b"\x1b[201~");
        } else {
            self.write(text.as_bytes());
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(10);
        if (rows, cols) == self.size {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.lock().screen_mut().set_size(rows, cols);
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Lock the parser to read the screen.
    pub fn lock(&self) -> MutexGuard<'_, Parser> {
        self.parser.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes the title set by the program via OSC escape, if it changed.
    pub fn poll_title(&mut self) -> Option<String> {
        let t = self.lock().callbacks_mut().title.take();
        if let Some(t) = &t {
            self.title = t.clone();
        }
        t
    }

    pub fn take_bell(&self) -> bool {
        std::mem::take(&mut self.lock().callbacks_mut().bell)
    }

    /// Time since the program last printed something (`None` = never).
    pub fn idle_for(&self) -> Option<Duration> {
        self.last_output.lock().ok().and_then(|t| t.map(|t| t.elapsed()))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Scroll the view into the scrollback by `delta` lines (positive = up).
    pub fn scroll(&mut self, delta: isize) {
        let next = (self.scroll_offset as isize + delta).max(0) as usize;
        let actual = {
            let mut p = self.lock();
            p.screen_mut().set_scrollback(next);
            // vt100 clamps to the available scrollback.
            p.screen().scrollback()
        };
        self.scroll_offset = actual;
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            self.lock().screen_mut().set_scrollback(0);
        }
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Every line, oldest scrollback first, then the screen; and how many
    /// lines of scrollback there are (line `i` is at the top of the view at
    /// scroll offset `scrollback - i`).
    pub fn history(&self) -> (Vec<String>, usize) {
        let mut p = self.lock();
        let cur = p.screen().scrollback();
        p.screen_mut().set_scrollback(usize::MAX);
        let max = p.screen().scrollback();
        let (rows, cols) = p.screen().size();
        let rows = rows as usize;
        let mut lines = vec![String::new(); max + rows];
        let mut off = max;
        loop {
            p.screen_mut().set_scrollback(off);
            for (r, t) in p.screen().rows(0, cols).enumerate() {
                if let Some(l) = lines.get_mut(max - off + r) {
                    *l = t;
                }
            }
            if off == 0 {
                break;
            }
            off = off.saturating_sub(rows);
        }
        p.screen_mut().set_scrollback(cur);
        (lines, max)
    }

    /// The output as logical lines: rows the terminal wrapped because they
    /// were too long are joined again (for finding URLs in narrow panes).
    pub fn logical_lines(&self) -> Vec<String> {
        logical_lines(&mut self.lock())
    }

    /// Scroll so that history line `line` (see [`history`](Self::history))
    /// is shown, a few lines from the top.
    pub fn reveal(&mut self, line: usize, scrollback: usize) {
        let (rows, _) = self.size();
        let top = line.saturating_sub(rows as usize / 3);
        let off = scrollback.saturating_sub(top);
        self.scroll_offset = 0;
        self.scroll(off as isize);
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.kill();
    }
}

/// On Windows, programs like `claude` are often `.cmd` shims that
/// CreateProcess can't run directly; resolve them through PATH.
fn resolve_program(p: &str) -> String {
    #[cfg(windows)]
    {
        if let Ok(found) = which_like(p) {
            return found;
        }
    }
    p.to_string()
}

#[cfg(windows)]
fn which_like(p: &str) -> Result<String> {
    use std::env;
    let exts = ["", ".exe", ".cmd", ".bat", ".ps1"];
    let path = env::var_os("PATH").context("no PATH")?;
    for dir in env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{p}{ext}"));
            if cand.is_file() {
                return Ok(cand.to_string_lossy().into_owned());
            }
        }
    }
    anyhow::bail!("not found")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn runs_echo_and_captures_output() {
        let spec = CommandSpec::program("sh", vec!["-c".into(), "printf 'salom zenith'".into()]);
        let term = Terminal::spawn(&spec, 10, 40, Arc::new(|| {})).unwrap();
        let start = Instant::now();
        loop {
            let text = term.lock().screen().contents();
            if text.contains("salom zenith") {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "output not received: {text:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn answers_cursor_position_query() {
        // `read` waits for the reply to ESC[6n; if we answer, it prints OK.
        let script = r#"printf '\033[6n'; IFS= read -r -d R reply; printf 'OK%s' "${reply#*[}""#;
        let spec = CommandSpec::program("bash", vec!["-c".into(), script.into()]);
        let term = Terminal::spawn(&spec, 10, 40, Arc::new(|| {})).unwrap();
        let start = Instant::now();
        loop {
            let text = term.lock().screen().contents();
            if text.contains("OK1;1") {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no reply: {text:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A clickable thing found in terminal output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Url(String),
    /// A file path with optional 1-based line and column (`src/a.rs:12:5`).
    File {
        path: String,
        line: Option<usize>,
        col: Option<usize>,
    },
}

/// Find the link under char column `col` of a terminal row.
/// See [`Terminal::logical_lines`].
pub fn logical_lines(p: &mut Parser) -> Vec<String> {
    let cur = p.screen().scrollback();
    p.screen_mut().set_scrollback(usize::MAX);
    let max = p.screen().scrollback();
    let (rows, cols) = p.screen().size();
    let mut out: Vec<String> = vec![];
    let mut joining = false;
    // Oldest first: one screen at a time from the top of the scrollback.
    let mut off = max;
    let mut seen = 0usize;
    loop {
        p.screen_mut().set_scrollback(off);
        let first_new = (max - off).max(seen) - (max - off);
        for (r, t) in p.screen().rows(0, cols).enumerate().skip(first_new) {
            let wrapped = p.screen().row_wrapped(r as u16);
            if joining {
                if let Some(last) = out.last_mut() {
                    last.push_str(&t);
                }
            } else {
                out.push(t);
            }
            joining = wrapped;
        }
        seen = max - off + rows as usize;
        if off == 0 {
            break;
        }
        off = off.saturating_sub(rows as usize);
    }
    p.screen_mut().set_scrollback(cur);
    out
}

pub fn link_at(row: &str, col: usize) -> Option<Link> {
    let chars: Vec<char> = row.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let stop = |c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '(' | ')' | '[' | ']' | '{' | '}')
    };
    let mut s = col;
    while s > 0 && !stop(chars[s - 1]) {
        s -= 1;
    }
    let mut e = col;
    while e < chars.len() && !stop(chars[e]) {
        e += 1;
    }
    let mut token: String = chars[s..e].iter().collect();
    // Trailing punctuation is usually not part of the link.
    while token.ends_with(['.', ',', ';', ':', '!', '?']) {
        token.pop();
    }
    if token.starts_with("http://") || token.starts_with("https://") {
        return Some(Link::Url(token));
    }
    // Rust-style `--> src/main.rs:3:5` and Python-style `File "x.py", line 3` (path part only).
    let mut parts = token.rsplitn(3, ':').collect::<Vec<_>>();
    parts.reverse();
    let (path, line, col) = match parts.as_slice() {
        [p, l, c] if l.parse::<usize>().is_ok() && c.parse::<usize>().is_ok() => {
            (p.to_string(), l.parse().ok(), c.parse().ok())
        }
        [p, l, c] if c.parse::<usize>().is_ok() => (format!("{p}:{l}"), c.parse().ok(), None),
        [p, l] if l.parse::<usize>().is_ok() => (p.to_string(), l.parse().ok(), None),
        _ => (token.clone(), None, None),
    };
    // Must look like a file: has an extension or a path separator, and no URL-ish scheme.
    let looks_like_file = !path.is_empty()
        && !path.contains("://")
        && (path.contains('/')
            || path.contains('\\')
            || path.rsplit_once('.').is_some_and(|(a, ext)| {
                !a.is_empty() && !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
            }));
    looks_like_file.then_some(Link::File { path, line, col })
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn joins_wrapped_rows() {
        let mut p = vt100::Parser::new_with_callbacks(4, 20, 100, Callbacks::default());
        for i in 0..6 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        p.process(b"served at http://localhost:35617/ ok\r\nnext\r\n");
        let lines = logical_lines(&mut p);
        assert!(lines.iter().any(|l| l.trim_end() == "served at http://localhost:35617/ ok"), "{lines:?}");
        assert_eq!(lines.iter().filter(|l| l.starts_with("line ")).count(), 6);
        assert!(lines.iter().any(|l| l.trim_end() == "next"));
    }

    #[test]
    fn finds_links() {
        let row = "  --> src/main.rs:12:5 and see https://example.com/docs.";
        assert_eq!(link_at(row, 10), Some(Link::File { path: "src/main.rs".into(), line: Some(12), col: Some(5) }));
        assert_eq!(link_at(row, 40), Some(Link::Url("https://example.com/docs".into())));
        assert_eq!(link_at(row, 25), None); // "and"
        assert_eq!(
            link_at("error in app.py:7", 12),
            Some(Link::File { path: "app.py".into(), line: Some(7), col: None })
        );
        assert_eq!(
            link_at("C:\\proj\\a.rs:3:1", 3),
            Some(Link::File { path: "C:\\proj\\a.rs".into(), line: Some(3), col: Some(1) })
        );
        assert_eq!(link_at("hello world", 2), None);
        assert_eq!(
            link_at("  File \"/tmp/x.py\", line 3", 10),
            Some(Link::File { path: "/tmp/x.py".into(), line: None, col: None })
        );
    }
}
