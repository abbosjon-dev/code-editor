//! The code editor widget: custom-painted, virtualized (only visible lines are
//! laid out), with gutter, selections, find highlights, indent guides,
//! bracket matching and a minimap.

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, CursorIcon, Event, FontFamily, FontId, Key, Modifiers, Pos2, Rect, Sense,
    Stroke, StrokeKind, TextFormat, UiBuilder,
};
use unicode_width::UnicodeWidthChar;
use zenith_core::{Position, Selection};

use crate::document::Document;
use crate::settings::Settings;
use crate::theme::{Palette, MONO_BOLD, MONO_ITALIC};

/// Things the editor asks the app to do.
pub enum EditorAction {
    AskAgent { question: String },
    SendSelectionToAgent,
}

struct Metrics {
    font: FontId,
    char_w: f32,
    line_h: f32,
    row_h: f32,
    tab: usize,
}

/// Visual column offsets for each char boundary of a line (tabs expanded,
/// wide chars counted as 2).
fn visual_cols(chars: &[char], tab: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(chars.len() + 1);
    let mut v = 0;
    out.push(0);
    for &c in chars {
        v += if c == '\t' { tab - v % tab } else { c.width().unwrap_or(1).max(1) };
        out.push(v);
    }
    out
}

fn line_chars(doc: &Document, line: usize) -> Vec<char> {
    let rope = doc.buffer.rope();
    if line >= rope.len_lines() {
        return vec![];
    }
    let n = doc.buffer.line_len(line);
    rope.line(line).chars().take(n).collect()
}

const MINIMAP_W: f32 = 84.0;
const TEXT_PAD: f32 = 10.0;

