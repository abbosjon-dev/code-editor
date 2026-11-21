//! Reviewing changes made to open files by other programs — typically an AI
//! agent working in the terminal. The new content is loaded immediately, and
//! the previous version is kept so each change can be inspected and reverted.

use eframe::egui::{self, Color32, FontId, RichText, Stroke};
use similar::{DiffOp, TextDiff};

use crate::document::Document;
use crate::theme::Palette;

/// Pending review of an external change.
pub struct Review {
    /// Content before the external change(s).
    pub base: String,
    /// Show the diff view instead of the editor.
    pub show: bool,
    /// Hunks for a buffer version (diffing every frame is too slow for big files).
    cache: std::cell::RefCell<Option<(u64, std::rc::Rc<Vec<Hunk>>)>>,
}

impl Review {
    pub fn new(base: String) -> Self {
        Self { base, show: false, cache: Default::default() }
    }

    pub fn hunks(&self, doc_version: u64, current: impl FnOnce() -> String) -> std::rc::Rc<Vec<Hunk>> {
        let mut c = self.cache.borrow_mut();
        if let Some((v, h)) = c.as_ref() {
            if *v == doc_version {
                return h.clone();
            }
        }
        let h = std::rc::Rc::new(hunks(&self.base, &current()));
        *c = Some((doc_version, h.clone()));
        h
    }
}

/// A changed region: lines `old` in the base were replaced by lines `new`.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: Vec<String>,
    pub new_start: usize,
    pub new_lines: Vec<String>,
}

pub fn hunks(base: &str, current: &str) -> Vec<Hunk> {
    let diff = TextDiff::configure().timeout(std::time::Duration::from_millis(200)).diff_lines(base, current);
    let old: Vec<&str> = split_lines(base);
    let new: Vec<&str> = split_lines(current);
    let mut out: Vec<Hunk> = vec![];
    for op in diff.ops() {
        let (o, ol, n, nl) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_len, new_index, 0),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, 0, new_index, new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_len, new_index, new_len),
        };
        // Merge with the previous op when adjacent.
        if let Some(last) = out.last_mut() {
            if last.old_start + last.old_lines.len() == o && last.new_start + last.new_lines.len() == n {
                last.old_lines.extend(old[o..o + ol].iter().map(|s| s.to_string()));
                last.new_lines.extend(new[n..n + nl].iter().map(|s| s.to_string()));
                continue;
            }
        }
        out.push(Hunk {
            old_start: o,
            old_lines: old[o..o + ol].iter().map(|s| s.to_string()).collect(),
            new_start: n,
            new_lines: new[n..n + nl].iter().map(|s| s.to_string()).collect(),
        });
    }
    out
}

/// Split keeping line terminators, so joining reproduces the text exactly.
fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// `current` with one hunk reverted to its base content.
pub fn revert_hunk(current: &str, hunk: &Hunk) -> String {
    let lines = split_lines(current);
    let mut out = String::with_capacity(current.len());
    for l in &lines[..hunk.new_start.min(lines.len())] {
        out.push_str(l);
    }
    for l in &hunk.old_lines {
        out.push_str(l);
    }
    for l in &lines[(hunk.new_start + hunk.new_lines.len()).min(lines.len())..] {
        out.push_str(l);
    }
    out
}

pub enum ReviewAction {
    Toggle,
    KeepAll,
    RevertAll,
    RevertHunk(usize),
    /// Dirty buffer, file changed on disk: replace buffer with disk content.
    LoadDisk,
    /// Dirty buffer, file changed on disk: ignore the disk version.
    KeepMine,
}

/// Banner shown above the editor. Returns the user's choice.
pub fn banner(ui: &mut egui::Ui, doc: &Document, p: &Palette, conflict: bool) -> Option<ReviewAction> {
    let mut action = None;
    let n = doc.review.as_ref().map(|r| r.hunks(doc.buffer.version(), || doc.buffer.text()).len()).unwrap_or(0);
    if !conflict && n == 0 {
        return None;
    }
    egui::Frame::new()
        .fill(p.accent_soft.gamma_multiply(0.6))
        .stroke(Stroke::new(1.0_f32, p.accent_soft))
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if conflict {
                    ui.label(RichText::new("⚠").color(p.warning));
                    ui.label("This file changed on disk while you have unsaved edits.");
                    if ui.button("Load disk version").clicked() {
                        action = Some(ReviewAction::LoadDisk);
                    }
                    if ui.button("Keep mine").clicked() {
                        action = Some(ReviewAction::KeepMine);
                    }
                    return;
                }
                ui.label(RichText::new("✳").color(p.accent));
                ui.label(format!(
                    "Changed outside the editor (e.g. by an AI agent): {n} change{}",
                    if n == 1 { "" } else { "s" }
                ));
                let showing = doc.review.as_ref().is_some_and(|r| r.show);
                if ui.button(if showing { "Back to editor" } else { "Review diff" }).clicked() {
                    action = Some(ReviewAction::Toggle);
                }
                if ui.add(egui::Button::new(RichText::new("✓ Keep").color(Color32::WHITE)).fill(p.success)).clicked()
                {
                    action = Some(ReviewAction::KeepAll);
                }
                if ui
                    .add(egui::Button::new(RichText::new("↺ Revert all").color(Color32::WHITE)).fill(p.error))
                    .clicked()
                {
                    action = Some(ReviewAction::RevertAll);
                }
            });
        });
    action
}

