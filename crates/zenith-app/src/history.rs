//! The Git History window: commit graph and list, a commit's message and
//! files, a file's diff in that commit, and comparing two commits.
//! All git work runs on threads.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, Sense, Stroke};

use crate::git::Runner;
use crate::git_log::{self, Commit, Row};
use crate::theme::Palette;

type Files = Vec<(char, String, String)>;

/// A file's diff: path, aligned rows, old and new text.
struct Diff {
    path: String,
    rows: Vec<crate::git::DiffRow>,
    old: String,
    new: String,
}

pub struct History {
    run: Runner,
    /// Only this file's history (path relative to the repository).
    pub file: Option<String>,
    pub title: String,
    commits: Vec<Commit>,
    rows: Vec<Row>,
    log_rx: Option<Receiver<Result<Vec<Commit>, String>>>,
    error: Option<String>,
    selected: Option<usize>,
    /// A second commit (Ctrl+click): compare it with the selected one.
    compare: Option<usize>,
    files: Files,
    message: String,
    files_rx: Option<Receiver<(Result<Files, String>, String)>>,
    diff: Option<Diff>,
    diff_rx: Option<Receiver<Diff>>,
    filter: String,
    /// Scroll the diff to its first change once (it just arrived).
    diff_jump: bool,
    waker: Arc<dyn Fn() + Send + Sync>,
    /// Select this commit once the log has loaded.
    pub select_hash: Option<String>,
    scroll_to_selected: bool,
}

/// What the window asks the app to do.
pub enum Action {
    /// Open the working copy of this file (path relative to the repository).
    OpenFile(String),
}

const LANE_W: f32 = 12.0;

/// Branch / tag names, short: local ones first, `origin/x` dropped when
/// `x` is also local, at most three.
fn short_refs(refs: &str) -> String {
    let names: Vec<String> = refs.split(", ").map(|r| r.replace("HEAD -> ", "▸ ").replace("tag: ", "🏷 ")).collect();
    let local: Vec<&String> = names.iter().filter(|n| !n.contains('/') || n.starts_with("▸")).collect();
    let mut out: Vec<&str> = local.iter().map(|s| s.as_str()).collect();
    for n in &names {
        if let Some((_, b)) = n.split_once('/') {
            if !local.iter().any(|l| l.trim_start_matches("▸ ") == b) && !n.starts_with('▸') {
                out.push(n);
            }
        }
    }
    let more = out.len().saturating_sub(3);
    let mut s = out.into_iter().take(3).collect::<Vec<_>>().join(", ");
    if more > 0 {
        s.push_str(&format!(" +{more}"));
    }
    s
}

impl History {
    pub fn new(run: Runner, file: Option<String>, title: String, waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut h = Self {
            run,
            file,
            title,
            commits: vec![],
            rows: vec![],
            log_rx: None,
            error: None,
            selected: None,
            compare: None,
            files: vec![],
            message: String::new(),
            files_rx: None,
            diff: None,
            diff_rx: None,
            filter: String::new(),
            diff_jump: false,
            waker,
            select_hash: None,
            scroll_to_selected: false,
        };
        h.reload();
        h
    }

