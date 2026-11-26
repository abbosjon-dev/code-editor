//! Renders a [`zenith_terminal::Terminal`] and forwards keyboard / mouse input.

use std::sync::Arc;

use eframe::egui::{
    self, text::LayoutJob, Color32, CursorIcon, Event, FontFamily, FontId, Key, Rect, Sense, Stroke, StrokeKind,
    TextFormat,
};
use zenith_terminal::{Color, CommandSpec, MouseProtocolEncoding, MouseProtocolMode, Terminal};

use crate::theme::{Palette, MONO_BOLD};

#[derive(Clone, Debug, PartialEq)]
pub enum TermKind {
    Shell,
    Agent { id: String, name: String, icon: String },
}

pub struct TermTab {
    pub id: u64,
    pub term: Option<Terminal>,
    pub kind: TermKind,
    pub spec: CommandSpec,
    pub error: Option<String>,
    selection: Option<((u16, u16), (u16, u16))>,
    scroll_accum: f32,
    /// Set when started with resume arguments: (start time, args to drop if it exits right away).
    pub resumed: Option<(std::time::Instant, Vec<String>)>,
    /// Ctrl+F: search in the output and scrollback.
    find: Option<TermFind>,
}

/// The terminal's search bar.
#[derive(Default)]
struct TermFind {
    query: String,
    /// Searched for this query (redone when it changes).
    searched: Option<String>,
    /// Matches: (history line, first column, length in chars).
    matches: Vec<(usize, usize, usize)>,
    current: usize,
    /// Scrollback lines when searched (history line numbering).
    scrollback: usize,
    focus: bool,
}

/// Case-insensitive matches of `q` in `lines`: (line, column, length), in chars.
fn find_in_lines(lines: &[String], q: &str) -> Vec<(usize, usize, usize)> {
    let q: Vec<char> = q.to_lowercase().chars().collect();
    if q.is_empty() {
        return vec![];
    }
    let mut out = vec![];
    for (i, l) in lines.iter().enumerate() {
        let hay: Vec<char> = l.chars().flat_map(char::to_lowercase).collect();
        if hay.len() != l.chars().count() {
            continue; // lowercasing changed the length: skip rather than misplace
        }
        let mut c = 0;
        while c + q.len() <= hay.len() {
            if hay[c..c + q.len()] == q[..] {
                out.push((i, c, q.len()));
                c += q.len();
            } else {
                c += 1;
            }
        }
    }
    out
}

impl TermTab {
    pub fn spawn(id: u64, kind: TermKind, spec: CommandSpec, waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut t = Self {
            id,
            term: None,
            kind,
            spec,
            error: None,
            selection: None,
            scroll_accum: 0.0,
            resumed: None,
            find: None,
        };
        t.restart(waker);
        t
    }

