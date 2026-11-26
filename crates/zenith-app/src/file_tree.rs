//! Workspace explorer (lazy directory tree).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, FontId, Rect, Sense};

use crate::theme::{file_icon, Palette};

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

pub enum TreeAction {
    Open(PathBuf),
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
    RevealInTerminal(PathBuf),
    CopyPath(PathBuf),
    SelectForCompare(PathBuf),
    CompareWithSelected(PathBuf),
}

pub struct FileTree {
    pub root: PathBuf,
    expanded: HashSet<PathBuf>,
    cache: HashMap<PathBuf, Vec<Entry>>,
    pub selected: Option<PathBuf>,
    /// Git status letter per changed file ('M', 'U', 'A', 'D', ...).
    pub git: HashMap<PathBuf, char>,
    /// Folders that contain changed files.
    git_dirs: HashSet<PathBuf>,
    /// A file picked with "Select for Compare".
    pub compare_base: Option<PathBuf>,
}

const HIDDEN: &[&str] = &[".git", ".DS_Store", "Thumbs.db"];

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        Self {
            root,
            expanded,
            cache: HashMap::new(),
            selected: None,
            git: HashMap::new(),
            git_dirs: HashSet::new(),
            compare_base: None,
        }
    }

    pub fn set_git_status(&mut self, status: HashMap<PathBuf, char>) {
        self.git_dirs.clear();
        for path in status.keys() {
            let mut cur = path.parent();
            while let Some(d) = cur {
                if !self.git_dirs.insert(d.to_path_buf()) || !d.starts_with(&self.root) {
                    break;
                }
                cur = d.parent();
            }
        }
        self.git = status;
    }

    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn refresh_dir(&mut self, dir: &Path) {
        self.cache.remove(dir);
    }

    /// Expand parents so `path` becomes visible and select it.
    pub fn reveal(&mut self, path: &Path) {
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if !dir.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            cur = dir.parent();
        }
        self.selected = Some(path.to_path_buf());
    }

    fn children(&mut self, dir: &Path) -> Vec<Entry> {
        if let Some(c) = self.cache.get(dir) {
            return c.clone();
        }
        let mut out: Vec<Entry> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if HIDDEN.contains(&name.as_str()) {
                            return None;
                        }
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false) || e.path().is_dir();
                        Some(Entry { path: e.path(), name, is_dir })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        self.cache.insert(dir.to_path_buf(), out.clone());
        out
    }

    /// Just the rows (no scroll area), for stacking several roots in one view.
    pub fn contents_ui(&mut self, ui: &mut egui::Ui, p: &Palette, active_file: Option<&Path>) -> Option<TreeAction> {
        let mut action = None;
        let root = self.root.clone();
        ui.spacing_mut().item_spacing.y = 0.0;
        self.dir_ui(ui, p, &root, 0, active_file, &mut action);
        action
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, p: &Palette, active_file: Option<&Path>) -> Option<TreeAction> {
        let mut action = None;
        let root = self.root.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            self.dir_ui(ui, p, &root, 0, active_file, &mut action);
            // Empty area context menu → create in root.
            let rest =
                ui.allocate_response(egui::vec2(ui.available_width(), ui.available_height().max(40.0)), Sense::click());
            rest.context_menu(|ui| {
                if ui.button("New File…").clicked() {
                    action = Some(TreeAction::NewFile(root.clone()));
                    ui.close_menu();
                }
                if ui.button("New Folder…").clicked() {
                    action = Some(TreeAction::NewFolder(root.clone()));
                    ui.close_menu();
                }
                if ui.button("Refresh").clicked() {
                    self.refresh();
                    ui.close_menu();
                }
            });
        });
        action
    }

    fn dir_ui(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        dir: &Path,
        depth: usize,
        active: Option<&Path>,
        action: &mut Option<TreeAction>,
    ) {
        for entry in self.children(dir) {
            let row_h = 24.0;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
            let is_open = entry.is_dir && self.expanded.contains(&entry.path);
            let is_active = active == Some(entry.path.as_path());
            let is_sel = self.selected.as_ref() == Some(&entry.path);
            if is_active || is_sel {
                ui.painter().rect_filled(rect, 4.0, if is_active { p.accent_soft } else { p.bg_active });
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 4.0, p.bg_hover);
            }
            let x = rect.min.x + 8.0 + depth as f32 * 14.0;
            let cy = rect.center().y;
            // Indent guide lines.
            for d in 0..depth {
                let gx = rect.min.x + 14.0 + d as f32 * 14.0;
                ui.painter().vline(gx, rect.y_range(), egui::Stroke::new(1.0_f32, p.indent_guide));
            }
            let font = FontId::proportional(ui.style().text_styles[&egui::TextStyle::Body].size);
            if entry.is_dir {
                ui.painter().text(
                    egui::pos2(x + 5.0, cy),
                    Align2::CENTER_CENTER,
                    if is_open { "⏷" } else { "⏵" },
                    FontId::proportional(10.0),
                    p.text_dim,
                );
                ui.painter().text(
                    egui::pos2(x + 20.0, cy),
                    Align2::CENTER_CENTER,
                    if is_open { "🗁" } else { "🗀" },
                    FontId::proportional(13.0),
                    p.accent,
                );
            } else {
                let (glyph, color) = file_icon(&entry.name);
                ui.painter().text(
                    egui::pos2(x + 20.0, cy),
                    Align2::CENTER_CENTER,
                    glyph,
                    FontId::proportional(11.0),
                    color,
                );
            }
            let git = self.git.get(&entry.path).copied();
            let git_color = |c: char| match c {
                'U' | 'A' => p.success,
                'D' => p.error,
                _ => p.warning,
            };
            let mut text_rect = Rect::from_min_max(egui::pos2(x + 32.0, rect.min.y), rect.max);
            if let Some(letter) = git {
                ui.painter().text(
                    egui::pos2(rect.max.x - 10.0, cy),
                    Align2::RIGHT_CENTER,
                    letter.to_string(),
                    FontId::monospace(11.0),
                    git_color(letter),
                );
                text_rect.max.x -= 22.0;
            }
            let name_color = if let Some(letter) = git {
                git_color(letter)
            } else if entry.is_dir && self.git_dirs.contains(&entry.path) {
                p.warning.gamma_multiply(0.85)
            } else if entry.name.starts_with('.') {
                p.text_dim
            } else {
                p.text
            };
            ui.painter().with_clip_rect(text_rect).text(
                egui::pos2(text_rect.min.x, cy),
                Align2::LEFT_CENTER,
                &entry.name,
                font,
                name_color,
            );

            if resp.clicked() {
                self.selected = Some(entry.path.clone());
                if entry.is_dir {
                    if is_open {
                        self.expanded.remove(&entry.path);
                    } else {
                        self.expanded.insert(entry.path.clone());
                    }
                } else {
                    *action = Some(TreeAction::Open(entry.path.clone()));
                }
            }
            let target_dir = if entry.is_dir { entry.path.clone() } else { dir.to_path_buf() };
            resp.context_menu(|ui| {
                if ui.button("New File…").clicked() {
                    *action = Some(TreeAction::NewFile(target_dir.clone()));
                    ui.close_menu();
                }
                if ui.button("New Folder…").clicked() {
                    *action = Some(TreeAction::NewFolder(target_dir.clone()));
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Rename…").clicked() {
                    *action = Some(TreeAction::Rename(entry.path.clone()));
                    ui.close_menu();
                }
                if ui.button("Delete").clicked() {
                    *action = Some(TreeAction::Delete(entry.path.clone()));
                    ui.close_menu();
                }
                if !entry.is_dir {
                    ui.separator();
                    if ui.button("Select for Compare").clicked() {
                        *action = Some(TreeAction::SelectForCompare(entry.path.clone()));
                        ui.close_menu();
                    }
                    if let Some(base) = self.compare_base.as_ref().filter(|b| **b != entry.path) {
                        let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if ui.button(format!("Compare with '{name}'")).clicked() {
                            *action = Some(TreeAction::CompareWithSelected(entry.path.clone()));
                            ui.close_menu();
                        }
                    }
                }
                ui.separator();
                if ui.button("Copy Path").clicked() {
                    *action = Some(TreeAction::CopyPath(entry.path.clone()));
                    ui.close_menu();
                }
                if ui.button("Open in Terminal").clicked() {
                    *action = Some(TreeAction::RevealInTerminal(target_dir.clone()));
                    ui.close_menu();
                }
            });
            if entry.is_dir && self.expanded.contains(&entry.path) {
                self.dir_ui(ui, p, &entry.path, depth + 1, active, action);
            }
        }
    }
}
