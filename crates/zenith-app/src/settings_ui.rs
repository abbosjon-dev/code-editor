//! Settings window: edits `Settings` live; the app saves and applies changes.

use eframe::egui::{self, RichText};
use zenith_ai::AgentProfile;

use crate::settings::Settings;
use crate::theme::Palette;

/// What changed, so the app can re-apply only what is needed.
#[derive(Default)]
pub struct Changes {
    pub any: bool,
    pub style: bool,
    pub syntax_theme: bool,
    pub lsp: bool,
    pub open_file: bool,
    pub open_keybindings: bool,
    pub open_workspace_file: bool,
    /// Settings Sync buttons.
    pub sync: Option<crate::sync_ui::SyncOp>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Appearance,
    Editor,
    Ai,
    Advanced,
}

#[allow(clippy::too_many_arguments)]
pub fn window(
    ctx: &egui::Context,
    open: &mut bool,
    section: &mut Section,
    s: &mut Settings,
    agents: &[AgentProfile],
    p: &Palette,
    overridden: &[String],
    search: &mut String,
) -> Changes {
    ANY_SHOWN.with(|a| a.set(false));
    let mut c = Changes::default();
    let mut still_open = *open;
    egui::Window::new("Settings")
        .open(&mut still_open)
        .collapsible(false)
        .resizable(true)
        .default_size([720.0, 500.0])
        .min_width(680.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                // Search and section list.
                ui.vertical(|ui| {
                    ui.set_width(130.0);
                    ui.add(egui::TextEdit::singleline(search).hint_text("🔍 Search").desired_width(130.0));
                    ui.add_space(4.0);
                    for (sec, label) in [
                        (Section::Appearance, "🎨  Appearance"),
                        (Section::Editor, "📝  Editor"),
                        (Section::Ai, "✳  AI & Terminal"),
                        (Section::Advanced, "⚙  Advanced"),
                    ] {
                        let sel = *section == sec && search.trim().is_empty();
                        let b = egui::Button::new(RichText::new(label).color(if sel { p.text } else { p.text_dim }))
                            .fill(if sel { p.accent_soft } else { egui::Color32::TRANSPARENT })
                            .min_size(egui::vec2(130.0, 28.0));
                        if ui.add(b).clicked() {
                            *section = sec;
                            search.clear();
                        }
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.spacing_mut().slider_width = 200.0;
                    if !overridden.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "This workspace's .zenith/settings.toml overrides: {}",
                                    overridden.join(", ")
                                ))
                                .small()
                                .color(p.warning),
                            );
                            if ui.small_button("Edit").clicked() {
                                c.open_workspace_file = true;
                            }
                        });
                        ui.add_space(4.0);
                    }
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        egui::Grid::new("settings-grid").num_columns(2).spacing([24.0, 10.0]).striped(false).show(
                            ui,
                            |ui| {
                                let query = search.trim().to_lowercase();
                                FILTER.with(|f| *f.borrow_mut() = query.clone());
                                if query.is_empty() {
                                    match section {
                                        Section::Appearance => appearance(ui, s, &mut c),
                                        Section::Editor => editor(ui, s, &mut c),
                                        Section::Ai => ai(ui, s, agents, &mut c),
                                        Section::Advanced => advanced(ui, s, &mut c),
                                    }
                                } else {
                                    // Every section's matching rows, under its name.
                                    let head = |n: &'static str| HEADER.with(|h| *h.borrow_mut() = Some(n));
                                    head("Appearance");
                                    appearance(ui, s, &mut c);
                                    head("Editor");
                                    editor(ui, s, &mut c);
                                    head("AI & Terminal");
                                    ai(ui, s, agents, &mut c);
                                    head("Advanced");
                                    advanced(ui, s, &mut c);
                                    if HEADER.with(|h| h.borrow_mut().take()).is_some() && !ANY_SHOWN.with(|a| a.get())
                                    {
                                        ui.label(RichText::new("No settings match").weak());
                                        ui.end_row();
                                    }
                                }
                                FILTER.with(|f| f.borrow_mut().clear());
                            },
                        );
                    });
                });
            });
        });
    *open = still_open;
    c
}