pub fn editor_ui(
    ui: &mut egui::Ui,
    doc: &mut Document,
    p: &Palette,
    s: &Settings,
    cursor_epoch: f64,
) -> Vec<EditorAction> {
    let mut actions = vec![];
    doc.sync();

    if doc.find.open {
        find_bar(ui, doc, p);
    }

    let font = FontId::monospace(s.editor_font_size);
    let (char_w, row_h) = crate::theme::mono_metrics(ui, &font);
    let m = Metrics {
        font,
        char_w,
        line_h: (s.editor_font_size * s.line_height).round(),
        row_h,
        tab: doc.buffer.tab_size.max(1),
    };

    let total_lines = doc.buffer.len_lines();
    let digits = total_lines.to_string().len().max(3);
    let gutter_w = digits as f32 * m.char_w + 28.0;
    let longest = doc.longest_line_chars();

    let full = ui.available_rect_before_wrap();
    let show_minimap = s.show_minimap && full.width() > 500.0;
    let editor_rect =
        if show_minimap { Rect::from_min_max(full.min, egui::pos2(full.max.x - MINIMAP_W, full.max.y)) } else { full };
    let viewport_h = editor_rect.height();

    let id = ui.make_persistent_id(("zenith-editor", doc.id));
    let has_focus = ui.memory(|mem| mem.has_focus(id));

    // --- keyboard ---
    let mut moved = false;
    if has_focus {
        ui.memory_mut(|mem| {
            mem.set_focus_lock_filter(
                id,
                egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false },
            )
        });
        let page = (viewport_h / m.line_h).max(1.0) as isize - 1;
        moved = handle_keys(ui, doc, page);
    }

    let mut scroll = egui::ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded);
    if let Some(line) = doc.reveal_line.take() {
        let y = (line as f32 * m.line_h - viewport_h * 0.35).max(0.0);
        scroll = scroll.vertical_scroll_offset(y);
    }
    if let Some(y) = ui.data_mut(|d| d.remove_temp::<f32>(id.with("minimap-jump"))) {
        scroll = scroll.vertical_scroll_offset(y);
    }

    let content_w = gutter_w + TEXT_PAD + (longest as f32 + 8.0) * m.char_w;
    let content_h = total_lines as f32 * m.line_h + viewport_h * 0.6;

    let out = ui
        .scope_builder(UiBuilder::new().max_rect(editor_rect), |ui| {
            scroll.show_viewport(ui, |ui, viewport| {
                let size = egui::vec2(content_w.max(ui.available_width()), content_h);
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                let resp = ui.interact(rect, id, Sense::click_and_drag()).on_hover_cursor(CursorIcon::Text);
                let clip = ui.clip_rect();
                let text_x = rect.min.x + gutter_w + TEXT_PAD;

                // --- mouse ---
                if let Some(pos) = resp.interact_pointer_pos() {
                    let in_gutter = pos.x < clip.min.x + gutter_w;
                    let hit = |doc: &Document, pos: Pos2| -> usize {
                        let line = (((pos.y - rect.min.y) / m.line_h).floor().max(0.0) as usize)
                            .min(doc.buffer.len_lines().saturating_sub(1));
                        let chars = line_chars(doc, line);
                        let vc = visual_cols(&chars, m.tab);
                        let target = (pos.x - text_x) / m.char_w;
                        let mut col = vc.len() - 1;
                        for i in 0..vc.len() - 1 {
                            let mid = (vc[i] + vc[i + 1]) as f32 / 2.0;
                            if target < mid {
                                col = i;
                                break;
                            }
                        }
                        doc.buffer.pos_to_char(Position::new(line, col))
                    };
                    let shift = ui.input(|i| i.modifiers.shift);
                    if resp.triple_clicked() {
                        let line = doc.buffer.cursor_pos().line;
                        doc.buffer.select_line(line);
                    } else if resp.double_clicked() {
                        let idx = hit(doc, pos);
                        doc.buffer.select_word_at(idx);
                    } else if ui.input(|i| i.pointer.primary_pressed()) && resp.hovered() {
                        let idx = hit(doc, pos);
                        if in_gutter && !shift {
                            let line = doc.buffer.char_to_pos(idx).line;
                            doc.buffer.select_line(line);
                        } else {
                            doc.buffer.set_cursor(idx, shift);
                        }
                        ui.memory_mut(|mem| mem.request_focus(id));
                    } else if resp.dragged_by(egui::PointerButton::Primary) {
                        let idx = hit(doc, pos);
                        if in_gutter {
                            let line = doc.buffer.char_to_pos(idx).line;
                            let anchor_line = doc.buffer.char_to_pos(doc.buffer.sel.anchor).line;
                            let end = if line + 1 < doc.buffer.len_lines() {
                                doc.buffer.pos_to_char(Position::new(line + 1, 0))
                            } else {
                                doc.buffer.len_chars()
                            };
                            if line >= anchor_line {
                                doc.buffer.sel.head = end;
                            } else {
                                doc.buffer.sel.head = doc.buffer.pos_to_char(Position::new(line, 0));
                            }
                        } else {
                            doc.buffer.sel.head = idx;
                        }
                        doc.scroll_to_cursor = true;
                    }
                }
                if resp.secondary_clicked() {
                    ui.memory_mut(|mem| mem.request_focus(id));
                }
                resp.context_menu(|ui| {
                    let has_sel = !doc.buffer.sel.is_empty();
                    if ui.add_enabled(has_sel, egui::Button::new("Cut")).clicked() {
                        ui.ctx().copy_text(doc.buffer.selected_text());
                        doc.buffer.backspace();
                        ui.close_menu();
                    }
                    if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                        ui.ctx().copy_text(doc.buffer.selected_text());
                        ui.close_menu();
                    }
                    if ui.button("Select All").clicked() {
                        doc.buffer.select_all();
                        ui.close_menu();
                    }
                    if ui.button("Toggle Comment").clicked() {
                        let t = doc.comment_token();
                        doc.buffer.toggle_comment(t);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("✳ Send Selection to AI Agent").clicked() {
                        actions.push(EditorAction::SendSelectionToAgent);
                        ui.close_menu();
                    }
                    for (label, q) in [
                        ("✳ Explain This Code", "Explain what this code does."),
                        ("✳ Find Bugs", "Review this code for bugs and suggest fixes."),
                        ("✳ Write Tests", "Write unit tests for this code."),
                        (
                            "✳ Refactor",
                            "Refactor this code to be cleaner and more idiomatic, keeping behavior the same.",
                        ),
                    ] {
                        if ui.button(label).clicked() {
                            actions.push(EditorAction::AskAgent { question: q.into() });
                            ui.close_menu();
                        }
                    }
                });

                // --- paint ---
                let painter = ui.painter().clone();
                let first = ((viewport.min.y / m.line_h).floor().max(0.0)) as usize;
                let last = (((viewport.max.y / m.line_h).ceil()) as usize + 1).min(total_lines);
                if !doc.highlight.ensure(&doc.buffer, last) {
                    ui.ctx().request_repaint();
                }

                let sel = doc.buffer.sel;
                let cursor = doc.buffer.cursor_pos();
                let sel_range = sel.range();
                let bracket = matching_bracket(doc);

                // First find-match index at or after the first visible char.
                let first_char = doc.buffer.rope().line_to_char(first.min(total_lines.saturating_sub(1)));
                let mut mi = doc.find.matches.partition_point(|r| r.end < first_char);
                let mut prev_indent = 0usize;

                for line in first..last {
                    let y = rect.min.y + line as f32 * m.line_h;
                    let chars = line_chars(doc, line);
                    let vc = visual_cols(&chars, m.tab);
                    let x_of = |col: usize| text_x + vc[col.min(vc.len() - 1)] as f32 * m.char_w;
                    let line_start = doc.buffer.rope().line_to_char(line);
                    let line_end = line_start + chars.len();
                    let row = Rect::from_min_size(egui::pos2(clip.min.x, y), egui::vec2(clip.width(), m.line_h));

                    if line == cursor.line && sel.is_empty() {
                        painter.rect_filled(row, 0.0, p.current_line);
                    }

                    // Find matches.
                    while mi < doc.find.matches.len() && doc.find.matches[mi].start <= line_end {
                        let r = doc.find.matches[mi].clone();
                        if r.end >= line_start {
                            let a = r.start.max(line_start) - line_start;
                            let b = r.end.min(line_end) - line_start;
                            let mr = Rect::from_min_max(
                                egui::pos2(x_of(a), y + 1.0),
                                egui::pos2(x_of(b), y + m.line_h - 1.0),
                            );
                            let is_cur = doc.find.current == mi;
                            painter.rect_filled(
                                mr,
                                2.0,
                                if is_cur { p.find_match } else { p.find_match.gamma_multiply(0.45) },
                            );
                        }
                        if r.end > line_end {
                            break;
                        }
                        mi += 1;
                    }

                    // Selection.
                    if !sel.is_empty() && sel_range.start <= line_end && sel_range.end >= line_start {
                        let a = sel_range.start.max(line_start) - line_start;
                        let b = sel_range.end.min(line_end) - line_start;
                        let mut x1 = x_of(b);
                        if sel_range.end > line_end {
                            x1 += m.char_w * 0.6; // newline is selected
                        }
                        let sr = Rect::from_min_max(egui::pos2(x_of(a), y), egui::pos2(x1, y + m.line_h));
                        painter.rect_filled(sr, 3.0, p.selection);
                    }

                    // Indent guides.
                    if s.show_indent_guides {
                        let lead = chars.iter().take_while(|c| **c == ' ' || **c == '\t').count();
                        let indent = if lead == chars.len() { prev_indent } else { vc[lead] };
                        prev_indent = indent;
                        for k in (0..indent).step_by(m.tab) {
                            let x = (text_x + k as f32 * m.char_w).round() + 0.5;
                            painter.vline(x, y..=y + m.line_h, Stroke::new(1.0_f32, p.indent_guide));
                        }
                    }

                    // Text.
                    if !chars.is_empty() {
                        let job = layout_line(&chars, doc.highlight.line(line), &m, p);
                        let galley = ui.fonts(|f| f.layout_job(job));
                        painter.galley(egui::pos2(text_x, y + (m.line_h - m.row_h) / 2.0), galley, p.text);
                    }

                    // Bracket match boxes.
                    if let Some((a, b)) = bracket {
                        for idx in [a, b] {
                            if idx >= line_start && idx < line_end {
                                let c = idx - line_start;
                                let br = Rect::from_min_max(
                                    egui::pos2(x_of(c), y + 1.0),
                                    egui::pos2(x_of(c + 1), y + m.line_h - 1.0),
                                );
                                painter.rect_stroke(br, 2.0, Stroke::new(1.0_f32, p.text_dim), StrokeKind::Inside);
                            }
                        }
                    }
                }

                // Cursor.
                let cursor_chars = line_chars(doc, cursor.line);
                let cvc = visual_cols(&cursor_chars, m.tab);
                let cx = text_x + cvc[cursor.col.min(cvc.len() - 1)] as f32 * m.char_w;
                let cy = rect.min.y + cursor.line as f32 * m.line_h;
                let cursor_rect = Rect::from_min_size(egui::pos2(cx - 1.0, cy + 2.0), egui::vec2(2.0, m.line_h - 4.0));
                if has_focus {
                    let t = ui.input(|i| i.time) - cursor_epoch;
                    let visible = !s.cursor_blink || (t % 1.1) < 0.65;
                    if visible {
                        painter.rect_filled(cursor_rect, 1.0, p.cursor);
                    }
                    if s.cursor_blink {
                        let next = 0.55 - (t % 0.55);
                        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(next.max(0.05)));
                    }
                    ui.output_mut(|o| {
                        o.ime = Some(egui::output::IMEOutput { rect: clip, cursor_rect });
                    });
                }
                if moved || doc.scroll_to_cursor {
                    doc.scroll_to_cursor = false;
                    let margin = Rect::from_min_max(
                        egui::pos2(cx - gutter_w - TEXT_PAD - 4.0 * m.char_w, cy - m.line_h * 2.0),
                        egui::pos2(cx + 6.0 * m.char_w, cy + m.line_h * 3.0),
                    );
                    ui.scroll_to_rect(margin, None);
                }

                // Gutter (painted last so it stays fixed when scrolling horizontally).
                let gutter = Rect::from_min_max(
                    egui::pos2(clip.min.x, clip.min.y),
                    egui::pos2(clip.min.x + gutter_w, clip.max.y),
                );
                painter.rect_filled(gutter, 0.0, p.bg);
                let num_font = FontId::monospace(s.editor_font_size * 0.86);
                for line in first..last {
                    let y = rect.min.y + line as f32 * m.line_h + m.line_h / 2.0;
                    let active = line == cursor.line;
                    painter.text(
                        egui::pos2(clip.min.x + gutter_w - 14.0, y),
                        Align2::RIGHT_CENTER,
                        (line + 1).to_string(),
                        num_font.clone(),
                        if active { p.text } else { p.text_faint },
                    );
                }
            })
        })
        .inner;

    // --- minimap ---
    if show_minimap {
        let mm = Rect::from_min_max(egui::pos2(editor_rect.max.x, full.min.y), full.max);
        minimap(ui, doc, p, &m, mm, &out, id);
    }

    ui.allocate_rect(full, Sense::hover());
    actions
}

