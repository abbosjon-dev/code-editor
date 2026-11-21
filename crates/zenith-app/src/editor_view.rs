//! The code editor widget: custom-painted, virtualized (only visible lines are
//! laid out), with gutter, selections, find highlights, indent guides,
//! bracket matching and a minimap.

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, CursorIcon, Event, FontFamily, FontId, Key, Modifiers, Pos2, Rect, Sense,
    Stroke, StrokeKind, TextFormat, UiBuilder,
};
use unicode_width::UnicodeWidthChar;
use zenith_core::{Position, Selection};
use zenith_lsp::Severity;

use crate::document::Document;
use crate::settings::Settings;
use crate::theme::{Palette, MONO_BOLD, MONO_ITALIC};

/// Things the editor asks the app to do.
pub enum EditorAction {
    AskAgent {
        question: String,
    },
    SendSelectionToAgent,
    /// Ask the language server for completions at the caret.
    Complete {
        trigger: Option<char>,
    },
    /// Ask for hover info at a char index.
    Hover {
        idx: usize,
    },
    GotoDefinition {
        idx: usize,
    },
    /// Run an app command (from the context menu).
    Run(crate::commands::Cmd),
    /// Ask for parameter hints at the caret.
    SignatureHelp,
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

/// Widget id of the editor for `doc` in `pane` (0 = left/main, 1 = right split).
pub fn editor_id(ui: &egui::Ui, doc: u64, pane: usize) -> egui::Id {
    ui.make_persistent_id(("zenith-editor", doc, pane))
}

const MINIMAP_W: f32 = 84.0;
const TEXT_PAD: f32 = 10.0;

pub fn editor_ui(
    ui: &mut egui::Ui,
    doc: &mut Document,
    p: &Palette,
    s: &Settings,
    cursor_epoch: f64,
    pane: usize,
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
    let gutter_w = digits as f32 * m.char_w + 40.0;
    let longest = doc.longest_line_chars();

    let full = ui.available_rect_before_wrap();
    let show_minimap = s.show_minimap && full.width() > 500.0;
    let editor_rect =
        if show_minimap { Rect::from_min_max(full.min, egui::pos2(full.max.x - MINIMAP_W, full.max.y)) } else { full };
    let viewport_h = editor_rect.height();

    let id = editor_id(ui, doc.id, pane);
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
        let (m2, acts) = handle_keys(ui, doc, page);
        moved = m2;
        actions.extend(acts);
    }
    doc.refilter_completion();
    if moved || doc.reveal_line.is_some() {
        doc.reveal_cursor_in_folds();
    }
    // Soft wrap: columns that fit in the text area (no horizontal scrolling).
    doc.wrap_cols = if s.word_wrap {
        (((editor_rect.width() - gutter_w - TEXT_PAD - 18.0) / m.char_w).floor() as usize).max(20)
    } else {
        0
    };
    let fm = doc.fold_map();
    let total_rows = fm.row_count();
    let wrapping = fm.wrap_cols > 0;
    let mut cursor_screen: Option<Pos2> = None;
    let mut cursor_top: Option<Pos2> = None;
    // Parameter hints are only valid on the line where they were requested.
    if doc.signature.as_ref().is_some_and(|sg| sg.line != doc.buffer.cursor_pos().line) {
        doc.signature = None;
    }
    let mut hover_screen: Option<(Pos2, usize)> = None;