thread_local! {
    /// The search text (lowercase); rows that don't match aren't drawn.
    static FILTER: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// A section name to show before its first matching row (while searching).
    static HEADER: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
    /// Some row matched (to say so when none did).
    static ANY_SHOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a row with this title / hint matches the search.
fn matches(title: &str, hint: &str) -> bool {
    FILTER.with(|f| {
        let f = f.borrow();
        f.is_empty()
            || f.split_whitespace().all(|w| title.to_lowercase().contains(w) || hint.to_lowercase().contains(w))
    })
}

/// Draw a row's label when it matches the search; returns whether it did
/// (the caller then adds the widget and ends the row).
fn row_label(ui: &mut egui::Ui, title: &str, hint: &str) -> bool {
    if !matches(title, hint) {
        return false;
    }
    ANY_SHOWN.with(|a| a.set(true));
    if let Some(h) = HEADER.with(|h| h.borrow_mut().take()) {
        ui.label(RichText::new(h).strong().size(13.0));
        ui.end_row();
    }
    ui.vertical(|ui| {
        ui.set_min_width(210.0);
        ui.set_max_width(210.0);
        ui.label(title);
        if !hint.is_empty() {
            ui.label(RichText::new(hint).small().weak());
        }
    });
    true
}

fn checkbox(ui: &mut egui::Ui, v: &mut bool, title: &str, hint: &str) -> bool {
    if !row_label(ui, title, hint) {
        return false;
    }
    let r = ui.checkbox(v, "").changed();
    ui.end_row();
    r
}

fn slider<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    v: &mut T,
    range: std::ops::RangeInclusive<T>,
    title: &str,
    hint: &str,
) -> bool {
    if !row_label(ui, title, hint) {
        return false;
    }
    let r = ui.add(egui::Slider::new(v, range).trailing_fill(true)).changed();
    ui.end_row();
    r
}

fn appearance(ui: &mut egui::Ui, s: &mut Settings, c: &mut Changes) {
    if row_label(ui, "Color theme", "") {
        egui::ComboBox::from_id_salt("theme")
            .selected_text(
                crate::theme::Palette::THEMES
                    .iter()
                    .find(|(id, _)| *id == s.theme)
                    .map_or("Zenith Midnight (dark)", |t| t.1),
            )
            .show_ui(ui, |ui| {
                for &(id, name) in crate::theme::Palette::THEMES {
                    if ui.selectable_value(&mut s.theme, id.to_string(), name).changed() {
                        c.style = true;
                        c.any = true;
                    }
                }
            });
        ui.end_row();
    }

    if row_label(ui, "Syntax theme", "Colors for code") {
        egui::ComboBox::from_id_salt("syntax").selected_text(s.syntax_theme.clone()).height(320.0).show_ui(ui, |ui| {
            for name in crate::highlight::theme_names() {
                if ui.selectable_value(&mut s.syntax_theme, name.to_string(), name).changed() {
                    c.syntax_theme = true;
                    c.any = true;
                }
            }
        });
        ui.end_row();
    }

    if slider(ui, &mut s.ui_font_size, 10.0..=22.0, "UI font size", "") {
        c.style = true;
        c.any = true;
    }
    c.any |= slider(ui, &mut s.editor_font_size, 9.0..=32.0, "Editor font size", "");
    c.any |= slider(ui, &mut s.terminal_font_size, 9.0..=28.0, "Terminal font size", "");
    c.any |= slider(ui, &mut s.line_height, 1.0..=2.2, "Line height", "Multiple of the font size");
    c.any |= checkbox(ui, &mut s.show_minimap, "Minimap", "Code overview on the right");
    c.any |= checkbox(
        ui,
        &mut s.sticky_scroll,
        "Sticky scroll",
        "Keep the current function / class header at the top while scrolling",
    );
    c.any |= checkbox(ui, &mut s.word_wrap, "Word wrap", "Wrap long lines to the window (Alt+Z)");
    c.any |=
        checkbox(ui, &mut s.inline_diagnostics, "Inline problems", "Show error/warning text at the end of the line");
    c.any |= checkbox(ui, &mut s.inline_blame, "Inline git blame", "Author and commit of the caret's line");
    c.any |= checkbox(ui, &mut s.inlay_hints, "Inlay hints", "Types and parameter names from the language server");
    c.any |= checkbox(
        ui,
        &mut s.semantic_highlighting,
        "Semantic highlighting",
        "Color symbols by what they are (language server semantic tokens)",
    );
    c.any |= checkbox(ui, &mut s.show_indent_guides, "Indent guides", "");
    c.any |= checkbox(ui, &mut s.cursor_blink, "Blinking cursor", "");
}