fn layout_line(chars: &[char], spans: Option<&[crate::highlight::Span]>, m: &Metrics, p: &Palette) -> LayoutJob {
    let mut job = LayoutJob::default();
    let expand = |slice: &[char], start_vcol: &mut usize| -> String {
        let mut out = String::with_capacity(slice.len());
        for &c in slice {
            if c == '\t' {
                let n = m.tab - *start_vcol % m.tab;
                out.extend(std::iter::repeat_n(' ', n));
                *start_vcol += n;
            } else {
                out.push(c);
                *start_vcol += c.width().unwrap_or(1).max(1);
            }
        }
        out
    };
    let mut vcol = 0;
    let fmt = |color: Color32, bold: bool, italic: bool| TextFormat {
        font_id: if bold {
            FontId::new(m.font.size, FontFamily::Name(MONO_BOLD.into()))
        } else if italic {
            FontId::new(m.font.size, FontFamily::Name(MONO_ITALIC.into()))
        } else {
            m.font.clone()
        },
        color,
        ..Default::default()
    };
    match spans {
        Some(spans) if !spans.is_empty() => {
            let mut covered = 0;
            for sp in spans {
                let (a, b) = (sp.start.min(chars.len()), sp.end.min(chars.len()));
                if a >= b {
                    continue;
                }
                let t = expand(&chars[a..b], &mut vcol);
                job.append(&t, 0.0, fmt(sp.color, sp.bold, sp.italic));
                covered = b;
            }
            if covered < chars.len() {
                let t = expand(&chars[covered..], &mut vcol);
                job.append(&t, 0.0, fmt(p.text, false, false));
            }
        }
        _ => {
            let t = expand(chars, &mut vcol);
            job.append(&t, 0.0, fmt(p.text, false, false));
        }
    }
    job
}

