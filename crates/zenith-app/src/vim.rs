//! Vim-style modal editing on top of [`Buffer`].
//!
//! Supported: normal / insert / visual / visual-line modes; counts;
//! motions `h j k l w b e W B E 0 ^ $ gg G f F t T { } Enter Space`;
//! operators `d c y > <` with motions, doubled (`dd`) and text objects
//! (`iw aw iW aW i" a" i' a' i( a( ib i{ a{ iB i[ a[`); `x X D C s S r ~ J`,
//! `i a I A o O`, `p P`, `u Ctrl+R`, `v V`, `/ n N *`, `:w :q :wq :x :q! :N`.

use zenith_core::{Buffer, Position, Selection};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        }
    }
}

/// A key as Vim sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VKey {
    Char(char),
    Esc,
    Enter,
    Backspace,
    /// Ctrl + letter (lowercase).
    Ctrl(char),
}

/// Things the editor around the buffer must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Text yanked / deleted: also put it on the system clipboard.
    Copy(String),
    Save,
    Quit,
    SaveQuit,
    /// `/`: open the find bar (with a query for `*`).
    Find(Option<String>),
    FindNext(bool),
    Message(String),
}

#[derive(Clone, Debug, Default)]
pub struct VimState {
    pub mode: Mode,
    /// Keys of an unfinished command, e.g. `2d` or `ci`.
    pending: Vec<VKey>,
    register: String,
    linewise: bool,
    /// Visual mode: where the selection started, and the moving end.
    anchor: usize,
    head: usize,
    /// `:` command line being typed.
    pub cmdline: Option<String>,
}

impl VimState {
    /// Keys typed so far for the status bar (e.g. `2d`).
    pub fn pending_text(&self) -> String {
        self.pending
            .iter()
            .map(|k| match k {
                VKey::Char(c) => c.to_string(),
                VKey::Ctrl(c) => format!("^{}", c.to_ascii_uppercase()),
                _ => String::new(),
            })
            .collect()
    }

    /// Visual mode: the character the block caret is on.
    pub fn visual_head(&self) -> Option<usize> {
        matches!(self.mode, Mode::Visual | Mode::VisualLine).then_some(self.head)
    }

