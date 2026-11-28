//! Line history: how a range of lines (the selection, or the function the
//! caret is in) changed over time — each commit with its message and the
//! part of its diff that touched them (`git log -L`).

use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, RichText};

use crate::git::Runner;
use crate::git_log::Commit;
use crate::theme::Palette;

/// A commit and its diff of the lines.
type Entry = (Commit, String);

pub enum Action {
    /// Show this commit in the History window.
    ShowCommit(String),
}

pub struct LineHistory {
    pub title: String,
    entries: Vec<Entry>,
    error: Option<String>,
    rx: Option<Receiver<Result<Vec<Entry>, String>>>,
    /// Full messages, fetched when a commit is expanded.
    messages: std::collections::HashMap<String, String>,
    run: Runner,
}

impl LineHistory {
    pub fn new(
        run: Runner,
        file: String,
        from: usize,
        to: usize,
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (tx, rx) = channel();
        let r = run.clone();
        let f = file.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::git_log::line_history(&r, &f, from, to, 200));
            waker();
        });
        let name = file.rsplit('/').next().unwrap_or(&file).to_string();
        let title = if from == to { format!("{name}:{from}") } else { format!("{name}:{from}-{to}") };
        Self { title, entries: vec![], error: None, rx: Some(rx), messages: Default::default(), run }
    }

    pub fn ui(&mut self, ctx: &egui::Context, p: &Palette, font_size: f32, open: &mut bool) -> Option<Action> {
        if let Some(rx) = &self.rx {
            if let Ok(r) = rx.try_recv() {
                self.rx = None;
                match r {
                    Ok(e) => self.entries = e,
                    Err(e) => self.error = Some(e),
                }
            }
        }
        let mut action = None;
        let screen = ctx.screen_rect();
        egui::Window::new(format!("Line history  {}", self.title))
            .id(egui::Id::new("zenith-line-history"))
            .open(open)
            .default_size(egui::vec2(screen.width() * 0.55, screen.height() * 0.7))
            .default_pos(screen.min + egui::vec2(screen.width() * 0.22, screen.height() * 0.1))
            .collapsible(false)
            .show(ctx, |ui| {
                if self.rx.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading history…");
                    });
                    return;
                }
                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).color(p.error));
                    return;
                }
                ui.label(
                    RichText::new(format!("{} commit(s) changed these lines, newest first", self.entries.len()))
                        .small()
                        .color(p.text_dim),
                );
                ui.separator();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let font = egui::FontId::monospace(font_size * 0.88);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (c, diff) in &self.entries {
                        let id = ui.make_persistent_id(("lh", &c.hash));
                        egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id, true)
                            .show_header(ui, |ui| {
                                ui.label(RichText::new(&c.short).monospace().color(p.accent));
                                ui.label(RichText::new(&c.subject).strong());
                                ui.label(
                                    RichText::new(format!("{} · {}", c.author, crate::git::relative_time(c.time, now)))
                                        .small()
                                        .color(p.text_faint),
                                );
                                if ui.small_button("Commit").on_hover_text("Show the whole commit").clicked() {
                                    action = Some(Action::ShowCommit(c.hash.clone()));
                                }
                            })
                            .body(|ui| {
                                // The message body, when there is more than the subject.
                                let msg = self
                                    .messages
                                    .entry(c.hash.clone())
                                    .or_insert_with(|| crate::git_log::message(&self.run, &c.hash));
                                let body: String = msg.lines().skip(1).collect::<Vec<_>>().join("\n");
                                let body = body.trim();
                                if !body.is_empty() {
                                    ui.label(RichText::new(body).small().color(p.text_dim));
                                }
                                for l in diff.lines() {
                                    let color = if l.starts_with('+') {
                                        p.success
                                    } else if l.starts_with('-') {
                                        p.error
                                    } else if l.starts_with("@@") {
                                        p.accent
                                    } else {
                                        p.text_dim
                                    };
                                    ui.label(RichText::new(l).font(font.clone()).color(color));
                                }
                                ui.add_space(6.0);
                            });
                    }
                });
            });
        action
    }
}