fn minimap(
    ui: &mut egui::Ui,
    doc: &Document,
    p: &Palette,
    m: &Metrics,
    rect: Rect,
    out: &egui::scroll_area::ScrollAreaOutput<()>,
    id: egui::Id,
) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, p.bg);
    painter.vline(rect.min.x, rect.y_range(), Stroke::new(1.0_f32, p.border));
    let lh = 3.0;
    let total = doc.buffer.len_lines();
    let capacity = (rect.height() / lh) as usize;
    let max_offset = (out.content_size.y - out.inner_rect.height()).max(1.0);
    let frac = (out.state.offset.y / max_offset).clamp(0.0, 1.0);
    let first = if total > capacity { ((total - capacity) as f32 * frac) as usize } else { 0 };
    let cw = 1.2;
    for (row, line) in (first..total.min(first + capacity)).enumerate() {
        let y = rect.min.y + row as f32 * lh;
        let chars = line_chars(doc, line);
        if chars.is_empty() {
            continue;
        }
        let vc = visual_cols(&chars, m.tab);
        let draw = |a: usize, b: usize, color: Color32| {
            // Skip whitespace-only runs.
            let mut a = a;
            while a < b && chars[a].is_whitespace() {
                a += 1;
            }
            if a >= b {
                return;
            }
            let x0 = rect.min.x + 6.0 + vc[a] as f32 * cw;
            let x1 = (rect.min.x + 6.0 + vc[b] as f32 * cw).min(rect.max.x - 4.0);
            if x1 > x0 {
                painter.rect_filled(
                    Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + lh - 1.0)),
                    0.0,
                    color.gamma_multiply(0.75),
                );
            }
        };
        match doc.highlight.line(line) {
            Some(spans) if !spans.is_empty() => {
                for sp in spans {
                    draw(sp.start.min(chars.len()), sp.end.min(chars.len()), sp.color);
                }
            }
            _ => draw(0, chars.len(), p.text_faint),
        }
    }
    // Viewport slider.
    let view_first = out.state.offset.y / m.line_h;
    let view_lines = out.inner_rect.height() / m.line_h;
    let sy = rect.min.y + (view_first - first as f32) * lh;
    let slider = Rect::from_min_size(egui::pos2(rect.min.x + 1.0, sy), egui::vec2(rect.width() - 1.0, view_lines * lh));
    let resp = ui.interact(rect, id.with("minimap"), Sense::click_and_drag());
    let hovered = resp.hovered() || resp.dragged();
    painter.rect_filled(slider, 0.0, if hovered { p.accent_soft } else { p.current_line.gamma_multiply(1.6) });
    if let Some(pos) = resp.interact_pointer_pos() {
        if resp.clicked() || resp.dragged() {
            let line = first as f32 + (pos.y - rect.min.y) / lh - view_lines / 2.0;
            let y = (line.max(0.0) * m.line_h).min(max_offset);
            ui.data_mut(|d| d.insert_temp(id.with("minimap-jump"), y));
            ui.ctx().request_repaint();
        }
    }
}