    pub fn is_insert(&self) -> bool {
        self.mode == Mode::Insert
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

// ---------------------------------------------------------------- helpers

fn ch(b: &Buffer, i: usize) -> char {
    if i < b.len_chars() {
        b.rope().char(i)
    } else {
        '\n'
    }
}

fn line_of(b: &Buffer, i: usize) -> usize {
    b.char_to_pos(i).line
}

fn line_start(b: &Buffer, line: usize) -> usize {
    b.rope().line_to_char(line.min(b.len_lines().saturating_sub(1)))
}

fn line_end(b: &Buffer, line: usize) -> usize {
    line_start(b, line) + b.line_len(line)
}

fn first_nonblank(b: &Buffer, line: usize) -> usize {
    let s = line_start(b, line);
    let e = line_end(b, line);
    (s..e).find(|&i| !matches!(ch(b, i), ' ' | '\t')).unwrap_or(e)
}

/// In normal mode the caret sits on a character, never past the line end.
fn clamp_normal(b: &Buffer, i: usize) -> usize {
    let line = line_of(b, i);
    let (s, e) = (line_start(b, line), line_end(b, line));
    i.clamp(s, if e > s { e - 1 } else { s })
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if big || c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn word_forward(b: &Buffer, mut i: usize, big: bool) -> usize {
    let len = b.len_chars();
    if i >= len {
        return len;
    }
    let c0 = class(ch(b, i), big);
    if c0 != Class::Space {
        while i < len && class(ch(b, i), big) == c0 {
            i += 1;
        }
    }
    while i < len && class(ch(b, i), big) == Class::Space {
        // An empty line counts as a word.
        if ch(b, i) == '\n' && i + 1 < len && ch(b, i + 1) == '\n' {
            return i + 1;
        }
        i += 1;
    }
    i
}

fn word_backward(b: &Buffer, mut i: usize, big: bool) -> usize {
    while i > 0 && class(ch(b, i - 1), big) == Class::Space {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let c = class(ch(b, i - 1), big);
    while i > 0 && class(ch(b, i - 1), big) == c {
        i -= 1;
    }
    i
}

fn word_end(b: &Buffer, i: usize, big: bool) -> usize {
    let len = b.len_chars();
    let mut j = i + 1;
    while j < len && class(ch(b, j), big) == Class::Space {
        j += 1;
    }
    if j >= len {
        return len.saturating_sub(1);
    }
    let c = class(ch(b, j), big);
    while j + 1 < len && class(ch(b, j + 1), big) == c {
        j += 1;
    }
    j
}

fn paragraph(b: &Buffer, line: usize, forward: bool) -> usize {
    let n = b.len_lines();
    let blank = |l: usize| b.line_text(l).trim().is_empty();
    let mut l = line;
    if forward {
        while l + 1 < n && blank(l) {
            l += 1;
        }
        while l + 1 < n && !blank(l) {
            l += 1;
        }
    } else {
        while l > 0 && blank(l) {
            l -= 1;
        }
        while l > 0 && !blank(l) {
            l -= 1;
        }
    }
    l
}

/// Find `target` on the caret's line (f/t/F/T).
fn find_in_line(b: &Buffer, i: usize, target: char, forward: bool, till: bool) -> Option<usize> {
    let line = line_of(b, i);
    let (s, e) = (line_start(b, line), line_end(b, line));
    if forward {
        let from = if till { i + 2 } else { i + 1 };
        (from..e).find(|&j| ch(b, j) == target).map(|j| if till { j - 1 } else { j })
    } else {
        let to = if till { i.saturating_sub(1) } else { i };
        (s..to).rev().find(|&j| ch(b, j) == target).map(|j| if till { j + 1 } else { j })
    }
}

fn text_object(b: &Buffer, i: usize, around: bool, obj: char) -> Option<(usize, usize)> {
    match obj {
        'w' | 'W' => {
            let big = obj == 'W';
            let len = b.len_chars();
            if len == 0 {
                return None;
            }
            let c = class(ch(b, i), big);
            let (mut s, mut e) = (i, i);
            while s > 0 && class(ch(b, s - 1), big) == c && ch(b, s - 1) != '\n' {
                s -= 1;
            }
            while e < len && class(ch(b, e), big) == c && ch(b, e) != '\n' {
                e += 1;
            }
            if around {
                let e2 = (e..len).take_while(|&j| matches!(ch(b, j), ' ' | '\t')).last().map_or(e, |j| j + 1);
                if e2 > e {
                    e = e2;
                } else {
                    while s > 0 && matches!(ch(b, s - 1), ' ' | '\t') {
                        s -= 1;
                    }
                }
            }
            Some((s, e))
        }
        '"' | '\'' | '`' => {
            let line = line_of(b, i);
            let (ls, le) = (line_start(b, line), line_end(b, line));
            let quotes: Vec<usize> = (ls..le).filter(|&j| ch(b, j) == obj).collect();
            // The pair around the caret, else the first pair after it.
            let pair = quotes
                .chunks(2)
                .filter(|p| p.len() == 2)
                .find(|p| p[0] <= i && i <= p[1])
                .or_else(|| quotes.chunks(2).filter(|p| p.len() == 2).find(|p| p[0] > i))?;
            Some(if around { (pair[0], pair[1] + 1) } else { (pair[0] + 1, pair[1]) })
        }
        '(' | ')' | 'b' | '{' | '}' | 'B' | '[' | ']' | '<' | '>' => {
            let (open, close) = match obj {
                '(' | ')' | 'b' => ('(', ')'),
                '{' | '}' | 'B' => ('{', '}'),
                '[' | ']' => ('[', ']'),
                _ => ('<', '>'),
            };
            let len = b.len_chars();
            // Opening bracket enclosing the caret.
            let mut depth = 0i32;
            let mut s = None;
            let mut j = if ch(b, i) == open { i + 1 } else { i + usize::from(ch(b, i) == close) };
            while j > 0 {
                j -= 1;
                let c = ch(b, j);
                if c == close {
                    depth += 1;
                } else if c == open {
                    if depth == 0 {
                        s = Some(j);
                        break;
                    }
                    depth -= 1;
                }
            }
            let s = s?;
            let mut depth = 0i32;
            let mut e = None;
            for k in s + 1..len {
                let c = ch(b, k);
                if c == open {
                    depth += 1;
                } else if c == close {
                    if depth == 0 {
                        e = Some(k);
                        break;
                    }
                    depth -= 1;
                }
            }
            let e = e?;
            Some(if around { (s, e + 1) } else { (s + 1, e) })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- parsing

enum Parse {
    Incomplete,
    Invalid,
    Done(Command),
}

#[derive(Debug, Clone, Copy)]
enum Target {
    Motion(char, Option<char>),
    Object(bool, char),
    /// `dd`, `yy`, `cc`, `>>`.
    Line,
}

#[derive(Debug, Clone, Copy)]
struct Command {
    count: usize,
    op: Option<char>,
    target: Option<Target>,
    /// Stand-alone command (`x`, `p`, `i`…) when there is no target.
    action: Option<(char, Option<char>)>,
}

fn parse(keys: &[VKey], visual: bool) -> Parse {
    let mut it = keys.iter().peekable();
    let mut count = 0usize;
    let read_count = |it: &mut std::iter::Peekable<std::slice::Iter<VKey>>| {
        let mut n = 0usize;
        while let Some(VKey::Char(c @ '0'..='9')) = it.peek() {
            if n == 0 && *c == '0' {
                break;
            }
            n = n * 10 + c.to_digit(10).unwrap() as usize;
            it.next();
        }
        n
    };
    count += read_count(&mut it);
    let Some(&first) = it.next() else { return Parse::Incomplete };
    let c = match first {
        VKey::Char(c) => c,
        VKey::Ctrl(c) => {
            return Parse::Done(Command {
                count: count.max(1),
                op: None,
                target: None,
                action: Some((c.to_ascii_uppercase(), None)),
            })
        }
        VKey::Enter => '\n',
        _ => return Parse::Invalid,
    };
    let is_op = matches!(c, 'd' | 'c' | 'y' | '>' | '<');
    if is_op && !visual {
        let n2 = read_count(&mut it);
        if n2 > 0 {
            count = count.max(1) * n2;
        }
        let Some(&k) = it.next() else { return Parse::Incomplete };
        let VKey::Char(m) = k else { return Parse::Invalid };
        if m == c {
            return Parse::Done(Command { count: count.max(1), op: Some(c), target: Some(Target::Line), action: None });
        }
        return match parse_target(m, &mut it, true) {
            Parse::Done(t) => Parse::Done(Command { count: count.max(1), op: Some(c), target: t.target, action: None }),
            other => other,
        };
    }
    if visual && matches!(c, 'i' | 'a') {
        return match parse_target(c, &mut it, true) {
            Parse::Done(t) => Parse::Done(Command { count: 1, op: None, target: t.target, action: None }),
            other => other,
        };
    }
    match parse_target(c, &mut it, false) {
        Parse::Done(t) => Parse::Done(Command { count: count.max(1), ..t }),
        Parse::Invalid => {
            // Not a motion: a stand-alone command, some take one more key.
            if matches!(c, 'r' | 'g' | 'z') {
                let Some(&k) = it.next() else { return Parse::Incomplete };
                let VKey::Char(arg) = k else { return Parse::Invalid };
                return Parse::Done(Command {
                    count: count.max(1),
                    op: None,
                    target: None,
                    action: Some((c, Some(arg))),
                });
            }
            Parse::Done(Command { count: count.max(1), op: None, target: None, action: Some((c, None)) })
        }
        Parse::Incomplete => Parse::Incomplete,
    }
}

fn parse_target(c: char, it: &mut std::iter::Peekable<std::slice::Iter<VKey>>, after_op: bool) -> Parse {
    let done = |t: Target| Parse::Done(Command { count: 1, op: None, target: Some(t), action: None });
    match c {
        'h' | 'j' | 'k' | 'l' | 'w' | 'b' | 'e' | 'W' | 'B' | 'E' | '0' | '^' | '$' | 'G' | '{' | '}' | '\n' | ' '
        | '_' => done(Target::Motion(c, None)),
        'f' | 'F' | 't' | 'T' => match it.next() {
            None => Parse::Incomplete,
            Some(VKey::Char(a)) => done(Target::Motion(c, Some(*a))),
            Some(_) => Parse::Invalid,
        },
        'g' => match it.peek() {
            None => Parse::Incomplete,
            Some(VKey::Char('g')) => done(Target::Motion('g', Some('g'))),
            Some(_) => Parse::Invalid,
        },
        'i' | 'a' if after_op => match it.next() {
            None => Parse::Incomplete,
            Some(VKey::Char(o)) => done(Target::Object(c == 'a', *o)),
            Some(_) => Parse::Invalid,
        },
        _ => Parse::Invalid,
    }
}

// ---------------------------------------------------------------- engine

/// Where a motion goes from `i`, and how an operator treats it.
fn motion(b: &Buffer, i: usize, m: char, arg: Option<char>, count: usize, explicit: bool) -> Option<(usize, Kind)> {
    let line = line_of(b, i);
    let last = b.len_lines().saturating_sub(1);
    let col_on = |l: usize, i: usize| {
        let col = i - line_start(b, line);
        b.pos_to_char(Position::new(l, col))
    };
    Some(match m {
        'h' => ((i.saturating_sub(count)).max(line_start(b, line)), Kind::Exclusive),
        'l' | ' ' => ((i + count).min(line_end(b, line)), Kind::Exclusive),
        'j' => (col_on((line + count).min(last), i), Kind::Linewise),
        'k' => (col_on(line.saturating_sub(count), i), Kind::Linewise),
        '\n' => (first_nonblank(b, (line + count).min(last)), Kind::Linewise),
        '_' => (first_nonblank(b, (line + count - 1).min(last)), Kind::Linewise),
        'w' | 'W' => {
            let mut j = i;
            for _ in 0..count {
                j = word_forward(b, j, m == 'W');
            }
            (j, Kind::Exclusive)
        }
        'b' | 'B' => {
            let mut j = i;
            for _ in 0..count {
                j = word_backward(b, j, m == 'B');
            }
            (j, Kind::Exclusive)
        }
        'e' | 'E' => {
            let mut j = i;
            for _ in 0..count {
                j = word_end(b, j, m == 'E');
            }
            (j, Kind::Inclusive)
        }
        '0' => (line_start(b, line), Kind::Exclusive),
        '^' => (first_nonblank(b, line), Kind::Exclusive),
        '$' => {
            let l = (line + count - 1).min(last);
            (line_end(b, l).saturating_sub(1).max(line_start(b, l)), Kind::Inclusive)
        }
        'G' => (first_nonblank(b, if explicit { count.saturating_sub(1).min(last) } else { last }), Kind::Linewise),
        'g' => (first_nonblank(b, if explicit { count.saturating_sub(1).min(last) } else { 0 }), Kind::Linewise),
        '}' | '{' => {
            let mut l = line;
            for _ in 0..count {
                l = paragraph(b, l, m == '}');
            }
            (line_start(b, l), Kind::Exclusive)
        }
        'f' | 'F' | 't' | 'T' => {
            let mut j = i;
            for _ in 0..count {
                j = find_in_line(b, j, arg?, m.is_lowercase(), m == 't' || m == 'T')?;
            }
            (j, if m.is_lowercase() { Kind::Inclusive } else { Kind::Exclusive })
        }
        _ => return None,
    })
}

pub fn feed(st: &mut VimState, b: &mut Buffer, key: VKey) -> Vec<Effect> {
    let mut fx = vec![];
    if let Some(cmd) = &mut st.cmdline {
        match key {
            VKey::Esc => st.cmdline = None,
            VKey::Backspace => {
                if cmd.pop().is_none() {
                    st.cmdline = None;
                }
            }
            VKey::Enter => {
                let c = st.cmdline.take().unwrap_or_default();
                run_ex(c.trim(), b, &mut fx);
            }
            VKey::Char(c) => cmd.push(c),
            VKey::Ctrl(_) => {}
        }
        return fx;
    }
    if st.mode == Mode::Insert {
        if matches!(key, VKey::Esc | VKey::Ctrl('[') | VKey::Ctrl('c')) {
            st.mode = Mode::Normal;
            let i = b.sel.head;
            let line = line_of(b, i);
            let to = if i > line_start(b, line) { i - 1 } else { i };
            b.set_cursor(clamp_normal(b, to), false);
        }
        return fx;
    }
    if key == VKey::Esc {
        st.pending.clear();
        if st.mode != Mode::Normal {
            st.mode = Mode::Normal;
            b.set_cursor(clamp_normal(b, st.head), false);
        }
        return fx;
    }
    if key == VKey::Backspace && st.pending.is_empty() {
        let i = b.sel.head.saturating_sub(1);
        move_to(st, b, i);
        return fx;
    }
    st.pending.push(key);
    let visual = matches!(st.mode, Mode::Visual | Mode::VisualLine);
    match parse(&st.pending, visual) {
        Parse::Incomplete => {}
        Parse::Invalid => st.pending.clear(),
        Parse::Done(c) => {
            let explicit = st.pending.iter().any(|k| matches!(k, VKey::Char('1'..='9')));
            st.pending.clear();
            if visual {
                visual_cmd(st, b, c, explicit, &mut fx);
            } else {
                normal_cmd(st, b, c, explicit, &mut fx);
            }
        }
    }
    fx
}

fn move_to(st: &mut VimState, b: &mut Buffer, i: usize) {
    match st.mode {
        Mode::Visual | Mode::VisualLine => {
            st.head = i.min(b.len_chars());
            show_visual(st, b);
        }
        _ => b.set_cursor(clamp_normal(b, i), false),
    }
}

/// Mirror the visual selection into the buffer (inclusive of the head char).
fn show_visual(st: &VimState, b: &mut Buffer) {
    let (a, h) = (st.anchor, st.head);
    b.sel = if st.mode == Mode::VisualLine {
        let (l1, l2) = (line_of(b, a.min(h)), line_of(b, a.max(h)));
        let end = if l2 + 1 < b.len_lines() { line_start(b, l2 + 1) } else { b.len_chars() };
        if h >= a {
            Selection::new(line_start(b, l1), end)
        } else {
            Selection::new(end, line_start(b, l1))
        }
    } else if h >= a {
        Selection::new(a, (h + 1).min(b.len_chars()))
    } else {
        Selection::new((a + 1).min(b.len_chars()), h)
    };
}

fn yank(st: &mut VimState, text: String, linewise: bool, fx: &mut Vec<Effect>) {
    st.register = text.clone();
    st.linewise = linewise;
    fx.push(Effect::Copy(text));
}

/// Char range covering whole lines `l1..=l2` (with a newline to remove).
fn line_range(b: &Buffer, l1: usize, l2: usize) -> (usize, usize) {
    if l2 + 1 < b.len_lines() {
        (line_start(b, l1), line_start(b, l2 + 1))
    } else if l1 > 0 {
        // Last line: take the newline before it instead.
        (line_end(b, l1 - 1), b.len_chars())
    } else {
        (0, b.len_chars())
    }
}

fn apply_op(st: &mut VimState, b: &mut Buffer, op: char, s: usize, e: usize, linewise: bool, fx: &mut Vec<Effect>) {
    let (l1, l2) = (line_of(b, s), line_of(b, e.saturating_sub(1).max(s)));
    match op {
        '>' | '<' => {
            b.sel = Selection::new(line_start(b, l1), line_end(b, l2));
            b.indent_lines(op == '>');
            b.set_cursor(first_nonblank(b, l1), false);
        }
        'y' => {
            let text = if linewise {
                let mut t: String = (l1..=l2).map(|l| b.line_text(l) + "\n").collect();
                if t.is_empty() {
                    t.push('\n');
                }
                t
            } else {
                b.rope().slice(s..e).to_string()
            };
            yank(st, text, linewise, fx);
            b.set_cursor(clamp_normal(b, if linewise { line_start(b, l1).max(s.min(b.sel.head)) } else { s }), false);
        }
        'd' | 'c' => {
            if linewise {
                let text: String = (l1..=l2).map(|l| b.line_text(l) + "\n").collect();
                yank(st, text, true, fx);
                if op == 'c' {
                    // Keep the first line's indentation, replace the rest.
                    let indent = first_nonblank(b, l1);
                    b.sel = Selection::new(indent, line_end(b, l2));
                    b.insert("");
                    st.mode = Mode::Insert;
                } else {
                    let (rs, re) = line_range(b, l1, l2);
                    b.sel = Selection::new(rs, re);
                    b.insert("");
                    let l = l1.min(b.len_lines().saturating_sub(1));
                    b.set_cursor(first_nonblank(b, l), false);
                }
            } else {
                let text = b.rope().slice(s..e).to_string();
                yank(st, text, false, fx);
                b.sel = Selection::new(s, e);
                b.insert("");
                if op == 'c' {
                    st.mode = Mode::Insert;
                } else {
                    b.set_cursor(clamp_normal(b, s), false);
                }
            }
        }
        _ => {}
    }
}

fn normal_cmd(st: &mut VimState, b: &mut Buffer, c: Command, explicit: bool, fx: &mut Vec<Effect>) {
    let i = b.sel.head;
    let n = c.count;
    if let Some(target) = c.target {
        let (s, e, linewise) = match target {
            Target::Line => {
                let l = line_of(b, i);
                let l2 = (l + n - 1).min(b.len_lines().saturating_sub(1));
                (line_start(b, l), line_end(b, l2), true)
            }
            Target::Object(around, o) => match text_object(b, i, around, o) {
                Some((s, e)) => (s, e, false),
                None => return,
            },
            Target::Motion(m, arg) => {
                // `cw` changes to the end of the word, like `ce`.
                let m = if c.op == Some('c') && matches!(m, 'w' | 'W') && !ch(b, i).is_whitespace() {
                    if m == 'w' {
                        'e'
                    } else {
                        'E'
                    }
                } else {
                    m
                };
                let Some((t, kind)) = motion(b, i, m, arg, n, explicit) else { return };
                let Some(op) = c.op else {
                    move_to(st, b, t);
                    return;
                };
                let _ = op;
                let (mut s, mut e) = (i.min(t), i.max(t));
                if kind == Kind::Inclusive {
                    e = (e + 1).min(b.len_chars());
                }
                // `dw` on the last word of a line doesn't join lines.
                if matches!(m, 'w' | 'W') && line_of(b, e) > line_of(b, s) {
                    e = line_end(b, line_of(b, s)).max(s);
                }
                if kind == Kind::Linewise {
                    s = line_start(b, line_of(b, s));
                    e = line_end(b, line_of(b, e));
                }
                (s, e, kind == Kind::Linewise)
            }
        };
        if let Some(op) = c.op {
            apply_op(st, b, op, s, e, linewise, fx);
        }
        return;
    }
    let Some((a, arg)) = c.action else { return };
    let line = line_of(b, i);
    match a {
        'i' => st.mode = Mode::Insert,
        'a' => {
            st.mode = Mode::Insert;
            b.set_cursor((i + 1).min(line_end(b, line)), false);
        }
        'I' => {
            st.mode = Mode::Insert;
            b.set_cursor(first_nonblank(b, line), false);
        }
        'A' => {
            st.mode = Mode::Insert;
            b.set_cursor(line_end(b, line), false);
        }
        'o' | 'O' => {
            st.mode = Mode::Insert;
            let indent: String = b.line_text(line).chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            if a == 'o' {
                b.set_cursor(line_end(b, line), false);
                b.insert(&format!("\n{indent}"));
            } else {
                b.set_cursor(line_start(b, line), false);
                b.insert(&format!("{indent}\n"));
                b.set_cursor(line_start(b, line) + indent.chars().count(), false);
            }
        }
        'x' | 'X' => {
            let (s, e) = if a == 'x' {
                (i, (i + n).min(line_end(b, line)))
            } else {
                ((i.saturating_sub(n)).max(line_start(b, line)), i)
            };
            if s < e {
                apply_op(st, b, 'd', s, e, false, fx);
            }
        }
        's' => {
            let e = (i + n).min(line_end(b, line));
            apply_op(st, b, 'c', i, e, false, fx);
        }
        'S' => {
            let l2 = (line + n - 1).min(b.len_lines() - 1);
            apply_op(st, b, 'c', line_start(b, line), line_end(b, l2), true, fx);
        }
        'D' | 'C' => {
            let e = line_end(b, (line + n - 1).min(b.len_lines() - 1));
            apply_op(st, b, if a == 'D' { 'd' } else { 'c' }, i, e, false, fx);
        }
        'Y' => {
            let l2 = (line + n - 1).min(b.len_lines() - 1);
            apply_op(st, b, 'y', line_start(b, line), line_end(b, l2), true, fx);
        }
        'p' | 'P' => paste(st, b, a == 'p', n),
        'u' => {
            for _ in 0..n {
                b.undo();
            }
            let h = b.sel.start();
            b.set_cursor(clamp_normal(b, h), false);
        }
        'R' => {
            // Ctrl+R
            for _ in 0..n {
                b.redo();
            }
            let h = b.sel.start();
            b.set_cursor(clamp_normal(b, h), false);
        }
        'r' => {
            let Some(r) = arg else { return };
            let e = i + n;
            if e <= line_end(b, line) {
                b.sel = Selection::new(i, e);
                b.insert(&r.to_string().repeat(n));
                b.set_cursor(e - 1, false);
            }
        }
        '~' => {
            let e = (i + n).min(line_end(b, line));
            let t: String =
                b.rope()
                    .slice(i..e)
                    .chars()
                    .map(|c| {
                        if c.is_uppercase() {
                            c.to_lowercase().next().unwrap()
                        } else {
                            c.to_uppercase().next().unwrap()
                        }
                    })
                    .collect();
            b.sel = Selection::new(i, e);
            b.insert(&t);
            b.set_cursor(clamp_normal(b, e), false);
        }
        'J' => {
            for _ in 0..n.max(2) - 1 {
                let l = line_of(b, b.sel.head);
                if l + 1 >= b.len_lines() {
                    break;
                }
                let e = line_end(b, l);
                let next = first_nonblank(b, l + 1);
                let sep = if b.line_text(l + 1).trim().is_empty() { "" } else { " " };
                b.sel = Selection::new(e, next);
                b.insert(sep);
                b.set_cursor(e, false);
            }
        }
        'v' | 'V' => {
            st.mode = if a == 'v' { Mode::Visual } else { Mode::VisualLine };
            st.anchor = i;
            st.head = i;
            show_visual(st, b);
        }
        ':' => st.cmdline = Some(String::new()),
        '/' => fx.push(Effect::Find(None)),
        'n' | 'N' => fx.push(Effect::FindNext(a == 'n')),
        '*' => {
            if let Some((s, e)) = text_object(b, i, false, 'w') {
                fx.push(Effect::Find(Some(b.rope().slice(s..e).to_string())));
            }
        }
        'g' | 'z' => {} // unknown g/z combinations are ignored
        _ => {}
    }
}

fn paste(st: &mut VimState, b: &mut Buffer, after: bool, n: usize) {
    if st.register.is_empty() {
        return;
    }
    let i = b.sel.head;
    let line = line_of(b, i);
    if st.linewise {
        let body = st.register.repeat(n);
        let at = if after { line_end(b, line) } else { line_start(b, line) };
        let text = if after { format!("\n{}", body.strip_suffix('\n').unwrap_or(&body)) } else { body.clone() };
        b.set_cursor(at, false);
        b.insert(&text);
        let target = if after { line + 1 } else { line };
        b.set_cursor(first_nonblank(b, target), false);
    } else {
        let at = if after && line_end(b, line) > i { i + 1 } else { i };
        let text = st.register.repeat(n);
        b.set_cursor(at, false);
        b.insert(&text);
        let end = at + text.chars().count();
        b.set_cursor(clamp_normal(b, end.saturating_sub(1)), false);
    }
}

fn visual_cmd(st: &mut VimState, b: &mut Buffer, c: Command, explicit: bool, fx: &mut Vec<Effect>) {
    if let Some(Target::Object(around, o)) = c.target {
        if let Some((s, e)) = text_object(b, st.head, around, o) {
            st.anchor = s;
            st.head = e.saturating_sub(1).max(s);
            show_visual(st, b);
        }
        return;
    }
    if let Some(Target::Motion(m, arg)) = c.target {
        if let Some((t, _)) = motion(b, st.head, m, arg, c.count, explicit) {
            move_to(st, b, t);
        }
        return;
    }
    let Some((a, _)) = c.action else { return };
    let line_mode = st.mode == Mode::VisualLine;
    let (s, e) = (b.sel.start(), b.sel.end());
    let exit = |st: &mut VimState| st.mode = Mode::Normal;
    match a {
        'd' | 'x' | 'X' | 'D' => {
            exit(st);
            if line_mode {
                let (l1, l2) = (line_of(b, s), line_of(b, e.saturating_sub(1).max(s)));
                apply_op(st, b, 'd', line_start(b, l1), line_end(b, l2), true, fx);
            } else {
                apply_op(st, b, 'd', s, e, false, fx);
            }
        }
        'y' | 'Y' => {
            exit(st);
            if line_mode {
                let (l1, l2) = (line_of(b, s), line_of(b, e.saturating_sub(1).max(s)));
                apply_op(st, b, 'y', line_start(b, l1), line_end(b, l2), true, fx);
            } else {
                apply_op(st, b, 'y', s, e, false, fx);
            }
            b.set_cursor(clamp_normal(b, s), false);
        }
        'c' | 's' => {
            exit(st);
            if line_mode {
                let (l1, l2) = (line_of(b, s), line_of(b, e.saturating_sub(1).max(s)));
                apply_op(st, b, 'c', line_start(b, l1), line_end(b, l2), true, fx);
            } else {
                apply_op(st, b, 'c', s, e, false, fx);
            }
        }
        '>' | '<' => {
            exit(st);
            apply_op(st, b, a, s, e.saturating_sub(1).max(s), true, fx);
        }
        '~' | 'u' | 'U' => {
            exit(st);
            let t: String = b
                .rope()
                .slice(s..e)
                .chars()
                .map(|c| match a {
                    'u' => c.to_lowercase().next().unwrap(),
                    'U' => c.to_uppercase().next().unwrap(),
                    _ if c.is_uppercase() => c.to_lowercase().next().unwrap(),
                    _ => c.to_uppercase().next().unwrap(),
                })
                .collect();
            b.sel = Selection::new(s, e);
            b.insert(&t);
            b.set_cursor(clamp_normal(b, s), false);
        }
        'p' | 'P' => {
            exit(st);
            let reg = st.register.clone();
            b.sel = Selection::new(s, e);
            b.insert(reg.strip_suffix('\n').filter(|_| !line_mode).unwrap_or(&reg));
            b.set_cursor(clamp_normal(b, s), false);
        }
        'v' | 'V' => {
            let m = if a == 'v' { Mode::Visual } else { Mode::VisualLine };
            if st.mode == m {
                exit(st);
                b.set_cursor(clamp_normal(b, st.head), false);
            } else {
                st.mode = m;
                show_visual(st, b);
            }
        }
        'o' => {
            std::mem::swap(&mut st.anchor, &mut st.head);
            show_visual(st, b);
        }
        'J' => {
            exit(st);
            let (l1, l2) = (line_of(b, s), line_of(b, e.saturating_sub(1).max(s)));
            b.set_cursor(line_start(b, l1), false);
            let mut tmp = VimState::default();
            normal_cmd(
                &mut tmp,
                b,
                Command { count: (l2 - l1 + 1).max(2), op: None, target: None, action: Some(('J', None)) },
                false,
                fx,
            );
        }
        ':' => st.cmdline = Some(String::new()),
        _ => {}
    }
}

fn run_ex(cmd: &str, b: &mut Buffer, fx: &mut Vec<Effect>) {
    match cmd {
        "" => {}
        "w" | "write" => fx.push(Effect::Save),
        "q" | "quit" | "q!" | "quit!" => fx.push(Effect::Quit),
        "wq" | "x" | "xit" => fx.push(Effect::SaveQuit),
        "noh" | "nohlsearch" => {}
        _ => {
            if let Ok(n) = cmd.parse::<usize>() {
                let l = n.saturating_sub(1).min(b.len_lines().saturating_sub(1));
                b.set_cursor(first_nonblank(b, l), false);
            } else {
                fx.push(Effect::Message(format!("Not an editor command: {cmd}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, keys: &str) -> (Buffer, VimState) {
        let mut b = Buffer::from_str(text);
        b.set_cursor(0, false);
        let mut st = VimState::default();
        for c in keys.chars() {
            let k = match c {
                '⎋' => VKey::Esc,
                '⏎' => VKey::Enter,
                '®' => VKey::Ctrl('r'),
                c => VKey::Char(c),
            };
            if st.mode == Mode::Insert && !matches!(k, VKey::Esc) {
                if let VKey::Char(c) = k {
                    b.insert(&c.to_string());
                }
                continue;
            }
            feed(&mut st, &mut b, k);
        }
        (b, st)
    }

    #[test]
    fn motions_and_operators() {
        assert_eq!(run("one two three", "dw").0.text(), "two three");
        assert_eq!(run("one two three", "wdw").0.text(), "one three");
        assert_eq!(run("one two three", "2dw").0.text(), "three");
        assert_eq!(run("one two three", "d2w").0.text(), "three");
        assert_eq!(run("one two three", "cwuno⎋").0.text(), "uno two three");
        assert_eq!(run("one two three", "de").0.text(), " two three");
        assert_eq!(run("one two three", "d$").0.text(), "");
        assert_eq!(run("one two three", "wD").0.text(), "one ");
        assert_eq!(run("one two\nlast", "wdw").0.text(), "one \nlast");
        assert_eq!(run("a(b, c)d", "fbdi(").0.text(), "a()d");
        assert_eq!(run("a(b, c)d", "fbda(").0.text(), "ad");
        assert_eq!(run("say \"hi there\" ok", "fhci\"yo⎋").0.text(), "say \"yo\" ok");
        assert_eq!(run("foo bar", "wdiw").0.text(), "foo ");
        assert_eq!(run("foo bar baz", "wdaw").0.text(), "foo baz");
        assert_eq!(run("abc", "x").0.text(), "bc");
        assert_eq!(run("abc", "2x").0.text(), "c");
        assert_eq!(run("abc", "rz").0.text(), "zbc");
        assert_eq!(run("abc", "~").0.text(), "Abc");
        assert_eq!(run("abcdef", "dtd").0.text(), "def");
        assert_eq!(run("abcdef", "dfd").0.text(), "ef");
    }

    #[test]
    fn lines() {
        assert_eq!(run("a\nb\nc", "dd").0.text(), "b\nc");
        assert_eq!(run("a\nb\nc", "jdd").0.text(), "a\nc");
        assert_eq!(run("a\nb\nc", "Gdd").0.text(), "a\nb");
        assert_eq!(run("a\nb\nc", "2dd").0.text(), "c");
        assert_eq!(run("a\nb\nc", "dj").0.text(), "c");
        assert_eq!(run("a\nb\nc", "yyp").0.text(), "a\na\nb\nc");
        assert_eq!(run("a\nb\nc", "yyjP").0.text(), "a\na\nb\nc");
        assert_eq!(run("a\nb\nc", "ddp").0.text(), "b\na\nc");
        assert_eq!(run("a\nb", "J").0.text(), "a b");
        assert_eq!(run("    x\ny", "ccz⎋").0.text(), "    z\ny");
        assert_eq!(run("a\nb\nc", "oX⎋").0.text(), "a\nX\nb\nc");
        assert_eq!(run("a\nb\nc", "jOX⎋").0.text(), "a\nX\nb\nc");
        assert_eq!(run("x", ">>").0.text(), "    x");
        assert_eq!(run("a\nb\nc\nd", "2G").0.cursor_pos().line, 1);
        assert_eq!(run("a\nb\nc\nd", "Ggg").0.cursor_pos().line, 0);
    }

    #[test]
    fn visual_undo_and_paste() {
        assert_eq!(run("hello world", "vlld").0.text(), "lo world");
        assert_eq!(run("hello world", "veyP").0.text(), "hellohello world");
        assert_eq!(run("a\nb\nc", "Vjd").0.text(), "c");
        assert_eq!(run("hello", "viwU").0.text(), "HELLO");
        assert_eq!(run("one two", "dwu").0.text(), "one two");
        assert_eq!(run("one two", "dwu®").0.text(), "two");
        let (b, st) = run("abc", "ix⎋");
        assert_eq!((b.text().as_str(), st.mode), ("xabc", Mode::Normal));
        assert_eq!(b.sel.head, 0);
        assert_eq!(run("abc", "$p").0.text(), "abc"); // empty register
        assert_eq!(run("abc", "xp").0.text(), "bac");
    }

    #[test]
    fn ex_commands() {
        let mut b = Buffer::from_str("a\nb\nc");
        let mut st = VimState::default();
        for k in [VKey::Char(':'), VKey::Char('w'), VKey::Char('q'), VKey::Enter] {
            let fx = feed(&mut st, &mut b, k);
            if k == VKey::Enter {
                assert_eq!(fx, vec![Effect::SaveQuit]);
            }
        }
        for k in [VKey::Char(':'), VKey::Char('3'), VKey::Enter] {
            feed(&mut st, &mut b, k);
        }
        assert_eq!(b.cursor_pos().line, 2);
    }
}
