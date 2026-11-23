//! "MCP SERVERS" section of the AI Agents panel: edits the workspace's
//! `.mcp.json` (read by Claude Code and others) and tests stdio servers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, Color32, RichText, Stroke};
use zenith_ai::mcp::{self, McpServer};

use crate::theme::Palette;

enum Probe {
    Running(Receiver<Result<Vec<String>, String>>),
    Done(Result<Vec<String>, String>),
}

/// The add / edit form.
#[derive(Clone, Default)]
struct Form {
    /// Index being edited (None = new).
    index: Option<usize>,
    name: String,
    remote: bool,
    url: String,
    command: String,
    args: String,
    env: String,
    extra: serde_json::Map<String, serde_json::Value>,
    error: Option<String>,
}

#[derive(Default)]
pub struct McpPanel {
    /// Servers of `root` (or the error reading its `.mcp.json`).
    loaded: Option<(PathBuf, Result<Vec<McpServer>, String>)>,
    probes: HashMap<String, Probe>,
    form: Option<Form>,
}

/// Common servers to start from.
const PRESETS: &[(&str, &str, &str)] = &[
    ("filesystem", "npx", "-y @modelcontextprotocol/server-filesystem ."),
    ("fetch", "uvx", "mcp-server-fetch"),
    ("git", "uvx", "mcp-server-git --repository ."),
    ("memory", "npx", "-y @modelcontextprotocol/server-memory"),
];

/// Split a command line into words (quotes group words).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (_, c) => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| if a.is_empty() || a.contains(char::is_whitespace) { format!("\"{a}\"") } else { a.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

impl McpPanel {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn servers(&mut self, root: &Path) -> &Result<Vec<McpServer>, String> {
        if self.loaded.as_ref().is_none_or(|(r, _)| r != root) {
            self.loaded = Some((root.to_path_buf(), mcp::load(root)));
            self.probes.clear();
        }
        &self.loaded.as_ref().unwrap().1
    }

    fn save(&mut self, root: &Path, servers: Vec<McpServer>, gemini: bool) -> Result<(), String> {
        mcp::save(root, &servers, gemini)?;
        self.loaded = Some((root.to_path_buf(), Ok(servers)));
        Ok(())
    }