fn matching_bracket(doc: &Document) -> Option<(usize, usize)> {
    let rope = doc.buffer.rope();
    let len = rope.len_chars();
    let head = doc.buffer.sel.head;
    let pairs = [('(', ')'), ('[', ']'), ('{', '}')];
    let candidates = [Some(head), head.checked_sub(1)];
    for idx in candidates.into_iter().flatten() {
        if idx >= len {
            continue;
        }
        let c = rope.char(idx);
        for (open, close) in pairs {
            if c == open {
                let mut depth = 0i32;
                for (i, ch) in rope.chars_at(idx).enumerate().take(20_000) {
                    if ch == open {
                        depth += 1;
                    } else if ch == close {
                        depth -= 1;
                        if depth == 0 {
                            return Some((idx, idx + i));
                        }
                    }
                }
            } else if c == close {
                let mut depth = 0i32;
                let mut i = idx + 1;
                let mut steps = 0;
                while i > 0 && steps < 20_000 {
                    i -= 1;
                    steps += 1;
                    let ch = rope.char(i);
                    if ch == close {
                        depth += 1;
                    } else if ch == open {
                        depth -= 1;
                        if depth == 0 {
                            return Some((i, idx));
                        }
                    }
                }
            }
        }
    }
    None
}

/// Returns true if the caret moved (so the view should follow it).
fn handle_keys(ui: &mut egui::Ui, doc: &mut Document, page: isize) -> bool {
    let events = ui.input(|i| i.events.clone());
    let mac = cfg!(target_os = "macos");
    let mut moved = false;
    for ev in events {
        match ev {
            Event::Text(t) => {
                for c in t.chars() {
                    if !c.is_control() {
                        doc.buffer.type_char(c);
                    }
                }
                moved = true;
            }
            Event::Paste(t) => {
                doc.buffer.insert(&t);
                doc.buffer.seal_history();
                moved = true;
            }
            Event::Copy | Event::Cut => {
                let cut = matches!(ev, Event::Cut);
                if doc.buffer.sel.is_empty() {
                    // VS Code: copy/cut the whole line when nothing is selected.
                    let line = doc.buffer.cursor_pos().line;
                    doc.buffer.select_line(line);
                    let mut text = doc.buffer.selected_text();
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    ui.ctx().copy_text(text);
                    if cut {
                        doc.buffer.backspace();
                    } else {
                        let pos = doc.buffer.char_to_pos(doc.buffer.sel.anchor);
                        let idx = doc.buffer.pos_to_char(pos);
                        doc.buffer.set_cursor(idx, false);
                    }
                } else {
                    ui.ctx().copy_text(doc.buffer.selected_text());
                    if cut {
                        doc.buffer.backspace();
                    }
                }
                moved = true;
            }
            Event::Ime(egui::ImeEvent::Commit(t)) => {
                doc.buffer.insert(&t);
                moved = true;
            }
            Event::Key { key, pressed: true, modifiers, .. } if handle_key(doc, key, modifiers, mac, page) => {
                moved = true;
            }
            _ => {}
        }
    }
    moved
}