fn editor(ui: &mut egui::Ui, s: &mut Settings, c: &mut Changes) {
    c.any |= slider(ui, &mut s.tab_size, 1..=8, "Tab size", "For new files");
    c.any |= checkbox(ui, &mut s.insert_spaces, "Insert spaces", "Use spaces instead of tabs");
    c.any |= checkbox(ui, &mut s.auto_save, "Auto save", "Save modified files every few seconds");
    c.any |= checkbox(ui, &mut s.format_trim_trailing_whitespace, "Trim trailing whitespace", "On save");
    c.any |= checkbox(ui, &mut s.format_on_save, "Format on save", "Uses the language server");
    c.any |= checkbox(
        ui,
        &mut s.flutter_hot_reload_on_save,
        "Flutter: hot reload on save",
        "Apply saved Dart changes to running Flutter apps",
    );
    if checkbox(
        ui,
        &mut s.tree_sitter,
        "Tree-sitter",
        "Syntax-tree highlighting and folding (Rust, Python, JS/TS, Go, C/C++, JSON, Bash, HTML, CSS)",
    ) {
        c.syntax_theme = true;
        c.any = true;
    }
    c.any |=
        checkbox(ui, &mut s.vim_mode, "Vim mode", "Modal editing: Normal / Insert / Visual, motions, operators, :w :q");
    if row_label(ui, "Keyboard shortcuts", "Rebind any command in keybindings.toml") {
        if ui.button("Edit keybindings.toml").clicked() {
            c.open_keybindings = true;
        }
        ui.end_row();
    }
    c.any |= checkbox(
        ui,
        &mut s.review_external_changes,
        "Review external changes",
        "Show a diff when an AI agent edits an open file",
    );
}

fn ai(ui: &mut egui::Ui, s: &mut Settings, agents: &[AgentProfile], c: &mut Changes) {
    if row_label(ui, "Default AI agent", "Used by Ask AI / Send to agent") {
        let current =
            agents.iter().find(|a| a.id == s.default_agent).map(|a| a.name.clone()).unwrap_or(s.default_agent.clone());
        egui::ComboBox::from_id_salt("agent").selected_text(current).show_ui(ui, |ui| {
            for a in agents {
                let label = if a.is_installed() { a.name.clone() } else { format!("{} (not installed)", a.name) };
                if ui.selectable_value(&mut s.default_agent, a.id.clone(), label).changed() {
                    c.any = true;
                }
            }
        });
        ui.end_row();
    }

    if row_label(ui, "Terminal shell", "Empty = system default") {
        c.any |= ui.add(egui::TextEdit::singleline(&mut s.shell).hint_text("e.g. zsh, pwsh, bash -l")).lost_focus();
        ui.end_row();
    }

    c.any |= checkbox(
        ui,
        &mut s.restore_terminals,
        "Restore terminals & agents",
        "Reopen them on startup; Claude Code continues its last conversation",
    );
    if row_label(ui, "Custom agents", "Add [[agents]] entries in settings.toml") {
        if ui.button("Edit settings.toml").clicked() {
            c.open_file = true;
        }
        ui.end_row();
    }
}

fn advanced(ui: &mut egui::Ui, s: &mut Settings, c: &mut Changes) {
    if row_label(ui, "Plugin marketplace", "Index of installable plugins (URL or file; empty = official)") {
        c.any |= ui
            .add(
                egui::TextEdit::singleline(&mut s.plugin_registry)
                    .hint_text(crate::marketplace::DEFAULT_REGISTRY)
                    .desired_width(300.0),
            )
            .changed();
        ui.end_row();
    }
    if row_label(ui, "Settings Sync", "Git repository for settings, shortcuts and plugins") {
        ui.vertical(|ui| {
            c.any |= ui
                .add(
                    egui::TextEdit::singleline(&mut s.sync_repo)
                        .hint_text("git@github.com:you/zenith-settings.git")
                        .desired_width(300.0),
                )
                .changed();
            ui.horizontal(|ui| {
                let connected = crate::sync::is_set_up(&crate::settings::config_dir());
                if ui.button(if connected { "Reconnect" } else { "Connect" }).clicked() {
                    c.sync = Some(crate::sync_ui::SyncOp::Setup);
                }
                if ui.add_enabled(connected, egui::Button::new("⬆ Push")).clicked() {
                    c.sync = Some(crate::sync_ui::SyncOp::Push);
                }
                if ui.add_enabled(connected, egui::Button::new("⬇ Pull")).clicked() {
                    c.sync = Some(crate::sync_ui::SyncOp::Pull);
                }
            });
        });
        ui.end_row();
    }
    c.any |= checkbox(ui, &mut s.sync_on_startup, "Pull on startup", "Get synced settings when Zenith starts");
    c.any |= checkbox(
        ui,
        &mut s.check_for_updates,
        "Check for updates",
        "Once a day, ask GitHub for a newer release and show a notice (nothing is installed)",
    );
    if checkbox(ui, &mut s.lsp_enabled, "Language servers", "Completion, diagnostics, go to definition") {
        c.lsp = true;
        c.any = true;
    }
    if row_label(ui, "Settings file", &crate::settings::settings_path().display().to_string()) {
        if ui.button("Open settings.toml").clicked() {
            c.open_file = true;
        }
        ui.end_row();
    }
}