    let mut scroll = egui::ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded);
    if let Some(line) = doc.reveal_line.take() {
        let y = (fm.row_of(line) as f32 * m.line_h - viewport_h * 0.35).max(0.0);
        scroll = scroll.vertical_scroll_offset(y);
    }
    if let Some(y) = ui.data_mut(|d| d.remove_temp::<f32>(id.with("minimap-jump"))) {
        scroll = scroll.vertical_scroll_offset(y);
    }

    let content_w = if wrapping { 0.0 } else { gutter_w + TEXT_PAD + (longest as f32 + 8.0) * m.char_w };
    let content_h = total_rows as f32 * m.line_h + viewport_h * 0.6;

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
                        let row = (((pos.y - rect.min.y) / m.line_h).floor().max(0.0) as usize)
                            .min(total_rows.saturating_sub(1));
                        let line = fm.line_at(row);
                        let chars = line_chars(doc, line);
                        let vc = visual_cols(&chars, m.tab);
                        let (seg_s, seg_e) = fm.row_cols(row);
                        let seg_s = seg_s.min(chars.len());
                        // On a wrapped row (not the last), stay before the break.
                        let seg_e = if seg_e == usize::MAX { chars.len() } else { seg_e.saturating_sub(1).max(seg_s) };
                        let target = (pos.x - text_x) / m.char_w + vc[seg_s] as f32;
                        let mut col = seg_e;
                        for i in seg_s..seg_e {
                            let mid = (vc[i] + vc[i + 1]) as f32 / 2.0;
                            if target < mid {
                                col = i;
                                break;
                            }
                        }
                        doc.buffer.pos_to_char(Position::new(line, col))
                    };
                    let shift = ui.input(|i| i.modifiers.shift);
                    let alt_held = ui.input(|i| i.modifiers.alt);
                    if resp.triple_clicked() && !alt_held {
                        let line = doc.buffer.cursor_pos().line;
                        doc.buffer.select_line(line);
                    } else if resp.double_clicked() && !alt_held {
                        let idx = hit(doc, pos);
                        doc.buffer.select_word_at(idx);
                    } else if ui.input(|i| i.pointer.primary_pressed()) && resp.hovered() {
                        let idx = hit(doc, pos);
                        doc.completion = None;
                        let alt = ui.input(|i| i.modifiers.alt);
                        if alt && !in_gutter {
                            doc.buffer.add_selection(Selection::cursor(idx));
                        } else if ui.input(|i| i.modifiers.command) && !in_gutter {
                            doc.buffer.set_cursor(idx, false);
                            actions.push(EditorAction::GotoDefinition { idx });
                        } else if in_gutter
                            && pos.x < clip.min.x + 16.0
                            && doc.lsp_active
                            && doc.diagnostics.iter().any(|d| d.line == doc.buffer.char_to_pos(idx).line)
                        {
                            // Lightbulb: quick fixes for this line.
                            doc.buffer.clear_extra();
                            doc.buffer.set_cursor(idx, false);
                            actions.push(EditorAction::Run(crate::commands::Cmd::CodeActions));
                        } else if in_gutter && !shift && pos.x > clip.min.x + gutter_w - 20.0 {
                            // Fold chevron column.
                            let line = doc.buffer.char_to_pos(idx).line;
                            doc.toggle_fold(line);
                        } else if in_gutter && !shift {
                            let line = doc.buffer.char_to_pos(idx).line;
                            doc.buffer.select_line(line);
                        } else {
                            doc.buffer.clear_extra();
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
                // Hover: after the pointer rests on text, ask for info.
                if let Some(pos) = resp.hover_pos() {
                    if pos.x > clip.min.x + gutter_w && !resp.dragged() {
                        let row = ((pos.y - rect.min.y) / m.line_h).floor().max(0.0) as usize;
                        let line = fm.line_at(row);
                        if row < total_rows {
                            let chars = line_chars(doc, line);
                            let vc = visual_cols(&chars, m.tab);
                            let seg_s = fm.row_cols(row).0.min(chars.len());
                            let target = ((pos.x - text_x) / m.char_w).floor() as isize + vc[seg_s] as isize;
                            if let Some(col) =
                                (0..chars.len()).find(|&c| vc[c] as isize <= target && (vc[c + 1] as isize) > target)
                            {
                                let idx = doc.buffer.pos_to_char(Position::new(line, col));
                                hover_screen = Some((pos, idx));
                                if ui.input(|i| i.modifiers.command) {
                                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                }
                            }
                        }
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
                    if doc.lsp_active {
                        ui.separator();
                        use crate::commands::Cmd;
                        for (label, cmd) in [
                            ("Go to Definition", Cmd::GotoDefinition),
                            ("Find All References", Cmd::FindReferences),
                            ("Rename Symbol", Cmd::RenameSymbol),
                            ("Format Document", Cmd::FormatDocument),
                        ] {
                            if ui.add(egui::Button::new(label).shortcut_text(cmd.shortcut())).clicked() {
                                actions.push(EditorAction::Run(cmd));
                                ui.close_menu();
                            }
                        }
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
                let first_row = ((viewport.min.y / m.line_h).floor().max(0.0)) as usize;
                let last_row = (((viewport.max.y / m.line_h).ceil()) as usize + 1).min(total_rows);
                // Document line bounds of what's on screen.
                let first = fm.line_at(first_row);
                let last = if last_row == 0 { 0 } else { (fm.line_at(last_row - 1) + 1).min(total_lines) };
                let y_of_line = |line: usize| rect.min.y + fm.row_of(line) as f32 * m.line_h;
                if !doc.highlight.ensure(&doc.buffer, first, last) {
                    ui.ctx().request_repaint();
                }

                let sel = doc.buffer.sel;
                let cursor = doc.buffer.cursor_pos();
                let all_ranges: Vec<std::ops::Range<usize>> =
                    doc.buffer.selections().iter().map(|s| s.range()).collect();
                let bracket = matching_bracket(doc);

                let mut prev_indent = 0usize;

                for row in first_row..last_row {
                    let line = fm.line_at(row);
                    let y = rect.min.y + row as f32 * m.line_h;
                    let chars = line_chars(doc, line);
                    let vc = visual_cols(&chars, m.tab);
                    // The part of the line shown on this row (soft wrap).
                    let (seg_s, seg_raw_e) = fm.row_cols(row);
                    let seg_s = seg_s.min(chars.len());
                    let seg_e = seg_raw_e.min(chars.len());
                    let last_seg = seg_raw_e == usize::MAX;
                    let x_of = |col: usize| text_x + (vc[col.clamp(seg_s, seg_e)] - vc[seg_s]) as f32 * m.char_w;
                    // Clip a line-relative [a, b) range to this row.
                    let in_seg = |a: usize, b: usize| -> Option<(usize, usize)> {
                        let (a2, b2) = (a.max(seg_s), b.min(seg_e));
                        if a2 > b2 || (a2 == b2 && a != b && !(last_seg && a2 == seg_e)) {
                            None
                        } else {
                            Some((a2, b2))
                        }
                    };
                    let line_start = doc.buffer.rope().line_to_char(line);
                    let line_end = line_start + chars.len();
                    let mut mi = doc.find.matches.partition_point(|r| r.end < line_start);
                    let row = Rect::from_min_size(egui::pos2(clip.min.x, y), egui::vec2(clip.width(), m.line_h));

                    if line == cursor.line && sel.is_empty() && !doc.buffer.has_multi() {
                        painter.rect_filled(row, 0.0, p.current_line);
                    }

                    // Find matches.
                    while mi < doc.find.matches.len() && doc.find.matches[mi].start <= line_end {
                        let r = doc.find.matches[mi].clone();
                        let seg = in_seg(r.start.max(line_start) - line_start, r.end.min(line_end) - line_start);
                        if let (true, Some((a, b))) = (r.end >= line_start, seg) {
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

                    // Selections (all cursors).
                    for sel_range in &all_ranges {
                        if sel_range.is_empty() || sel_range.start > line_end || sel_range.end < line_start {
                            continue;
                        }
                        let Some((a, b)) = in_seg(
                            sel_range.start.max(line_start) - line_start,
                            sel_range.end.min(line_end) - line_start,
                        ) else {
                            continue;
                        };
                        let mut x1 = x_of(b);
                        if sel_range.end > line_end && last_seg {
                            x1 += m.char_w * 0.6; // newline is selected
                        }
                        let sr = Rect::from_min_max(egui::pos2(x_of(a), y), egui::pos2(x1, y + m.line_h));
                        painter.rect_filled(sr, 3.0, p.selection);
                    }

                    // Indent guides.
                    if s.show_indent_guides && seg_s == 0 {
                        let lead = chars.iter().take_while(|c| **c == ' ' || **c == '\t').count();
                        let indent = if lead == chars.len() { prev_indent } else { vc[lead] };
                        prev_indent = indent;
                        for k in (0..indent).step_by(m.tab) {
                            let x = (text_x + k as f32 * m.char_w).round() + 0.5;
                            painter.vline(x, y..=y + m.line_h, Stroke::new(1.0_f32, p.indent_guide));
                        }
                    }

                    // Diagnostics: squiggly underline.
                    for d in doc.diagnostics.iter().filter(|d| d.start <= line_end && d.end >= line_start) {
                        let a = d.start.clamp(line_start, line_end) - line_start;
                        let mut b = d.end.clamp(line_start, line_end) - line_start;
                        if a == b {
                            b = (a + 1).min(chars.len().max(a + 1));
                        }
                        let Some((a, b)) = in_seg(a, b) else { continue };
                        let color = severity_color(p, d.severity);
                        let (x0, x1) = (x_of(a), x_of(b).max(x_of(a) + m.char_w));
                        let base = y + m.line_h - 3.0;
                        let mut pts = vec![];
                        let mut x = x0;
                        let mut up = true;
                        while x <= x1 {
                            pts.push(egui::pos2(x, if up { base - 1.5 } else { base + 1.0 }));
                            x += 2.5;
                            up = !up;
                        }
                        if pts.len() >= 2 {
                            painter.add(egui::Shape::line(pts, Stroke::new(1.0_f32, color)));
                        }
                    }

                    // Text.
                    if seg_e > seg_s {
                        let spans = doc.highlight.line(line).map(|sp| slice_spans(sp, seg_s, seg_e));
                        let job = layout_line(&chars[seg_s..seg_e], spans.as_deref(), &m, p);
                        let galley = ui.fonts(|f| f.layout_job(job));
                        painter.galley(egui::pos2(text_x, y + (m.line_h - m.row_h) / 2.0), galley, p.text);
                    }

                    if doc.folds.contains(&line) && !fm.is_identity() && last_seg {
                        let x = x_of(chars.len()) + m.char_w;
                        let pill =
                            Rect::from_min_size(egui::pos2(x, y + 3.0), egui::vec2(m.char_w * 3.0, m.line_h - 6.0));
                        painter.rect_filled(pill, 4.0, p.bg_active);
                        painter.text(
                            pill.center(),
                            Align2::CENTER_CENTER,
                            "⋯",
                            FontId::proportional(m.font.size * 0.9),
                            p.text_dim,
                        );
                    }

                    // Bracket match boxes.
                    if let Some((a, b)) = bracket {
                        for idx in [a, b] {
                            if idx >= line_start
                                && idx < line_end
                                && in_seg(idx - line_start, idx - line_start + 1).is_some()
                            {
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
                // Screen position of a caret, honoring soft wrap.
                let caret_xy = |doc: &Document, line: usize, col: usize| -> (f32, f32) {
                    let chars = line_chars(doc, line);
                    let vc = visual_cols(&chars, m.tab);
                    let col = col.min(chars.len());
                    let row = fm.row_of_pos(line, col);
                    let seg_s = fm.row_cols(row).0.min(col);
                    (text_x + (vc[col] - vc[seg_s]) as f32 * m.char_w, rect.min.y + row as f32 * m.line_h)
                };
                let (cx, cy) = caret_xy(doc, cursor.line, cursor.col);
                let cursor_rect = Rect::from_min_size(egui::pos2(cx - 1.0, cy + 2.0), egui::vec2(2.0, m.line_h - 4.0));
                cursor_screen = Some(egui::pos2(cx, cy + m.line_h));
                cursor_top = Some(egui::pos2(cx, cy));
                if has_focus {
                    let t = ui.input(|i| i.time) - cursor_epoch;
                    let visible = !s.cursor_blink || (t % 1.1) < 0.65;
                    if visible {
                        painter.rect_filled(cursor_rect, 1.0, p.cursor);
                        for extra in &doc.buffer.extra {
                            let ep = doc.buffer.char_to_pos(extra.head);
                            if ep.line < first || ep.line >= last {
                                continue;
                            }
                            if fm.is_hidden(ep.line) {
                                continue;
                            }
                            let (ex, ey) = caret_xy(doc, ep.line, ep.col);
                            painter.rect_filled(
                                Rect::from_min_size(egui::pos2(ex - 1.0, ey + 2.0), egui::vec2(2.0, m.line_h - 4.0)),
                                1.0,
                                p.cursor.gamma_multiply(0.8),
                            );
                        }
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
                for &(a, b, kind) in &doc.git_hunks {
                    if b <= first || a >= last || (fm.is_hidden(a) && fm.is_hidden(b.saturating_sub(1))) {
                        continue;
                    }
                    let x = clip.min.x + gutter_w - 5.0;
                    let color = match kind {
                        crate::git::LineChange::Added => p.success,
                        crate::git::LineChange::Modified => p.accent,
                        crate::git::LineChange::Deleted => p.error,
                    };
                    if kind == crate::git::LineChange::Deleted {
                        let y = y_of_line(a);
                        painter.add(egui::Shape::convex_polygon(
                            vec![egui::pos2(x - 1.0, y - 4.0), egui::pos2(x + 4.0, y), egui::pos2(x - 1.0, y + 4.0)],
                            color,
                            Stroke::NONE,
                        ));
                    } else {
                        let y0 = y_of_line(a.max(first));
                        let y1 = rect.min.y + (fm.last_row_of(b.min(last).saturating_sub(1)) + 1) as f32 * m.line_h;
                        painter.rect_filled(
                            Rect::from_min_max(egui::pos2(x, y0 + 1.0), egui::pos2(x + 3.0, y1 - 1.0)),
                            1.0,
                            color,
                        );
                    }
                }
                let bulb_line = doc.lsp_active && doc.diagnostics.iter().any(|d| d.line == cursor.line);
                if bulb_line && !fm.is_hidden(cursor.line) && cursor.line >= first && cursor.line < last {
                    let y = y_of_line(cursor.line) + m.line_h / 2.0;
                    painter.text(
                        egui::pos2(clip.min.x + 8.0, y),
                        Align2::CENTER_CENTER,
                        "💡",
                        FontId::proportional(m.font.size * 0.85),
                        p.warning,
                    );
                }
                for d in doc.diagnostics.iter().rev() {
                    if bulb_line && d.line == cursor.line {
                        continue;
                    }
                    if d.line >= first && d.line < last && d.severity <= Severity::Warning && !fm.is_hidden(d.line) {
                        let y = y_of_line(d.line) + m.line_h / 2.0;
                        painter.circle_filled(egui::pos2(clip.min.x + 6.0, y), 3.0, severity_color(p, d.severity));
                    }
                }
                let gutter_hovered = resp.hover_pos().is_some_and(|h| h.x < clip.min.x + gutter_w);
                for row in first_row..last_row {
                    let line = fm.line_at(row);
                    let y = rect.min.y + row as f32 * m.line_h + m.line_h / 2.0;
                    if fm.row_cols(row).0 != 0 {
                        continue; // continuation of a wrapped line: no number
                    }
                    let active = line == cursor.line;
                    let folded = doc.folds.contains(&line);
                    if folded || (gutter_hovered && crate::folding::is_foldable(&doc.buffer, line)) {
                        painter.text(
                            egui::pos2(clip.min.x + gutter_w - 13.0, y),
                            Align2::CENTER_CENTER,
                            if folded { "▸" } else { "▾" },
                            FontId::proportional(m.font.size * 0.8),
                            if folded { p.accent } else { p.text_faint },
                        );
                    }
                    painter.text(
                        egui::pos2(clip.min.x + gutter_w - 22.0, y),
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

    // --- completion popup ---
    if let (Some(pos), true) = (cursor_screen, has_focus) {
        if out.inner_rect.contains(pos - egui::vec2(0.0, 1.0)) {
            completion_popup(ui, doc, p, pos, id, &m);
        }
    }

    // --- parameter hints ---
    if let (Some(top), true, Some(sig)) = (cursor_top, has_focus, doc.signature.as_ref().and_then(|s| s.help.as_ref()))
    {
        signature_popup(ui, p, top, sig, id, &m);
    }

    // --- hover: diagnostics + language server info ---
    match hover_screen {
        Some((pos, idx)) => {
            let still = ui.input(|i| i.pointer.time_since_last_movement()) > 0.45;
            let diag: Vec<String> = doc
                .diagnostics
                .iter()
                .filter(|d| d.start <= idx && idx < d.end.max(d.start + 1))
                .map(|d| d.message.clone())
                .collect();
            if still {
                let same = doc.hover.as_ref().is_some_and(|h| h.idx == idx);
                if !same && doc.lsp_active {
                    actions.push(EditorAction::Hover { idx });
                }
                let info =
                    doc.hover.as_ref().filter(|h| h.idx == idx).and_then(|h| h.text.clone()).filter(|t| !t.is_empty());
                if !diag.is_empty() || info.is_some() {
                    hover_popup(ui, p, pos, &diag, info.as_deref(), id);
                }
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
        }
        None => doc.hover = None,
    }

    ui.allocate_rect(full, Sense::hover());
    actions
}

fn severity_color(p: &Palette, s: Severity) -> Color32 {
    match s {
        Severity::Error => p.error,
        Severity::Warning => p.warning,
        Severity::Info => p.accent,
        Severity::Hint => p.text_faint,
    }
}

fn completion_popup(ui: &mut egui::Ui, doc: &mut Document, p: &Palette, pos: Pos2, id: egui::Id, m: &Metrics) {
    let Some(c) = &mut doc.completion else { return };
    if c.filtered.is_empty() {
        return;
    }
    let mut accept = None;
    let row_h = (m.line_h).max(22.0);
    let visible = c.filtered.len().min(10);
    let start = c.selected.saturating_sub(visible - 1).min(c.filtered.len() - visible);
    egui::Area::new(id.with("completion")).order(egui::Order::Foreground).fixed_pos(pos + egui::vec2(-8.0, 2.0)).show(
        ui.ctx(),
        |ui| {
            egui::Frame::popup(ui.style()).fill(p.bg_elevated).inner_margin(4).show(ui, |ui| {
                ui.set_width(420.0);
                for (row, &i) in c.filtered.iter().enumerate().skip(start).take(visible) {
                    let item = &c.items[i];
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(420.0, row_h), Sense::click());
                    if row == c.selected {
                        ui.painter().rect_filled(rect, 4.0, p.accent_soft);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, p.bg_hover);
                    }
                    let kind = item.kind_label();
                    ui.painter().text(
                        egui::pos2(rect.min.x + 8.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        kind,
                        FontId::monospace(10.5),
                        p.accent,
                    );
                    let label: String = item.label.chars().take(48).collect();
                    let label_rect = ui.painter().text(
                        egui::pos2(rect.min.x + 56.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::monospace(m.font.size * 0.95),
                        p.text,
                    );
                    if let Some(detail) = &item.detail {
                        let free = Rect::from_min_max(
                            egui::pos2(label_rect.max.x + 16.0, rect.min.y),
                            egui::pos2(rect.max.x - 8.0, rect.max.y),
                        );
                        if free.width() > 40.0 {
                            let galley =
                                ui.painter().layout_no_wrap(detail.clone(), FontId::proportional(11.5), p.text_dim);
                            let x = (free.max.x - galley.size().x).max(free.min.x);
                            ui.painter().with_clip_rect(free).galley(
                                egui::pos2(x, rect.center().y - galley.size().y / 2.0),
                                galley,
                                p.text_dim,
                            );
                        }
                    }
                    if resp.clicked() {
                        accept = Some(row);
                    }
                }
                if c.filtered.len() > visible {
                    ui.label(
                        egui::RichText::new(format!("{} / {}", c.selected + 1, c.filtered.len()))
                            .small()
                            .color(p.text_faint),
                    );
                }
            });
        },
    );
    if let Some(row) = accept {
        c.selected = row;
        doc.accept_completion();
        ui.memory_mut(|mem| mem.request_focus(id));
    }
}

fn signature_popup(
    ui: &mut egui::Ui,
    p: &Palette,
    top: Pos2,
    sig: &zenith_lsp::SignatureHelp,
    id: egui::Id,
    m: &Metrics,
) {
    egui::Area::new(id.with("signature"))
        .order(egui::Order::Foreground)
        .pivot(Align2::LEFT_BOTTOM)
        .fixed_pos(top - egui::vec2(8.0, 2.0))
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).fill(p.bg_elevated).inner_margin(egui::Margin::symmetric(10, 6)).show(
                ui,
                |ui| {
                    ui.set_max_width(640.0);
                    let font = FontId::monospace(m.font.size * 0.92);
                    let mut job = LayoutJob::default();
                    let chars: Vec<char> = sig.label.chars().collect();
                    let (a, b) = sig.active.unwrap_or((0, 0));
                    let fmt = |color: Color32, underline: bool| TextFormat {
                        font_id: font.clone(),
                        color,
                        underline: if underline { Stroke::new(1.0_f32, color) } else { Stroke::NONE },
                        ..Default::default()
                    };
                    let part = |r: std::ops::Range<usize>| {
                        chars[r.start.min(chars.len())..r.end.min(chars.len())].iter().collect::<String>()
                    };
                    job.append(&part(0..a), 0.0, fmt(p.text_dim, false));
                    job.append(&part(a..b), 0.0, fmt(p.accent, true));
                    job.append(&part(b..chars.len()), 0.0, fmt(p.text_dim, false));
                    ui.label(job);
                    if !sig.documentation.is_empty() {
                        let doc: String = sig.documentation.lines().take(4).collect::<Vec<_>>().join("\n");
                        ui.label(egui::RichText::new(doc).small().color(p.text_faint));
                    }
                },
            );
        });
}

fn hover_popup(ui: &mut egui::Ui, p: &Palette, pos: Pos2, diags: &[String], info: Option<&str>, id: egui::Id) {
    egui::Area::new(id.with("hover"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(12.0, 16.0))
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).fill(p.bg_elevated).inner_margin(10).show(ui, |ui| {
                ui.set_max_width(560.0);
                for d in diags {
                    ui.label(egui::RichText::new(format!("● {d}")).color(p.error));
                }
                if let Some(text) = info {
                    if !diags.is_empty() {
                        ui.separator();
                    }
                    let mut lines = text.lines();
                    if let Some(first) = lines.next() {
                        ui.label(egui::RichText::new(first).monospace().color(p.accent));
                    }
                    let rest: Vec<&str> = lines.take(18).collect();
                    let rest = rest.join("\n");
                    if !rest.trim().is_empty() {
                        ui.label(egui::RichText::new(rest.trim()).color(p.text_dim));
                    }
                }
            });
        });
}

/// Spans of `[start, end)` of a line, re-based to start at 0.
fn slice_spans(spans: &[crate::highlight::Span], start: usize, end: usize) -> Vec<crate::highlight::Span> {
    spans
        .iter()
        .filter(|sp| sp.end > start && sp.start < end)
        .map(|sp| crate::highlight::Span { start: sp.start.max(start) - start, end: sp.end.min(end) - start, ..*sp })
        .collect()
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
fn handle_keys(ui: &mut egui::Ui, doc: &mut Document, page: isize) -> (bool, Vec<EditorAction>) {
    let events = ui.input(|i| i.events.clone());
    let mac = cfg!(target_os = "macos");
    let mut moved = false;
    let mut actions = vec![];
    for ev in events {
        // Completion popup navigation takes priority.
        if let (Some(c), Event::Key { key, pressed: true, modifiers, .. }) = (&mut doc.completion, &ev) {
            if !c.filtered.is_empty() && modifiers.is_none() {
                match key {
                    Key::ArrowDown => {
                        c.selected = (c.selected + 1) % c.filtered.len();
                        continue;
                    }
                    Key::ArrowUp => {
                        c.selected = (c.selected + c.filtered.len() - 1) % c.filtered.len();
                        continue;
                    }
                    Key::Enter | Key::Tab => {
                        doc.accept_completion();
                        moved = true;
                        continue;
                    }
                    Key::Escape => {
                        doc.completion = None;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        if matches!(ev, Event::Key { key: Key::Escape, pressed: true, .. }) && doc.signature.is_some() {
            doc.signature = None;
            continue;
        }
        match ev {
            Event::Text(t) => {
                for c in t.chars() {
                    if !c.is_control() {
                        doc.buffer.edit_each(|b| b.type_char(c));
                        if !doc.lsp_active || doc.buffer.has_multi() {
                            continue;
                        }
                        if c == '(' || c == ',' {
                            actions.push(EditorAction::SignatureHelp);
                        } else if c == ')' {
                            doc.signature = None;
                        } else if doc.signature.is_some() {
                            actions.push(EditorAction::SignatureHelp);
                        }
                        let ident = c.is_alphanumeric() || c == '_';
                        if doc.lsp_triggers.contains(&c) {
                            doc.completion = None;
                            actions.push(EditorAction::Complete { trigger: Some(c) });
                        } else if ident && doc.completion.is_none() {
                            actions.push(EditorAction::Complete { trigger: None });
                        } else if !ident {
                            doc.completion = None;
                        }
                    }
                }
                moved = true;
            }
            Event::Paste(t) => {
                if doc.buffer.has_multi() {
                    doc.buffer.paste_multi(&t);
                } else {
                    doc.buffer.insert(&t);
                }
                doc.buffer.seal_history();
                moved = true;
            }
            Event::Copy | Event::Cut => {
                let cut = matches!(ev, Event::Cut);
                if doc.buffer.has_multi() {
                    ui.ctx().copy_text(doc.buffer.selected_texts().join("\n"));
                    if cut {
                        doc.buffer.edit_each(|b| {
                            if !b.sel.is_empty() {
                                b.backspace()
                            }
                        });
                    }
                } else if doc.buffer.sel.is_empty() {
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
                doc.buffer.edit_each(|b| b.insert(&t));
                moved = true;
            }
            Event::Key { key: Key::Space, pressed: true, modifiers, .. } if modifiers.ctrl => {
                doc.completion = None;
                actions.push(EditorAction::Complete { trigger: None });
            }
            Event::Key { key: Key::F12, pressed: true, .. } => {
                actions.push(EditorAction::GotoDefinition { idx: doc.buffer.sel.head });
            }
            Event::Key { key: Key::F8, pressed: true, .. } => {
                doc.next_diagnostic();
            }
            Event::Key { key, pressed: true, modifiers, .. } if handle_key(doc, key, modifiers, mac, page) => {
                if !matches!(key, Key::Backspace) {
                    doc.completion = None;
                }
                moved = true;
            }
            _ => {}
        }
    }
    (moved, actions)
}

fn handle_key(doc: &mut Document, key: Key, mods: Modifiers, mac: bool, page: isize) -> bool {
    if let Some(r) = multi_key(doc, key, mods, mac) {
        return r;
    }
    let b = &mut doc.buffer;
    // Multi-cursor creation.
    match key {
        Key::ArrowUp if mods.command && mods.alt => {
            b.add_cursor_vertical(false);
            return true;
        }
        Key::ArrowDown if mods.command && mods.alt => {
            b.add_cursor_vertical(true);
            return true;
        }
        Key::F2 if mods.command => {
            b.select_all_occurrences();
            return true;
        }
        _ => {}
    }
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
        Key::ArrowUp if doc.wrap_cols > 0 => move_visual(doc, -1, shift),
        Key::ArrowDown if doc.wrap_cols > 0 => move_visual(doc, 1, shift),
        Key::ArrowUp => {
            b.move_vertical(-1, shift);
            skip_folded(doc, -1, shift);
        }
        Key::ArrowDown => {
            b.move_vertical(1, shift);
            skip_folded(doc, 1, shift);
        }
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
        Key::OpenBracket | Key::OpenCurlyBracket if cmd && (shift || key == Key::OpenCurlyBracket) => {
            doc.fold_at_cursor();
            return true;
        }
        Key::CloseBracket | Key::CloseCurlyBracket if cmd && (shift || key == Key::CloseCurlyBracket) => {
            doc.unfold_at_cursor();
            return true;
        }
        Key::OpenBracket if cmd => b.indent_lines(false),
        Key::CloseBracket if cmd => b.indent_lines(true),
        _ => return false,
    }
    true
}

/// Up/down by screen row when lines are soft-wrapped.
fn move_visual(doc: &mut Document, dir: isize, shift: bool) {
    let map = doc.fold_map();
    let tab = doc.buffer.tab_size.max(1);
    let pos = doc.buffer.cursor_pos();
    let row = map.row_of_pos(pos.line, pos.col);
    let target = row as isize + dir;
    if target < 0 || target as usize >= map.row_count() {
        let idx = if target < 0 { 0 } else { doc.buffer.len_chars() };
        doc.buffer.set_cursor(idx, shift);
        return;
    }
    let target = target as usize;
    let (s0, _) = map.row_cols(row);
    let cur_chars = line_chars(doc, pos.line);
    let cvc = visual_cols(&cur_chars, tab);
    let x = cvc[pos.col.min(cur_chars.len())] - cvc[s0.min(cur_chars.len())];
    let line = map.line_at(target);
    let chars = line_chars(doc, line);
    let vc = visual_cols(&chars, tab);
    let (ts, te) = map.row_cols(target);
    let ts = ts.min(chars.len());
    let te = if te == usize::MAX { chars.len() } else { te.saturating_sub(1).max(ts) };
    let col = (ts..=te).find(|&c| vc[c] - vc[ts] >= x).unwrap_or(te);
    let idx = doc.buffer.pos_to_char(Position::new(line, col));
    doc.buffer.set_cursor(idx, shift);
}

/// Keep vertical movement out of folded regions.
fn skip_folded(doc: &mut Document, dir: isize, shift: bool) {
    if doc.folds.is_empty() {
        return;
    }
    let map = doc.fold_map();
    let mut guard = 0;
    while map.is_hidden(doc.buffer.cursor_pos().line) && guard < 1_000_000 {
        doc.buffer.move_vertical(dir, shift);
        guard += 1;
    }
}

/// Ctrl/Cmd+D: select the word under the caret, then add the next match as another cursor.
fn select_next_occurrence(doc: &mut Document) {
    if doc.buffer.add_next_occurrence() {
        doc.reveal_line = Some(doc.buffer.cursor_pos().line);
    }
}

/// Keys handled specially when several cursors are active. Returns None to
/// fall through to single-cursor handling.
fn multi_key(doc: &mut Document, key: Key, mods: Modifiers, mac: bool) -> Option<bool> {
    let b = &mut doc.buffer;
    if !b.has_multi() {
        return None;
    }
    let shift = mods.shift;
    let word = if mac { mods.alt } else { mods.ctrl };
    let line_mod = mac && mods.mac_cmd;
    match key {
        Key::Escape => b.clear_extra(),
        Key::ArrowLeft if line_mod => b.move_each(|b| b.move_home(shift)),
        Key::ArrowRight if line_mod => b.move_each(|b| b.move_end(shift)),
        Key::ArrowLeft if word => b.move_each(|b| b.move_word_left(shift)),
        Key::ArrowRight if word => b.move_each(|b| b.move_word_right(shift)),
        Key::ArrowLeft if !mods.alt => b.move_each(|b| b.move_left(shift)),
        Key::ArrowRight if !mods.alt => b.move_each(|b| b.move_right(shift)),
        Key::ArrowUp if !mods.alt && !mods.command => b.move_each(|b| b.move_vertical(-1, shift)),
        Key::ArrowDown if !mods.alt && !mods.command => b.move_each(|b| b.move_vertical(1, shift)),
        Key::Home => b.move_each(|b| b.move_home(shift)),
        Key::End => b.move_each(|b| b.move_end(shift)),
        Key::Backspace if word => b.edit_each(|b| b.delete_word_left()),
        Key::Backspace => b.edit_each(|b| b.backspace()),
        Key::Delete if word => b.edit_each(|b| b.delete_word_right()),
        Key::Delete => b.edit_each(|b| b.delete()),
        Key::Enter if !mods.command => b.edit_each(|b| b.newline()),
        Key::Tab if shift => b.edit_each(|b| b.indent_lines(false)),
        Key::Tab => b.edit_each(|b| b.tab()),
        _ => return None,
    }
    Some(true)
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