fn handle_key(doc: &mut Document, key: Key, mods: Modifiers, mac: bool, page: isize) -> bool {
    let b = &mut doc.buffer;
    let shift = mods.shift;
    let cmd = mods.command;
    let word = if mac { mods.alt } else { mods.ctrl };
    match key {
        Key::ArrowLeft if mac && mods.mac_cmd => b.move_home(shift),
        Key::ArrowRight if mac && mods.mac_cmd => b.move_end(shift),
        Key::ArrowLeft if word => b.move_word_left(shift),
        Key::ArrowRight if word => b.move_word_right(shift),
        Key::ArrowLeft => b.move_left(shift),
        Key::ArrowRight => b.move_right(shift),
        Key::ArrowUp if mods.alt && !shift => b.move_line(true),
        Key::ArrowDown if mods.alt && !shift => b.move_line(false),
        Key::ArrowUp if mods.alt && shift => b.duplicate_line(),
        Key::ArrowDown if mods.alt && shift => b.duplicate_line(),
        Key::ArrowUp if mac && mods.mac_cmd => b.set_cursor(0, shift),
        Key::ArrowDown if mac && mods.mac_cmd => {
            let n = b.len_chars();
            b.set_cursor(n, shift)
        }
        Key::ArrowUp => b.move_vertical(-1, shift),
        Key::ArrowDown => b.move_vertical(1, shift),
        Key::Home if cmd => b.set_cursor(0, shift),
        Key::End if cmd => {
            let n = b.len_chars();
            b.set_cursor(n, shift)
        }
        Key::Home => b.move_home(shift),
        Key::End => b.move_end(shift),
        Key::PageUp => b.move_vertical(-page, shift),
        Key::PageDown => b.move_vertical(page, shift),
        Key::Backspace if mac && mods.mac_cmd => {
            b.move_home(true);
            b.backspace();
        }
        Key::Backspace if word => b.delete_word_left(),
        Key::Backspace => b.backspace(),
        Key::Delete if word => b.delete_word_right(),
        Key::Delete => b.delete(),
        Key::Enter if cmd && shift => {
            b.move_home(false);
            b.move_home(false);
            b.insert("\n");
            b.move_left(false);
        }
        Key::Enter if cmd => {
            b.move_end(false);
            b.newline();
        }
        Key::Enter => b.newline(),
        Key::Tab if shift => b.indent_lines(false),
        Key::Tab => b.tab(),
        Key::Escape => {
            if doc.find.open {
                doc.find.open = false;
            } else {
                let h = b.sel.head;
                b.sel = Selection::cursor(h);
            }
        }
        Key::A if cmd => b.select_all(),
        Key::Z if cmd && shift => {
            b.redo();
        }
        Key::Z if cmd => {
            b.undo();
        }
        Key::Y if cmd && !mac => {
            b.redo();
        }
        Key::Slash if cmd => {
            let t = doc.comment_token();
            doc.buffer.toggle_comment(t);
        }
        Key::K if cmd && shift => {
            let line = b.cursor_pos().line;
            b.select_line(line);
            b.backspace();
        }
        Key::L if cmd => {
            let line = b.char_to_pos(b.sel.end()).line;
            let start = if b.sel.is_empty() { b.pos_to_char(Position::new(line, 0)) } else { b.sel.start() };
            b.select_line(line);
            b.sel.anchor = start;
        }
        Key::D if cmd && shift => b.duplicate_line(),
        Key::D if cmd => select_next_occurrence(doc),
        Key::OpenBracket if cmd => b.indent_lines(false),
        Key::CloseBracket if cmd => b.indent_lines(true),
        _ => return false,
    }
    true
}