    /// Draw the section; returns a message to show (saved / error).
    pub fn ui(&mut self, ui: &mut egui::Ui, p: &Palette, root: Option<&Path>, gemini: &mut bool) -> Option<String> {
        let mut message = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("MCP SERVERS").size(11.0).color(p.text_dim).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if root.is_some() && ui.small_button("+ Add").clicked() {
                    self.form = Some(Form::default());
                }
                if root.is_some() && ui.small_button("⟳").on_hover_text("Reload .mcp.json").clicked() {
                    self.loaded = None;
                }
            });
        });
        let Some(root) = root else {
            ui.label(RichText::new("Open a folder to configure MCP servers.").small().color(p.text_faint));
            return None;
        };
        // Finished probes.
        for probe in self.probes.values_mut() {
            if let Probe::Running(rx) = probe {
                if let Ok(r) = rx.try_recv() {
                    *probe = Probe::Done(r);
                }
            }
        }
        let servers = match self.servers(root) {
            Ok(s) => s.clone(),
            Err(e) => {
                ui.label(RichText::new(e).small().color(p.error));
                return None;
            }
        };
        if servers.is_empty() {
            ui.label(
                RichText::new("No servers yet. They're saved to .mcp.json in this folder, where agents pick them up.")
                    .small()
                    .color(p.text_faint),
            );
        }
        let mut edit = None;
        let mut delete = None;
        let mut test = None;
        for (i, s) in servers.iter().enumerate() {
            egui::Frame::new()
                .fill(p.bg_elevated)
                .corner_radius(6)
                .inner_margin(8)
                .stroke(Stroke::new(1.0_f32, p.border))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (dot, tip) = match self.probes.get(&s.name) {
                            Some(Probe::Running(_)) => (p.warning, "testing…".to_string()),
                            Some(Probe::Done(Ok(t))) => (p.success, format!("{} tools: {}", t.len(), t.join(", "))),
                            Some(Probe::Done(Err(e))) => (p.error, e.clone()),
                            None => (p.text_faint, "not tested".into()),
                        };
                        ui.label(RichText::new("●").color(dot)).on_hover_text(&tip);
                        ui.label(RichText::new(&s.name).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                delete = Some(i);
                            }
                            if ui.small_button("Edit").clicked() {
                                edit = Some(i);
                            }
                            if !s.is_remote()
                                && ui.small_button("Test").on_hover_text("Start it and list its tools").clicked()
                            {
                                test = Some(i);
                            }
                        });
                    });
                    let detail =
                        if s.is_remote() { s.url.clone() } else { format!("$ {} {}", s.command, join_args(&s.args)) };
                    ui.add(egui::Label::new(RichText::new(detail).monospace().small().color(p.text_dim)).truncate());
                    if let Some(Probe::Done(r)) = self.probes.get(&s.name) {
                        let (t, c) = match r {
                            Ok(tools) => (format!("✓ {} tools: {}", tools.len(), tools.join(", ")), p.success),
                            Err(e) => (format!("✕ {e}"), p.error),
                        };
                        ui.add(egui::Label::new(RichText::new(t).small().color(c)).truncate());
                    }
                });
            ui.add_space(4.0);
        }
        if ui
            .checkbox(gemini, RichText::new("Also write .gemini/settings.json").small())
            .on_hover_text("Gemini CLI reads its MCP servers from there")
            .changed()
        {
            message = Some(String::new()); // settings changed (saved by the caller)
            if let Err(e) = self.save(root, servers.clone(), *gemini) {
                message = Some(e);
            }
        }
        ui.label(
            RichText::new("Agents load these when they start; Claude Code asks you to approve project servers.")
                .small()
                .color(p.text_faint),
        );

        if let Some(i) = test {
            let s = servers[i].clone();
            let (tx, rx) = channel();
            let cwd = root.to_path_buf();
            std::thread::spawn(move || {
                let _ = tx.send(mcp::probe(&s, &cwd));
            });
            self.probes.insert(servers[i].name.clone(), Probe::Running(rx));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        if self.probes.values().any(|p| matches!(p, Probe::Running(_))) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        if let Some(i) = delete {
            let mut list = servers.clone();
            let removed = list.remove(i);
            self.probes.remove(&removed.name);
            message = Some(match self.save(root, list, *gemini) {
                Ok(()) => format!("Removed MCP server {}", removed.name),
                Err(e) => e,
            });
        }
        if let Some(i) = edit {
            let s = &servers[i];
            self.form = Some(Form {
                index: Some(i),
                name: s.name.clone(),
                remote: s.is_remote(),
                url: s.url.clone(),
                command: s.command.clone(),
                args: join_args(&s.args),
                env: s.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"),
                extra: s.extra.clone(),
                error: None,
            });
        }
        if let Some(m) = self.form_window(ui.ctx(), p, root, &servers, *gemini) {
            message = Some(m);
        }
        message
    }

    fn form_window(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        root: &Path,
        servers: &[McpServer],
        gemini: bool,
    ) -> Option<String> {
        let mut form = self.form.take()?;
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new(if form.index.is_some() { "Edit MCP server" } else { "Add MCP server" })
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
            .min_width(460.0)
            .show(ctx, |ui| {
                if form.index.is_none() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Presets:").small().color(p.text_dim));
                        for (name, cmd, args) in PRESETS {
                            if ui.small_button(*name).clicked() {
                                form.name = name.to_string();
                                form.remote = false;
                                form.command = cmd.to_string();
                                form.args = args.to_string();
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                egui::Grid::new("mcp-form").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(300.0));
                    ui.end_row();
                    ui.label("Type");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut form.remote, false, "Local command (stdio)");
                        ui.radio_value(&mut form.remote, true, "Remote URL (HTTP)");
                    });
                    ui.end_row();
                    if form.remote {
                        ui.label("URL");
                        ui.add(
                            egui::TextEdit::singleline(&mut form.url)
                                .hint_text("https://example.com/mcp")
                                .desired_width(300.0),
                        );
                        ui.end_row();
                    } else {
                        ui.label("Command");
                        ui.add(egui::TextEdit::singleline(&mut form.command).hint_text("npx").desired_width(300.0));
                        ui.end_row();
                        ui.label("Arguments");
                        ui.add(
                            egui::TextEdit::singleline(&mut form.args)
                                .hint_text("-y @modelcontextprotocol/server-filesystem .")
                                .desired_width(300.0),
                        );
                        ui.end_row();
                        ui.label("Environment");
                        ui.add(
                            egui::TextEdit::multiline(&mut form.env)
                                .hint_text("KEY=value (one per line)")
                                .desired_rows(2)
                                .desired_width(300.0),
                        );
                        ui.end_row();
                    }
                });
                if let Some(e) = &form.error {
                    ui.label(RichText::new(e).color(p.error));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(p.accent)).clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    ui.label(RichText::new("Saved to .mcp.json").small().color(p.text_faint));
                });
            });
        if !open || cancel {
            return None;
        }
        if !save {
            self.form = Some(form);
            return None;
        }
        // Validate and save.
        let name = form.name.trim().to_string();
        let error = if name.is_empty() || name.contains(char::is_whitespace) {
            Some("Name must be one word")
        } else if servers.iter().enumerate().any(|(i, s)| s.name == name && Some(i) != form.index) {
            Some("A server with this name exists")
        } else if form.remote && !form.url.trim().starts_with("http") {
            Some("URL must start with http:// or https://")
        } else if !form.remote && form.command.trim().is_empty() {
            Some("Command is required")
        } else {
            None
        };
        if let Some(e) = error {
            form.error = Some(e.into());
            self.form = Some(form);
            return None;
        }
        let server = McpServer {
            name: name.clone(),
            url: if form.remote { form.url.trim().to_string() } else { String::new() },
            command: if form.remote { String::new() } else { form.command.trim().to_string() },
            args: if form.remote { vec![] } else { split_args(&form.args) },
            env: if form.remote {
                Default::default()
            } else {
                form.env
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .filter(|(k, _)| !k.is_empty())
                    .collect()
            },
            extra: form.extra.clone(),
        };
        let mut list = servers.to_vec();
        match form.index {
            Some(i) => {
                self.probes.remove(&list[i].name);
                list[i] = server;
            }
            None => list.push(server),
        }
        match self.save(root, list, gemini) {
            Ok(()) => Some(format!("Saved MCP server {name} to .mcp.json")),
            Err(e) => {
                form.error = Some(e);
                self.form = Some(form);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_split_and_join() {
        assert_eq!(split_args(r#"-y pkg "a b" 'c'  d"#), vec!["-y", "pkg", "a b", "c", "d"]);
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args(r#"--x """#), vec!["--x", ""]);
        let a = vec!["-y".to_string(), "a b".to_string()];
        assert_eq!(split_args(&join_args(&a)), a);
    }
}