/// Unified diff view with a revert button per change.
pub fn diff_view(ui: &mut egui::Ui, doc: &Document, p: &Palette, font_size: f32) -> Option<ReviewAction> {
    let review = doc.review.as_ref()?;
    let current = doc.buffer.text();
    let hs = review.hunks(doc.buffer.version(), || current.clone());
    let new_lines = split_lines(&current);
    let mut action = None;
    let font = FontId::monospace(font_size);
    let add_bg = p.success.gamma_multiply(0.18);
    let del_bg = p.error.gamma_multiply(0.18);
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(8.0);
        for (i, h) in hs.iter().enumerate() {
            egui::Frame::new()
                .fill(p.bg_elevated)
                .corner_radius(8)
                .stroke(Stroke::new(1.0_f32, p.border))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width() - 20.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Line {}  ·  −{} +{}",
                                h.new_start + 1,
                                h.old_lines.len(),
                                h.new_lines.len()
                            ))
                            .color(p.text_dim)
                            .size(12.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("↺ Revert this change").clicked() {
                                action = Some(ReviewAction::RevertHunk(i));
                            }
                        });
                    });
                    let line = |ui: &mut egui::Ui, sign: &str, text: &str, color: Color32, bg: Color32| {
                        let t = format!("{sign} {}", text.trim_end_matches(['\n', '\r']));
                        egui::Frame::new().fill(bg).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(RichText::new(t).font(font.clone()).color(color));
                        });
                    };
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // Context before.
                    for l in new_lines.iter().take(h.new_start).skip(h.new_start.saturating_sub(2)) {
                        line(ui, " ", l, p.text_dim, Color32::TRANSPARENT);
                    }
                    for l in &h.old_lines {
                        line(ui, "−", l, p.error, del_bg);
                    }
                    for l in &h.new_lines {
                        line(ui, "+", l, p.success, add_bg);
                    }
                    let after = h.new_start + h.new_lines.len();
                    for l in new_lines.iter().skip(after).take(2) {
                        line(ui, " ", l, p.text_dim, Color32::TRANSPARENT);
                    }
                });
            ui.add_space(8.0);
        }
        if hs.is_empty() {
            ui.label(RichText::new("No differences.").color(p.text_dim));
        }
    });
    action
}

/// Apply a review action to a document. May write the file.
pub fn apply(doc: &mut Document, action: ReviewAction) -> std::io::Result<()> {
    match action {
        ReviewAction::Toggle => {
            if let Some(r) = &mut doc.review {
                r.show = !r.show;
            }
        }
        ReviewAction::KeepAll => doc.review = None,
        ReviewAction::RevertAll => {
            if let Some(r) = doc.review.take() {
                set_and_save(doc, &r.base)?;
            }
        }
        ReviewAction::RevertHunk(i) => {
            let Some(base) = doc.review.as_ref().map(|r| r.base.clone()) else { return Ok(()) };
            let current = doc.buffer.text();
            if let Some(h) = hunks(&base, &current).get(i) {
                let text = revert_hunk(&current, h);
                set_and_save(doc, &text)?;
            }
            if doc.buffer.text() == base {
                doc.review = None;
            }
        }
        ReviewAction::LoadDisk => doc.reload_from_disk()?,
        ReviewAction::KeepMine => {
            // Accept the disk timestamp so we stop asking; saving will overwrite.
            doc.disk_mtime = doc.path().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        }
    }
    Ok(())
}

fn set_and_save(doc: &mut Document, text: &str) -> std::io::Result<()> {
    let sel = doc.buffer.sel;
    doc.buffer.set_text(text);
    doc.buffer.sel = sel;
    doc.buffer.sel.clamp(doc.buffer.len_chars());
    doc.buffer.clear_extra();
    if doc.path().is_some() {
        doc.save(false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_and_revert() {
        let base = "a\nb\nc\nd\ne\n";
        let cur = "a\nB\nc\nd\nnew\ne\n";
        let hs = hunks(base, cur);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].old_lines, vec!["b\n"]);
        assert_eq!(hs[0].new_lines, vec!["B\n"]);
        assert_eq!(hs[1].new_lines, vec!["new\n"]);
        let reverted = revert_hunk(cur, &hs[1]);
        assert_eq!(reverted, "a\nB\nc\nd\ne\n");
        let hs2 = hunks(base, &reverted);
        assert_eq!(revert_hunk(&reverted, &hs2[0]), base);
    }

    #[test]
    fn deletion_hunk() {
        let base = "1\n2\n3\n";
        let cur = "1\n3\n";
        let hs = hunks(base, cur);
        assert_eq!(hs.len(), 1);
        assert_eq!(revert_hunk(cur, &hs[0]), base);
    }
}
