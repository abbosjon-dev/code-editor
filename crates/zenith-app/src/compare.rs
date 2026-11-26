//! Compare two files (or a file with the clipboard) side by side. Open
//! documents are compared as edited, so the diff follows unsaved changes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, RichText};

use crate::document::Document;
use crate::git::{side_by_side, DiffRow};
use crate::theme::Palette;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    File(PathBuf),
    Text { label: String, text: String },
}

impl Source {
    pub fn label(&self, root: Option<&Path>) -> String {
        match self {
            Source::File(p) => {
                root.and_then(|r| p.strip_prefix(r).ok()).unwrap_or(p).to_string_lossy().replace('\\', "/")
            }
            Source::Text { label, .. } => label.clone(),
        }
    }
}

/// One side: where it comes from, its text and what that text was read at.
struct Side {
    src: Source,
    text: String,
    /// Document version, or file modification time (as nanoseconds).
    key: Option<u128>,
}

impl Side {
    fn new(src: Source) -> Self {
        let text = match &src {
            Source::Text { text, .. } => text.clone(),
            Source::File(_) => String::new(),
        };
        Self { src, text, key: None }
    }

    /// Re-read the text when it changed; true if it did.
    fn refresh(&mut self, docs: &[Document], check_disk: bool) -> bool {
        let Source::File(path) = &self.src else { return false };
        if let Some(d) = docs.iter().find(|d| d.path() == Some(path.as_path())) {
            let key = Some(d.id as u128 * 1_000_003 + d.buffer.version() as u128);
            if self.key != key {
                self.key = key;
                self.text = d.buffer.text();
                return true;
            }
            return false;
        }
        if self.key.is_some() && !check_disk {
            return false;
        }
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos())
            .unwrap_or(0);
        if self.key == Some(mtime) {
            return false;
        }
        self.key = Some(mtime);
        self.text = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).replace("\r\n", "\n")).unwrap_or_default();
        true
    }
}

pub enum Action {
    Open(PathBuf),
}

pub struct Compare {
    left: Side,
    right: Side,
    rows: Vec<DiffRow>,
    checked: Instant,
    jump: Option<usize>,
    first_jump: bool,
}

impl Compare {
    pub fn new(left: Source, right: Source) -> Self {
        Self {
            left: Side::new(left),
            right: Side::new(right),
            rows: vec![],
            checked: Instant::now(),
            jump: None,
            first_jump: true,
        }
    }

    /// Follow edits of open documents and changes on disk.
    pub fn refresh(&mut self, docs: &[Document]) {
        let disk = self.checked.elapsed() > Duration::from_secs(2);
        if disk {
            self.checked = Instant::now();
        }
        let a = self.left.refresh(docs, disk);
        let b = self.right.refresh(docs, disk);
        if a || b || (self.rows.is_empty() && !(self.left.text.is_empty() && self.right.text.is_empty())) {
            self.rows = side_by_side(&self.left.text, &self.right.text);
        }
    }

    fn changes(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(i, r)| r.changed && (*i == 0 || !self.rows[i - 1].changed))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        font_size: f32,
        root: Option<&Path>,
        open: &mut bool,
    ) -> Option<Action> {
        let mut action = None;
        let (la, lb) = (self.left.src.label(root), self.right.src.label(root));
        let screen = ctx.screen_rect();
        egui::Window::new(format!("Compare  {la} ↔ {lb}"))
            .id(egui::Id::new("zenith-compare"))
            .open(open)
            .default_size(screen.size() * egui::vec2(0.85, 0.8))
            .default_pos(screen.min + screen.size() * egui::vec2(0.075, 0.08))
            .collapsible(false)
            .show(ctx, |ui| {
                let changes = self.changes();
                let id = egui::Id::new("zenith-compare-rows");
                let top = ui.data(|d| d.get_temp::<usize>(id)).unwrap_or(0);
                let mut swap = false;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&la).color(p.error).strong());
                    ui.label(RichText::new("↔").color(p.text_dim));
                    ui.label(RichText::new(&lb).color(p.success).strong());
                    let n = changes.len();
                    let text = if n == 0 && !self.rows.is_empty() {
                        "identical".to_string()
                    } else {
                        format!("{n} change(s)")
                    };
                    ui.label(RichText::new(text).small().color(p.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(n > 0, egui::Button::new("Next ↓")).clicked() {
                            let t = changes.iter().find(|&&c| c > top + 3).or(changes.first());
                            self.jump = t.map(|&c| c.saturating_sub(3));
                        }
                        if ui.add_enabled(n > 0, egui::Button::new("Prev ↑")).clicked() {
                            let t = changes.iter().rev().find(|&&c| c + 3 < top).or(changes.last());
                            self.jump = t.map(|&c| c.saturating_sub(3));
                        }
                        if ui.button("↔ Swap").on_hover_text("Swap the two sides").clicked() {
                            swap = true;
                        }
                        for s in [&self.right.src, &self.left.src] {
                            if let Source::File(f) = s {
                                let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                if ui.button(format!("Open {name}")).clicked() {
                                    action = Some(Action::Open(f.clone()));
                                }
                            }
                        }
                    });
                });
                ui.separator();
                if swap {
                    std::mem::swap(&mut self.left, &mut self.right);
                    self.rows = side_by_side(&self.left.text, &self.right.text);
                }
                if self.first_jump && !changes.is_empty() {
                    self.first_jump = false;
                    self.jump = Some(changes[0].saturating_sub(3));
                }
                let old: Vec<&str> = self.left.text.lines().collect();
                let new: Vec<&str> = self.right.text.lines().collect();
                crate::diff_view::rows_ui(
                    ui,
                    id,
                    &self.rows,
                    &changes,
                    &old,
                    &new,
                    p,
                    font_size * 0.9,
                    self.jump.take(),
                    false,
                );
            });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_files_and_follows_edits() {
        let dir = std::env::temp_dir().join(format!("zenith-compare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&b, "one\nTWO\nthree\nfour\n").unwrap();
        let mut c = Compare::new(Source::File(a.clone()), Source::File(b.clone()));
        c.refresh(&[]);
        assert_eq!(c.changes().len(), 2); // two→TWO, and the added "four"
        assert!(c.rows.iter().filter(|r| r.changed).count() >= 2);
        // An open, edited document is compared as edited.
        let theme = crate::highlight::load_theme("OneHalfDark");
        let mut d = Document::open(1, &b, theme, 4).unwrap();
        d.buffer.select_all();
        d.buffer.insert("one\ntwo\nthree\n");
        c.refresh(std::slice::from_ref(&d));
        assert!(c.changes().is_empty());
        assert_eq!(Source::File(a).label(Some(&dir)), "a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
