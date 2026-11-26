//! Command palette / quick open / pickers, with fuzzy matching.

use eframe::egui::{self, Align2, FontId, Key, Sense, Stroke};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

use crate::commands::Cmd;
use crate::theme::Palette as Colors;

#[derive(Clone, Debug, PartialEq)]
pub enum PaletteAction {
    /// Insert a snippet body at the caret.
    InsertSnippet(String),
    /// Switch to a recent project (local folder).
    SwitchProject(std::path::PathBuf),
    /// Open a recent remote folder ("host:/path").
    SwitchRemote(String),
    Command(Cmd),
    PluginCommand(String),
    OpenFile(String),
    /// Compare the active file with this one (same path form as `OpenFile`).
    CompareWith(String),
    /// Start run configuration `i`.
    RunConfig(usize),
    /// Start emulator `i` of the last device scan.
    StartEmulator(usize),
    /// Mirror the device / simulator with this id.
    DeviceScreen(String),
    GotoLine(usize),
    SetTheme(String),
    SetSyntaxTheme(String),
    SetLanguage(String),
    LaunchAgent(String),
    OpenRecent(std::path::PathBuf),
    CodeAction(usize),
    GitCheckout(String),
    GitNewBranch,
    GotoPosition(usize, usize),
    /// Open a file at an LSP position (line, UTF-16 column).
    OpenAt(std::path::PathBuf, usize, usize),
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub action: PaletteAction,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Commands,
    Files,
    GotoLine,
    Pick,
    /// Items come from the language server as the query changes.
    WorkspaceSymbols,
}

pub struct CommandPalette {
    pub open: bool,
    pub mode: Mode,
    pub query: String,
    pub placeholder: String,
    items: Vec<Item>,
    filtered: Vec<usize>,
    selected: usize,
    filtered_for: Option<String>,
    focus: bool,
    matcher: Matcher,
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::Commands,
            query: String::new(),
            placeholder: String::new(),
            items: vec![],
            filtered: vec![],
            selected: 0,
            filtered_for: None,
            focus: false,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        }
    }
}

impl CommandPalette {
    pub fn show(&mut self, mode: Mode, items: Vec<Item>, placeholder: &str) {
        self.open = true;
        self.mode = mode;
        self.items = items;
        self.query.clear();
        self.placeholder = placeholder.into();
        self.selected = 0;
        self.filtered_for = None;
        self.focus = true;
    }

    /// Replace the items, keeping the query (live results).
    pub fn set_items(&mut self, items: Vec<Item>) {
        self.items = items;
        self.filtered_for = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.filtered.clear();
    }

    fn refilter(&mut self) {
        if self.filtered_for.as_deref() == Some(self.query.as_str()) {
            return;
        }
        self.filtered_for = Some(self.query.clone());
        self.selected = 0;
        let q = self.query.trim();
        if q.is_empty() {
            self.filtered = (0..self.items.len().min(2000)).collect();
            return;
        }
        let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let hay = nucleo_matcher::Utf32Str::new(&it.label, &mut buf);
                pattern.score(hay, &mut self.matcher).map(|s| (i, s))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.filtered = scored.into_iter().take(500).map(|(i, _)| i).collect();
    }