    pub fn restart(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.term = None;
        let (rows, cols) = (24, 80);
        match Terminal::spawn(&self.spec, rows, cols, waker) {
            Ok(t) => {
                self.term = Some(t);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    pub fn title(&self) -> String {
        match &self.kind {
            TermKind::Agent { name, icon, .. } => format!("{icon} {name}"),
            TermKind::Shell => {
                let t = self.term.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                let short = t.rsplit(['/', '\\']).next().unwrap_or("").trim().to_string();
                if short.is_empty() || short == "shell" {
                    "Terminal".into()
                } else {
                    short
                }
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        self.term.as_ref().is_some_and(|t| t.is_alive())
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(t) = &mut self.term {
            t.paste(text);
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Some(t) = &mut self.term {
            t.write(bytes);
        }
    }
}

pub enum TermEvent {
    Restart,
    /// Ctrl/Cmd+click on a file path or URL in the output.
    OpenLink(zenith_terminal::Link),
}

fn color(c: Color, p: &Palette, default: Color32) -> Color32 {
    match c {
        Color::Default => default,
        Color::Idx(i) => p.ansi_256(i),
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

pub fn terminal_ui(
    ui: &mut egui::Ui,
    tab: &mut TermTab,
    p: &Palette,
    font_size: f32,
    focus_request: bool,
) -> Option<TermEvent> {
    let rect = ui.available_rect_before_wrap();
    let id = egui::Id::new(("zenith-term", tab.id));
    let resp = ui.interact(rect, id, Sense::click_and_drag()).on_hover_cursor(CursorIcon::Text);
    ui.allocate_rect(rect, Sense::hover());
    let painter = ui.painter_at(rect);
    let bg = p.bg_panel;
    painter.rect_filled(rect, 0.0, bg);

    if focus_request || resp.clicked() || resp.drag_started() {
        ui.memory_mut(|m| m.request_focus(id));
    }

    let Some(term) = tab.term.as_mut() else {
        let msg = tab.error.clone().unwrap_or_else(|| "Terminal not started".into());
        painter.text(
            rect.min + egui::vec2(12.0, 12.0),
            egui::Align2::LEFT_TOP,
            msg,
            FontId::monospace(font_size),
            p.error,
        );
        return None;
    };

    let font = FontId::monospace(font_size);
    let bold_font = FontId::new(font_size, FontFamily::Name(MONO_BOLD.into()));
    let (char_w, row_h) = crate::theme::mono_metrics(ui, &font);
    let line_h = (font_size * 1.3).round();
    let pad = egui::vec2(10.0, 6.0);
    let inner = rect.shrink2(pad);
    let rows = ((inner.height() / line_h).floor() as u16).max(2);
    let cols = ((inner.width() / char_w).floor() as u16).max(10);
    term.resize(rows, cols);
    term.poll_title();

    let has_focus = ui.memory(|m| m.has_focus(id));
    let cell_at = |pos: egui::Pos2| -> (u16, u16) {
        let r = ((pos.y - inner.min.y) / line_h).floor().clamp(0.0, rows as f32 - 1.0) as u16;
        let c = ((pos.x - inner.min.x) / char_w).round().clamp(0.0, cols as f32) as u16;
        (r, c)
    };

    // --- mouse selection ---
    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let c = cell_at(pos);
            tab.selection = Some((c, c));
        }
    } else if resp.dragged() {
        if let (Some(pos), Some(sel)) = (resp.interact_pointer_pos(), tab.selection.as_mut()) {
            sel.1 = cell_at(pos);
        }
    } else if resp.clicked() {
        tab.selection = None;
    }

    // --- scroll wheel ---
    let mut event = None;

    // --- Ctrl/Cmd+click links (file:line:col, URLs) ---
    let link_mod = ui.input(|i| i.modifiers.command);
    if link_mod {
        if let Some(pos) = resp.hover_pos() {
            let (r, c) = cell_at(pos);
            let row = term.lock().screen().contents_between(r, 0, r, cols);
            if let Some(link) = zenith_terminal::link_at(&row, c as usize) {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                if resp.clicked() {
                    event = Some(TermEvent::OpenLink(link));
                }
            }
        }
    }
    if resp.hovered() {
        let dy = ui.input(|i| i.raw_scroll_delta.y);
        if dy != 0.0 {
            tab.scroll_accum += dy / line_h;
            let lines = tab.scroll_accum.trunc() as isize;
            tab.scroll_accum -= lines as f32;
            if lines != 0 {
                let (alt, mode, enc) = {
                    let p = term.lock();
                    let s = p.screen();
                    (s.alternate_screen(), s.mouse_protocol_mode(), s.mouse_protocol_encoding())
                };
                if mode != MouseProtocolMode::None && enc == MouseProtocolEncoding::Sgr {
                    let (r, c) = ui.input(|i| i.pointer.hover_pos()).map(cell_at).unwrap_or((0, 0));
                    let btn = if lines > 0 { 64 } else { 65 };
                    for _ in 0..lines.unsigned_abs() {
                        term.write(format!("\x1b[<{btn};{};{}M", c + 1, r + 1).as_bytes());
                    }
                } else if alt {
                    let seq: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
                    for _ in 0..lines.unsigned_abs() {
                        term.write(seq);
                    }
                } else {
                    term.scroll(lines);
                    tab.selection = None;
                }
            }
        }
    }

    // --- keyboard ---
    if has_focus {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
        let (app_cursor, alive) = (term.lock().screen().application_cursor(), term.is_alive());
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                Event::Text(t) => {
                    term.scroll_to_bottom();
                    term.write(t.as_bytes());
                }
                Event::Paste(t) => term.paste(&t),
                Event::Copy => {
                    if let Some(text) = selected_text(term, tab.selection) {
                        ui.ctx().copy_text(text);
                        tab.selection = None;
                    } else {
                        term.write(b"\x03");
                    }
                }
                Event::Cut => term.write(b"\x18"),
                Event::Ime(egui::ImeEvent::Commit(t)) => term.write(t.as_bytes()),
                Event::Key { key, pressed: true, modifiers, .. } => {
                    if !alive && key == Key::Enter {
                        event = Some(TermEvent::Restart);
                        continue;
                    }
                    // Ctrl+F: search the output (as in VS Code; the shell doesn't get it).
                    if modifiers.command && !modifiers.shift && !modifiers.alt && key == Key::F {
                        tab.find = Some(TermFind { focus: true, ..Default::default() });
                        continue;
                    }
                    // Ctrl+Shift+C / Cmd+C copy is handled via Event::Copy.
                    if modifiers.ctrl && modifiers.shift && key == Key::C {
                        if let Some(text) = selected_text(term, tab.selection) {
                            ui.ctx().copy_text(text);
                        }
                        continue;
                    }
                    if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                        term.scroll_to_bottom();
                        tab.selection = None;
                        term.write(&bytes);
                    }
                }
                _ => {}
            }
        }
    }

    // --- paint ---
    let parser = term.lock();
    let screen = parser.screen();
    let default_fg = p.text;
    let sel = tab.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    let in_sel = |r: u16, c: u16| sel.is_some_and(|(a, b)| (r, c) >= a && (r, c) < b);

    for r in 0..rows {
        let y = inner.min.y + r as f32 * line_h;
        let mut run = String::new();
        let mut run_start = 0u16;
        let mut run_fmt: Option<(Color32, bool, bool)> = None;
        let flush = |run: &mut String, start: u16, fmt: Option<(Color32, bool, bool)>| {
            if run.is_empty() {
                return;
            }
            let (fg, bold, underline) = fmt.unwrap();
            let mut job = LayoutJob::default();
            job.append(
                run,
                0.0,
                TextFormat {
                    font_id: if bold { bold_font.clone() } else { font.clone() },
                    color: fg,
                    underline: if underline { Stroke::new(1.0_f32, fg) } else { Stroke::NONE },
                    ..Default::default()
                },
            );
            let galley = ui.fonts(|f| f.layout_job(job));
            painter.galley(egui::pos2(inner.min.x + start as f32 * char_w, y + (line_h - row_h) / 2.0), galley, fg);
            run.clear();
        };
        let mut c = 0u16;
        while c < cols {
            let Some(cell) = screen.cell(r, c) else {
                c += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                c += 1;
                continue;
            }
            let width = if cell.is_wide() { 2 } else { 1 };
            let mut fg = color(cell.fgcolor(), p, default_fg);
            let mut cbg = color(cell.bgcolor(), p, bg);
            if cell.bold() {
                if let Color::Idx(i) = cell.fgcolor() {
                    if i < 8 {
                        fg = p.ansi[i as usize + 8];
                    }
                }
            }
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut cbg);
            }
            if cell.dim() {
                fg = fg.gamma_multiply(0.6);
            }
            if in_sel(r, c) {
                cbg = p.selection;
            }
            if cbg != bg {
                let br = Rect::from_min_size(
                    egui::pos2(inner.min.x + c as f32 * char_w, y),
                    egui::vec2(char_w * width as f32, line_h),
                );
                painter.rect_filled(br.expand2(egui::vec2(0.3, 0.0)), 0.0, cbg);
            }
            let contents = cell.contents();
            let text = if contents.is_empty() { " " } else { contents };
            let fmt = Some((fg, cell.bold(), cell.underline()));
            if width == 2 || fmt != run_fmt {
                flush(&mut run, run_start, run_fmt);
                run_start = c;
                run_fmt = fmt;
            }
            run.push_str(text);
            if width == 2 {
                flush(&mut run, run_start, run_fmt);
                run_start = c + 2;
                run_fmt = None;
            }
            c += width;
        }
        flush(&mut run, run_start, run_fmt);
    }

    if !screen.hide_cursor() && term.scroll_offset() == 0 && term.is_alive() {
        let (cr, cc) = screen.cursor_position();
        let cur = Rect::from_min_size(
            egui::pos2(inner.min.x + cc as f32 * char_w, inner.min.y + cr as f32 * line_h),
            egui::vec2(char_w, line_h),
        );
        if has_focus {
            painter.rect_filled(cur, 1.0, p.cursor.gamma_multiply(0.75));
        } else {
            painter.rect_stroke(cur, 1.0, Stroke::new(1.0_f32, p.cursor), StrokeKind::Inside);
        }
        if has_focus {
            ui.output_mut(|o| o.ime = Some(egui::output::IMEOutput { rect, cursor_rect: cur }));
        }
    }
    let offset = term.scroll_offset();
    let alive = term.is_alive();
    drop(parser);

    // --- search bar and its matches ---
    let mut refocus = false;
    if let Some(f) = &mut tab.find {
        let mut close = false;
        let mut step: isize = 0;
        let bar = egui::Area::new(id.with("find"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.right_top() + egui::vec2(-330.0, 6.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).fill(p.bg_elevated).inner_margin(6).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut f.query).hint_text("Find in terminal").desired_width(170.0),
                        );
                        if f.focus {
                            r.request_focus();
                            f.focus = false;
                        }
                        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            step = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                            r.request_focus();
                        }
                        if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        let n = f.matches.len();
                        ui.label(count_label(n, f.current, &f.query).color(if n == 0 && !f.query.is_empty() {
                            p.error
                        } else {
                            p.text_dim
                        }));
                        if ui.small_button("↑").on_hover_text("Previous (Shift+Enter)").clicked() {
                            step = -1;
                        }
                        if ui.small_button("↓").on_hover_text("Next (Enter)").clicked() {
                            step = 1;
                        }
                        if ui.small_button("✕").clicked() {
                            close = true;
                        }
                    });
                });
            });
        let _ = bar;
        // Search again when the query changed (newest match first).
        if f.searched.as_deref() != Some(f.query.as_str()) {
            let (lines, sb) = term.history();
            f.matches = find_in_lines(&lines, &f.query);
            f.scrollback = sb;
            f.searched = Some(f.query.clone());
            f.current = f.matches.len().saturating_sub(1);
            if let Some(&(l, _, _)) = f.matches.get(f.current) {
                term.reveal(l, sb);
            }
        }
        if step != 0 && !f.matches.is_empty() {
            let n = f.matches.len() as isize;
            f.current = ((f.current as isize + step).rem_euclid(n)) as usize;
            term.reveal(f.matches[f.current].0, f.scrollback);
        }
        // Highlight the matches on screen.
        let top = f.scrollback.saturating_sub(term.scroll_offset());
        for (k, &(l, c, len)) in f.matches.iter().enumerate() {
            if l < top || l >= top + rows as usize {
                continue;
            }
            let r = Rect::from_min_size(
                egui::pos2(inner.min.x + c as f32 * char_w, inner.min.y + (l - top) as f32 * line_h),
                egui::vec2(len as f32 * char_w, line_h),
            );
            if k == f.current {
                painter.rect_filled(r, 2.0, p.find_match);
                painter.rect_stroke(r, 2.0, Stroke::new(1.5_f32, p.warning), StrokeKind::Outside);
            } else {
                painter.rect_filled(r, 2.0, p.find_match.gamma_multiply(0.5));
            }
        }
        if close {
            refocus = true;
        }
    }
    if refocus {
        tab.find = None;
        term.scroll_to_bottom();
        ui.memory_mut(|m| m.request_focus(id));
    }

    if offset > 0 {
        painter.text(
            rect.right_top() + egui::vec2(-12.0, 8.0),
            egui::Align2::RIGHT_TOP,
            format!("↑ {offset} lines"),
            FontId::proportional(11.0),
            p.text_dim,
        );
    }
    if !alive {
        painter.text(
            rect.center_bottom() - egui::vec2(0.0, 18.0),
            egui::Align2::CENTER_CENTER,
            "Process exited — press Enter to restart",
            FontId::proportional(12.0),
            p.warning,
        );
    }
    event
}