    fn reload(&mut self) {
        let (tx, rx) = channel();
        let (run, file, w) = (self.run.clone(), self.file.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let _ = tx.send(git_log::log(&run, file.as_deref(), 2000));
            w();
        });
        self.log_rx = Some(rx);
    }

    fn select(&mut self, i: usize, compare: bool) {
        if compare && self.selected.is_some() && self.selected != Some(i) {
            self.compare = Some(i);
        } else {
            self.selected = Some(i);
            self.compare = None;
        }
        self.diff = None;
        let (Some(a), b) = (self.selected, self.compare) else { return };
        // Compare: older → newer (the list is newest first).
        let (from, to) = match b {
            Some(b) => {
                let (old, new) = if a > b { (a, b) } else { (b, a) };
                (Some(self.commits[old].hash.clone()), self.commits[new].hash.clone())
            }
            None => (None, self.commits[a].hash.clone()),
        };
        let (tx, rx) = channel();
        let (run, w) = (self.run.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let files = git_log::changed_files(&run, from.as_deref(), &to);
            let msg = match &from {
                Some(f) => format!("Changes from {} to {}", &f[..f.len().min(8)], &to[..to.len().min(8)]),
                None => git_log::message(&run, &to),
            };
            let _ = tx.send((files, msg));
            w();
        });
        self.files_rx = Some(rx);
    }

    fn show_diff(&mut self, path: String, old_path: String) {
        let Some(a) = self.selected else { return };
        let (old_rev, new_rev) = match self.compare {
            Some(b) => {
                let (old, new) = if a > b { (a, b) } else { (b, a) };
                (self.commits[old].hash.clone(), self.commits[new].hash.clone())
            }
            None => {
                let c = &self.commits[a];
                // A root commit has no parent: everything is new.
                (c.parents.first().cloned().unwrap_or_default(), c.hash.clone())
            }
        };
        let (tx, rx) = channel();
        let (run, w) = (self.run.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let old = if old_rev.is_empty() { String::new() } else { git_log::file_at(&run, &old_rev, &old_path) };
            let new = git_log::file_at(&run, &new_rev, &path);
            let rows = crate::git::side_by_side(&old, &new);
            let _ = tx.send(Diff { path, rows, old, new });
            w();
        });
        self.diff_rx = Some(rx);
    }

    fn poll(&mut self) {
        if let Some(r) = self.log_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.log_rx = None;
            match r {
                Ok(c) => {
                    self.rows = git_log::lanes(&c);
                    self.commits = c;
                    self.error = None;
                    let want = self.select_hash.take();
                    self.scroll_to_selected = want.is_some();
                    let at = want.and_then(|h| self.commits.iter().position(|c| c.hash == h)).unwrap_or(0);
                    if !self.commits.is_empty() {
                        self.select(at, false);
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some((files, msg)) = self.files_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.files_rx = None;
            self.message = msg;
            match files {
                Ok(f) => {
                    // A file's history: show that file's diff right away.
                    let first = self.file.as_ref().and_then(|want| f.iter().find(|x| &x.1 == want).cloned());
                    self.files = f;
                    if let Some((_, p, o)) = first {
                        self.show_diff(p, o);
                    }
                }
                Err(e) => self.message = e,
            }
        }
        if let Some(d) = self.diff_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.diff_rx = None;
            self.diff = Some(d);
            self.diff_jump = true;
        }
    }

    /// Draw the window; `open` is cleared when it's closed.
    pub fn ui(&mut self, ctx: &egui::Context, p: &Palette, font_size: f32, open: &mut bool) -> Option<Action> {
        self.poll();
        let mut action = None;
        let mut still_open = true;
        egui::Window::new(format!("Git History — {}", self.title))
            .id(egui::Id::new("git-history"))
            .open(&mut still_open)
            .default_size([
                1100.0_f32.min(ctx.screen_rect().width() - 40.0),
                640.0_f32.min(ctx.screen_rect().height() - 80.0),
            ])
            .max_width(ctx.screen_rect().width() - 20.0)
            .max_height(ctx.screen_rect().height() - 40.0)
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).color(p.error));
                    return;
                }
                if self.log_rx.is_some() && self.commits.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading history…");
                    });
                    return;
                }
                let h = ui.available_height();
                let w = ui.available_width();
                ui.horizontal_top(|ui| {
                    let down = egui::Layout::top_down(egui::Align::Min);
                    ui.allocate_ui_with_layout(egui::vec2(w * 0.46, h), down, |ui| {
                        ui.set_min_size(egui::vec2(w * 0.46, h));
                        self.commit_list(ui, p);
                    });
                    ui.separator();
                    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), h), down, |ui| {
                        action = self.details(ui, p, font_size);
                    });
                });
            });
        if !still_open {
            *open = false;
        }
        action
    }

    fn commit_list(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter by message, author or hash"));
        ui.label(
            RichText::new(format!(
                "{} commits · click: details · Ctrl+click: compare with the selected",
                self.commits.len()
            ))
            .small()
            .color(p.text_faint),
        );
        let q = self.filter.trim().to_lowercase();
        let shown: Vec<usize> = (0..self.commits.len())
            .filter(|&i| {
                let c = &self.commits[i];
                q.is_empty()
                    || c.subject.to_lowercase().contains(&q)
                    || c.author.to_lowercase().contains(&q)
                    || c.hash.starts_with(&q)
            })
            .collect();
        // Lanes only make sense on the unfiltered list.
        let graph = q.is_empty();
        let lanes_w = if graph {
            (self.rows.iter().map(|r| r.lane.max(r.through.iter().copied().max().unwrap_or(0))).max().unwrap_or(0) + 1)
                as f32
                * LANE_W
                + 6.0
        } else {
            0.0
        };
        let colors = [
            p.accent,
            p.success,
            p.warning,
            Color32::from_rgb(0x6a, 0xa5, 0xff),
            p.error,
            Color32::from_rgb(0x3d, 0xdb, 0xd9),
        ];
        let lane_color = |l: usize| colors[l % colors.len()];
        let row_h = 24.0;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut clicked = None;
        let mut area = egui::ScrollArea::vertical().id_salt("history-list").auto_shrink([false, false]);
        // A commit picked from elsewhere (line history): bring it into view once.
        if std::mem::take(&mut self.scroll_to_selected) {
            if let Some(pos) = self.selected.and_then(|s| shown.iter().position(|&i| i == s)) {
                area = area.vertical_scroll_offset((pos as f32 * row_h - 120.0).max(0.0));
            }
        }
        area.show_rows(ui, row_h, shown.len(), |ui, range| {
            for &i in &shown[range] {
                let c = &self.commits[i];
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
                let selected = self.selected == Some(i) || self.compare == Some(i);
                if selected {
                    ui.painter().rect_filled(rect, 3.0, p.accent_soft);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 3.0, p.bg_hover);
                }
                if graph {
                    let r = &self.rows[i];
                    let x = |l: usize| rect.min.x + 6.0 + l as f32 * LANE_W;
                    let (top, mid, bot) = (rect.min.y, rect.center().y, rect.max.y);
                    for &l in &r.through {
                        ui.painter().line_segment(
                            [egui::pos2(x(l), top), egui::pos2(x(l), bot)],
                            Stroke::new(1.5_f32, lane_color(l)),
                        );
                    }
                    // The line coming into the commit from the newer one above.
                    let incoming = i > 0
                        && (self.rows[i - 1].to_parents.contains(&r.lane)
                            || self.rows[i - 1].through.contains(&r.lane));
                    if incoming {
                        ui.painter().line_segment(
                            [egui::pos2(x(r.lane), top), egui::pos2(x(r.lane), mid)],
                            Stroke::new(1.5_f32, lane_color(r.lane)),
                        );
                    }
                    for &l in &r.from_above {
                        ui.painter().line_segment(
                            [egui::pos2(x(l), top), egui::pos2(x(r.lane), mid)],
                            Stroke::new(1.5_f32, lane_color(l)),
                        );
                    }
                    for &l in &r.to_parents {
                        ui.painter().line_segment(
                            [egui::pos2(x(r.lane), mid), egui::pos2(x(l), bot)],
                            Stroke::new(1.5_f32, lane_color(l)),
                        );
                    }
                    let merge = c.parents.len() > 1;
                    ui.painter().circle(
                        egui::pos2(x(r.lane), mid),
                        if merge { 3.5 } else { 4.0 },
                        if merge { p.bg } else { lane_color(r.lane) },
                        Stroke::new(1.5_f32, lane_color(r.lane)),
                    );
                }
                // Text: refs, subject, author · time.
                let mut job = egui::text::LayoutJob::default();
                let font = egui::FontId::proportional(13.0);
                if !c.refs.is_empty() {
                    job.append(
                        &format!(" {} ", short_refs(&c.refs)),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(11.0),
                            color: p.accent,
                            background: p.accent_soft,
                            ..Default::default()
                        },
                    );
                    job.append(" ", 0.0, egui::TextFormat::default());
                }
                job.append(
                    &c.subject,
                    0.0,
                    egui::TextFormat { font_id: font.clone(), color: p.text, ..Default::default() },
                );
                job.append(
                    &format!("   {} · {}", c.author, crate::git::relative_time(c.time, now)),
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(11.5),
                        color: p.text_faint,
                        ..Default::default()
                    },
                );
                job.wrap.max_rows = 1;
                job.wrap.max_width = rect.width() - lanes_w - 8.0;
                job.wrap.break_anywhere = true;
                let galley = ui.fonts(|f| f.layout_job(job));
                ui.painter().galley(
                    egui::pos2(rect.min.x + lanes_w, rect.center().y - galley.size().y / 2.0),
                    galley,
                    p.text,
                );
                let resp = resp.on_hover_text(format!("{}  {}", c.short, c.subject));
                if resp.clicked() {
                    clicked = Some((i, ui.input(|i| i.modifiers.command)));
                }
            }
        });
        if let Some((i, cmp)) = clicked {
            self.select(i, cmp);
        }
    }

    fn details(&mut self, ui: &mut egui::Ui, p: &Palette, font_size: f32) -> Option<Action> {
        let mut action = None;
        let sel = self.selected?;
        let c = &self.commits[sel];
        ui.horizontal(|ui| {
            ui.label(RichText::new(&c.short).monospace().color(p.accent));
            if ui.small_button("Copy hash").clicked() {
                ui.ctx().copy_text(c.hash.clone());
            }
            if let Some(o) = self.compare {
                ui.label(RichText::new(format!("↔ {}", self.commits[o].short)).monospace().color(p.warning));
            }
        });
        egui::ScrollArea::vertical().id_salt("history-msg").max_height(110.0).show(ui, |ui| {
            if self.files_rx.is_some() {
                ui.spinner();
            }
            ui.label(RichText::new(self.message.trim()).color(p.text_dim));
        });
        ui.separator();
        ui.label(RichText::new(format!("{} files changed", self.files.len())).small().color(p.text_faint));
        let mut pick = None;
        egui::ScrollArea::vertical().id_salt("history-files").max_height(150.0).show(ui, |ui| {
            for (st, path, old) in &self.files {
                let color = match st {
                    'A' => p.success,
                    'D' => p.error,
                    'R' => p.warning,
                    _ => p.accent,
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(st.to_string()).monospace().color(color));
                    let shown = self.diff.as_ref().is_some_and(|d| &d.path == path);
                    let label = if old != path { format!("{old} → {path}") } else { path.clone() };
                    if ui.add(egui::SelectableLabel::new(shown, label)).clicked() {
                        pick = Some((path.clone(), old.clone()));
                    }
                });
            }
        });
        if let Some((path, old)) = pick {
            self.show_diff(path, old);
        }
        ui.separator();
        if self.diff_rx.is_some() {
            ui.spinner();
        }
        let do_jump = std::mem::take(&mut self.diff_jump);
        if let Some(d) = &self.diff {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&d.path).strong());
                if ui.small_button("Open file").clicked() {
                    action = Some(Action::OpenFile(d.path.clone()));
                }
            });
            let changes: Vec<usize> = d
                .rows
                .iter()
                .enumerate()
                .filter(|(i, r)| r.changed && (*i == 0 || !d.rows[i - 1].changed))
                .map(|(i, _)| i)
                .collect();
            let old: Vec<&str> = d.old.lines().collect();
            let new: Vec<&str> = d.new.lines().collect();
            let id = ui.make_persistent_id(("history-diff", &d.path));
            let jump = changes.first().map(|c| c.saturating_sub(3)).filter(|_| do_jump);
            crate::diff_view::rows_ui(ui, id, &d.rows, &changes, &old, &new, p, font_size * 0.9, jump, false);
        }
        action
    }
}