    /// Draw the palette; returns the chosen action.
    pub fn ui(&mut self, ctx: &egui::Context, p: &Colors) -> Option<PaletteAction> {
        if !self.open {
            return None;
        }
        let mut chosen = None;
        if self.focus {
            // Lay out again next frame once list sizes are known (the palette
            // may have been opened by a background event, not by input).
            ctx.request_repaint();
        }
        let screen = ctx.screen_rect();
        let width = (screen.width() * 0.5).clamp(420.0, 680.0);
        let mut close = false;

        egui::Area::new(egui::Id::new("zenith-palette"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(screen.center().x - width / 2.0, screen.min.y + 56.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).fill(p.bg_elevated).inner_margin(8).corner_radius(10).show(ui, |ui| {
                    ui.set_width(width);
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(&self.placeholder)
                            .desired_width(f32::INFINITY)
                            .margin(egui::Margin::symmetric(10, 8))
                            .font(egui::TextStyle::Body),
                    );
                    if self.focus {
                        edit.request_focus();
                        self.focus = false;
                    }

                    if self.mode == Mode::GotoLine {
                        let n: Option<usize> = self.query.trim().parse().ok();
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(match n {
                                Some(n) => format!("Go to line {n}"),
                                None => "Type a line number and press Enter".into(),
                            })
                            .color(p.text_dim),
                        );
                        if ui.input(|i| i.key_pressed(Key::Enter)) {
                            if let Some(n) = n {
                                chosen = Some(PaletteAction::GotoLine(n.saturating_sub(1)));
                            }
                            close = true;
                        }
                    } else {
                        self.refilter();
                        let (up, down, enter) = ui.input(|i| {
                            (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter))
                        });
                        if down && !self.filtered.is_empty() {
                            self.selected = (self.selected + 1) % self.filtered.len();
                        }
                        if up && !self.filtered.is_empty() {
                            self.selected = (self.selected + self.filtered.len() - 1) % self.filtered.len();
                        }
                        if enter {
                            if let Some(&i) = self.filtered.get(self.selected) {
                                chosen = Some(self.items[i].action.clone());
                            }
                            close = true;
                        }
                        ui.add_space(6.0);
                        if self.filtered.is_empty() {
                            ui.label(egui::RichText::new("No matching results").color(p.text_dim));
                        }
                        let row_h = 30.0;
                        egui::ScrollArea::vertical()
                            .max_height(row_h * 12.0)
                            .min_scrolled_height(row_h * self.filtered.len().min(12) as f32)
                            .auto_shrink([false, true])
                            .show_rows(ui, row_h, self.filtered.len(), |ui, range| {
                                for row in range {
                                    let item = &self.items[self.filtered[row]];
                                    let (rect, resp) =
                                        ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
                                    let sel = row == self.selected;
                                    if sel {
                                        ui.painter().rect_filled(rect, 6.0, p.accent_soft);
                                        if up || down {
                                            ui.scroll_to_rect(rect, None);
                                        }
                                    } else if resp.hovered() {
                                        ui.painter().rect_filled(rect, 6.0, p.bg_hover);
                                    }
                                    let (main, sub) = if self.mode == Mode::Files {
                                        let (dir, name) = item.label.rsplit_once('/').unwrap_or(("", &item.label));
                                        (name.to_string(), dir.to_string())
                                    } else {
                                        (item.label.clone(), String::new())
                                    };
                                    let galley = ui.painter().layout_no_wrap(main, FontId::proportional(13.5), p.text);
                                    let w = galley.size().x;
                                    ui.painter().galley(
                                        egui::pos2(rect.min.x + 10.0, rect.center().y - galley.size().y / 2.0),
                                        galley,
                                        p.text,
                                    );
                                    if !sub.is_empty() {
                                        ui.painter().text(
                                            egui::pos2(rect.min.x + 18.0 + w, rect.center().y),
                                            Align2::LEFT_CENTER,
                                            sub,
                                            FontId::proportional(12.0),
                                            p.text_dim,
                                        );
                                    }
                                    if !item.detail.is_empty() {
                                        ui.painter().text(
                                            rect.right_center() - egui::vec2(10.0, 0.0),
                                            Align2::RIGHT_CENTER,
                                            &item.detail,
                                            FontId::monospace(11.5),
                                            p.text_dim,
                                        );
                                    }
                                    if resp.clicked() {
                                        chosen = Some(item.action.clone());
                                        close = true;
                                    }
                                }
                            });
                    }
                    if ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
        // Click outside closes.
        if ctx.input(|i| i.pointer.any_pressed()) {
            if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
                let area = ctx.memory(|m| m.area_rect(egui::Id::new("zenith-palette")));
                if area.is_some_and(|r| !r.contains(pos)) {
                    close = true;
                }
            }
        }
        let _ = Stroke::NONE;
        if close {
            self.close();
        }
        chosen
    }
}

pub fn command_items(extra: Vec<Item>) -> Vec<Item> {
    let mut v: Vec<Item> = Cmd::all()
        .iter()
        .map(|c| Item { label: c.title().into(), detail: c.shortcut(), action: PaletteAction::Command(*c) })
        .collect();
    v.extend(extra);
    v
}