fn selected_text(term: &Terminal, sel: Option<((u16, u16), (u16, u16))>) -> Option<String> {
    let (a, b) = sel?;
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    if a == b {
        return None;
    }
    let text = term.lock().screen().contents_between(a.0, a.1, b.0, b.1);
    (!text.is_empty()).then_some(text)
}

/// Translate a key press to the bytes a terminal would send.
fn key_bytes(key: Key, m: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let csi = |s: &str| format!("\x1b[{s}").into_bytes();
    let arrow = |c: char| {
        let mod_code = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.ctrl as u8);
        if mod_code > 1 {
            format!("\x1b[1;{mod_code}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    // Ctrl + letter → control code.
    if m.ctrl && !m.alt {
        let name = key.name();
        if name.len() == 1 {
            let ch = name.chars().next().unwrap().to_ascii_lowercase();
            if ch.is_ascii_lowercase() {
                return Some(vec![(ch as u8) & 0x1f]);
            }
        }
        match key {
            Key::OpenBracket => return Some(vec![0x1b]),
            Key::Backslash => return Some(vec![0x1c]),
            Key::CloseBracket => return Some(vec![0x1d]),
            Key::Space => return Some(vec![0x00]),
            _ => {}
        }
    }
    // Alt + letter → ESC prefix (Meta).
    if m.alt && !m.ctrl && !cfg!(target_os = "macos") {
        let name = key.name();
        if name.len() == 1 {
            let ch = name.chars().next().unwrap();
            let ch = if m.shift { ch.to_ascii_uppercase() } else { ch.to_ascii_lowercase() };
            return Some(vec![0x1b, ch as u8]);
        }
    }
    Some(match key {
        Key::Enter if m.shift => b"\x1b\r".to_vec(), // newline in many agent CLIs
        Key::Enter => b"\r".to_vec(),
        Key::Backspace if m.alt || m.ctrl => vec![0x1b, 0x7f],
        Key::Backspace => vec![0x7f],
        Key::Tab if m.shift => csi("Z"),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => vec![0x1b],
        Key::ArrowUp => arrow('A'),
        Key::ArrowDown => arrow('B'),
        Key::ArrowRight => arrow('C'),
        Key::ArrowLeft => arrow('D'),
        Key::Home => arrow('H'),
        Key::End => arrow('F'),
        Key::PageUp => csi("5~"),
        Key::PageDown => csi("6~"),
        Key::Insert => csi("2~"),
        Key::Delete => csi("3~"),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => csi("15~"),
        Key::F6 => csi("17~"),
        Key::F7 => csi("18~"),
        Key::F8 => csi("19~"),
        Key::F9 => csi("20~"),
        Key::F10 => csi("21~"),
        Key::F11 => csi("23~"),
        Key::F12 => csi("24~"),
        _ => return None,
    })
}

/// The "3 of 12" label of the search bar.
fn count_label(n: usize, current: usize, query: &str) -> egui::RichText {
    let t = if query.is_empty() {
        String::new()
    } else if n == 0 {
        "No results".into()
    } else {
        format!("{} of {n}", current + 1)
    };
    egui::RichText::new(t).small()
}

#[cfg(test)]
mod find_tests {
    use super::*;

    #[test]
    fn finds_case_insensitively() {
        let lines = vec!["error: boom".to_string(), "ok".into(), "Error again, ERROR".into()];
        assert_eq!(find_in_lines(&lines, "error"), [(0, 0, 5), (2, 0, 5), (2, 13, 5)]);
        assert!(find_in_lines(&lines, "").is_empty());
    }
}