/// Ctrl/Cmd+D: select the word under the caret, then jump to the next match.
fn select_next_occurrence(doc: &mut Document) {
    let b = &mut doc.buffer;
    if b.sel.is_empty() {
        if let Some(r) = b.word_under_cursor() {
            b.sel = Selection::new(r.start, r.end);
        }
        return;
    }
    let needle = b.selected_text();
    let hits = zenith_core::find_all(
        b.rope(),
        &needle,
        zenith_core::SearchOptions { case_sensitive: true, whole_word: false },
    );
    let end = b.sel.end();
    if let Some(next) = hits.iter().find(|r| r.start >= end).or(hits.first()) {
        b.sel = Selection::new(next.start, next.end);
        doc.reveal_line = Some(doc.buffer.char_to_pos(next.start).line);
    }
}

fn find_bar(ui: &mut egui::Ui, doc: &mut Document, p: &Palette) {
    let frame = egui::Frame::new()
        .fill(p.bg_elevated)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .stroke(Stroke::new(1.0_f32, p.border));
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            let toggle = if doc.find.show_replace { "▾" } else { "▸" };
            if ui.small_button(toggle).on_hover_text("Toggle replace").clicked() {
                doc.find.show_replace = !doc.find.show_replace;
            }
            let q = ui.add(
                egui::TextEdit::singleline(&mut doc.find.query)
                    .hint_text("Find")
                    .desired_width(260.0)
                    .font(egui::TextStyle::Monospace),
            );
            if doc.find.focus_query {
                q.request_focus();
                doc.find.focus_query = false;
            }
            if q.changed() {
                doc.update_find();
                if let Some(first) = doc.find.matches.get(doc.find.current).cloned() {
                    doc.reveal_line = Some(doc.buffer.char_to_pos(first.start).line);
                }
            }
            if q.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let back = ui.input(|i| i.modifiers.shift);
                doc.find_step(!back);
                q.request_focus();
            }
            if q.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                doc.find.open = false;
            }
            let n = doc.find.matches.len();
            let label = if n == 0 {
                if doc.find.query.is_empty() {
                    String::new()
                } else {
                    "No results".into()
                }
            } else {
                format!("{} of {}", (doc.find.current + 1).min(n), n)
            };
            ui.label(egui::RichText::new(label).color(p.text_dim).small());
            let mut changed = false;
            changed |= ui.toggle_value(&mut doc.find.opts.case_sensitive, "Aa").on_hover_text("Match case").changed();
            changed |= ui.toggle_value(&mut doc.find.opts.whole_word, "ab|").on_hover_text("Whole word").changed();
            if changed {
                doc.update_find();
            }
            if ui.small_button("↑").on_hover_text("Previous (Shift+Enter)").clicked() {
                doc.find_step(false);
            }
            if ui.small_button("↓").on_hover_text("Next (Enter)").clicked() {
                doc.find_step(true);
            }
            if ui.small_button("✕").clicked() {
                doc.find.open = false;
            }
        });
        if doc.find.show_replace {
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.add(
                    egui::TextEdit::singleline(&mut doc.find.replacement)
                        .hint_text("Replace")
                        .desired_width(260.0)
                        .font(egui::TextStyle::Monospace),
                );
                if ui.button("Replace").clicked() {
                    doc.replace_current();
                }
                if ui.button("Replace All").clicked() {
                    doc.replace_all();
                }
            });
        }
    });
}
