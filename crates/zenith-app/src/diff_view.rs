//! Side-by-side diff of a file against its HEAD version.

use std::rc::Rc;

use eframe::egui::{self, Align2, Color32, FontId, Rect, RichText, Sense, Stroke};

use crate::document::Document;
use crate::git::{side_by_side, DiffRow};
use crate::theme::Palette;

pub enum DiffAction {
    Close,
    /// Stage the change that starts at this line of the file.
    StageHunk(usize),
    /// Restore the committed lines of the change at this line.
    RevertHunk(usize),
}

/// A button on one change of a diff.
pub enum RowAction {
    Stage(usize),
    Revert(usize),
}

/// Cached alignment for a document version.
#[derive(Default)]
pub struct DiffState {
    cache: Option<(u64, Rc<Vec<DiffRow>>)>,
    /// Row to scroll to on the next frame.
    jump_to: Option<usize>,
}

impl DiffState {
    fn rows(&mut self, v: u64, base: &str, current: &str) -> Rc<Vec<DiffRow>> {
        if let Some((cv, rows)) = &self.cache {
            if *cv == v {
                return rows.clone();
            }
        }
        let rows = Rc::new(side_by_side(base, current));
        self.cache = Some((v, rows.clone()));
        rows
    }
}

pub fn ui(ui: &mut egui::Ui, doc: &mut Document, p: &Palette, font_size: f32) -> Option<DiffAction> {
    let mut action = None;
    let Some(base) = doc.git_base.clone() else {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("This file is not in the last commit (untracked or new).").color(p.text_dim));
            if ui.button("Back to editor").clicked() {
                action = Some(DiffAction::Close);
            }
        });
        return action;
    };
    let new_text = doc.buffer.text();
    let rows = doc.git_diff.rows(doc.buffer.version(), &base, &new_text);
    let changes: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(i, r)| r.changed && (*i == 0 || !rows[i - 1].changed))
        .map(|(i, _)| i)
        .collect();
    let old_lines: Vec<&str> = base.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    // Header.
    let mut jump: Option<isize> = None;
    egui::Frame::new().fill(p.bg_panel).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("HEAD").color(p.error).strong());
            ui.label(RichText::new("↔").color(p.text_dim));
            ui.label(RichText::new("Working tree").color(p.success).strong());
            ui.label(RichText::new(format!("{} change(s)", changes.len())).small().color(p.text_dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Open file").on_hover_text("Back to the editor").clicked() {
                    action = Some(DiffAction::Close);
                }
                if ui.add_enabled(!changes.is_empty(), egui::Button::new("Next ↓")).clicked() {
                    jump = Some(1);
                }
                if ui.add_enabled(!changes.is_empty(), egui::Button::new("Prev ↑")).clicked() {
                    jump = Some(-1);
                }
            });
        });
    });

    let id = ui.make_persistent_id(("git-diff", doc.id));
    let current_top = ui.data(|d| d.get_temp::<usize>(id)).unwrap_or(0);
    if let Some(dir) = jump {
        let target = if dir > 0 {
            changes.iter().find(|&&c| c > current_top + 1).or(changes.first())
        } else {
            changes.iter().rev().find(|&&c| c + 1 < current_top).or(changes.last())
        };
        doc.git_diff.jump_to = target.map(|&c| c.saturating_sub(3));
    }
    match rows_ui(ui, id, &rows, &changes, &old_lines, &new_lines, p, font_size, doc.git_diff.jump_to.take(), true) {
        Some(RowAction::Stage(l)) => action = Some(DiffAction::StageHunk(l)),
        Some(RowAction::Revert(l)) => action = Some(DiffAction::RevertHunk(l)),
        None => {}
    }
    action
}

/// Paint aligned rows side by side (old left, new right) in a scroll area
/// with `id`; the first visible row is stored under `id` in temp data.
#[allow(clippy::too_many_arguments)]
pub fn rows_ui(
    ui: &mut egui::Ui,
    id: egui::Id,
    rows: &[DiffRow],
    changes: &[usize],
    old_lines: &[&str],
    new_lines: &[&str],
    p: &Palette,
    font_size: f32,
    jump_to: Option<usize>,
    hunk_buttons: bool,
) -> Option<RowAction> {
    let mut action = None;
    let font = FontId::monospace(font_size);
    let row_h = (font_size * 1.45).round();
    let char_w = crate::theme::mono_metrics(ui, &font).0;
    let mut area = egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
    if let Some(r) = jump_to {
        area = area.vertical_scroll_offset(r as f32 * row_h);
    }
    let digits = rows.len().max(10).to_string().len();
    let num_w = digits as f32 * char_w + 16.0;
    let out = area.show_rows(ui, row_h, rows.len(), |ui, range| {
        let full_w = ui.available_width();
        let half = (full_w / 2.0).floor();
        ui.data_mut(|d| d.insert_temp(id, range.start));
        for i in range {
            let r = rows[i];
            let (rect, _) = ui.allocate_exact_size(egui::vec2(full_w, row_h), Sense::hover());
            let left = Rect::from_min_size(rect.min, egui::vec2(half, row_h));
            let right =
                Rect::from_min_size(egui::pos2(rect.min.x + half, rect.min.y), egui::vec2(full_w - half, row_h));
            let painter = ui.painter();
            for (side, line, lines, color) in
                [(left, r.old, &old_lines, p.error), (right, r.new, &new_lines, p.success)]
            {
                match line {
                    Some(n) => {
                        if r.changed {
                            painter.rect_filled(side, 0.0, color.gamma_multiply(0.14));
                        }
                        painter.text(
                            egui::pos2(side.min.x + num_w - 8.0, side.center().y),
                            Align2::RIGHT_CENTER,
                            (n + 1).to_string(),
                            FontId::monospace(font_size * 0.85),
                            p.text_faint,
                        );
                        let text = lines.get(n).copied().unwrap_or("").replace('\t', "    ");
                        painter.with_clip_rect(side.shrink2(egui::vec2(2.0, 0.0))).text(
                            egui::pos2(side.min.x + num_w, side.center().y),
                            Align2::LEFT_CENTER,
                            text,
                            font.clone(),
                            if r.changed { p.text } else { p.text_dim },
                        );
                    }
                    None => {
                        // Hatched filler where the other side has lines.
                        painter.rect_filled(side, 0.0, p.bg_panel);
                        let mut x = side.min.x - row_h;
                        while x < side.max.x {
                            painter.line_segment(
                                [egui::pos2(x, side.max.y), egui::pos2(x + row_h, side.min.y)],
                                Stroke::new(1.0_f32, p.border),
                            );
                            x += 8.0;
                        }
                    }
                }
            }
            painter.vline(rect.min.x + half, rect.y_range(), Stroke::new(1.0_f32, p.border));
            // Buttons on the first row of each change.
            if hunk_buttons && r.changed && (i == 0 || !rows[i - 1].changed) {
                // The change's first line in the file (after a deletion: the line that follows).
                let line = rows[i..].iter().find_map(|r| r.new).unwrap_or(new_lines.len());
                let at = egui::pos2(rect.max.x - 150.0, rect.min.y);
                let r = Rect::from_min_size(at, egui::vec2(146.0, row_h));
                ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                    ui.horizontal(|ui| {
                        if ui.small_button("+ Stage").on_hover_text("Stage only this change").clicked() {
                            action = Some(RowAction::Stage(line));
                        }
                        if ui
                            .small_button(RichText::new("↺ Revert").color(p.error))
                            .on_hover_text("Restore the committed lines")
                            .clicked()
                        {
                            action = Some(RowAction::Revert(line));
                        }
                    });
                });
            }
        }
    });
    // Overview of change positions on the right edge.
    let bar = Rect::from_min_max(egui::pos2(out.inner_rect.max.x - 6.0, out.inner_rect.min.y), out.inner_rect.max);
    if !rows.is_empty() {
        for &c in changes {
            let y = bar.min.y + bar.height() * c as f32 / rows.len() as f32;
            ui.painter().rect_filled(
                Rect::from_min_size(egui::pos2(bar.min.x, y), egui::vec2(4.0, 3.0)),
                1.0,
                Color32::from_rgb(0xe8, 0xb0, 0x4a),
            );
        }
    }
    action
}
