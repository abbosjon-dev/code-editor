//! Project setup: the files in `.zenith/` that describe how to work on a
//! project (run, debug, databases, settings) in one place — whether each
//! exists, whether it parses, what it holds — and a reminder of what to
//! commit (shared with the team) and what to keep out of Git (profiles).

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};

use crate::theme::Palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Run,
    Launch,
    Databases,
    Settings,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Run, Kind::Launch, Kind::Databases, Kind::Settings];

    pub fn file(self) -> &'static str {
        match self {
            Kind::Run => "run.toml",
            Kind::Launch => "launch.toml",
            Kind::Databases => "databases.toml",
            Kind::Settings => "settings.toml",
        }
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::Run => "How to run the project: commands, compounds, env files (Run panel, Ctrl+F5)",
            Kind::Launch => "Debug configurations (F5)",
            Kind::Databases => "Database connections for the Databases view",
            Kind::Settings => "Editor settings for this project (tab size, formatting…), over each person's own",
        }
    }

    pub fn path(self, root: &Path) -> PathBuf {
        root.join(".zenith").join(self.file())
    }
}

/// What a file holds, or why it can't be read.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Missing,
    /// Parsed: a short summary ("3 configurations").
    Ok(String),
    Error(String),
}

pub fn status(kind: Kind, root: &Path) -> Status {
    let path = kind.path(root);
    let Ok(text) = std::fs::read_to_string(&path) else { return Status::Missing };
    let value: toml::Table = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return Status::Error(e.message().to_string()),
    };
    let count = |key: &str| value.get(key).and_then(|v| v.as_array()).map_or(0, |a| a.len());
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    match kind {
        Kind::Run => {
            // The real loader also checks compounds against configurations.
            let (_, err) = crate::run::load(root);
            match err {
                Some(e) => Status::Error(e),
                None => {
                    let mut s = plural(count("run"), "configuration");
                    if count("compound") > 0 {
                        s.push_str(&format!(", {}", plural(count("compound"), "compound")));
                    }
                    Status::Ok(s)
                }
            }
        }
        Kind::Launch => match zenith_dap::parse_launch_file(&text) {
            Ok(c) => Status::Ok(plural(c.len(), "configuration")),
            Err(e) => Status::Error(e.to_string()),
        },
        Kind::Databases => Status::Ok(plural(count("connections"), "connection")),
        Kind::Settings => {
            let n = value.len();
            let denied: Vec<&String> =
                value.keys().filter(|k| crate::settings::WORKSPACE_DENIED.contains(&k.as_str())).collect();
            if denied.is_empty() {
                Status::Ok(plural(n, "setting"))
            } else {
                Status::Error(format!(
                    "not allowed in a project (ignored): {}",
                    denied.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ))
            }
        }
    }
}

/// Lines a project's .gitignore should have for Zenith's own files.
pub const IGNORE: &[&str] = &[".zenith/profile-*"];

/// Which of [`IGNORE`] the folder's .gitignore lacks.
pub fn missing_ignores(root: &Path) -> Vec<&'static str> {
    let text = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    IGNORE.iter().copied().filter(|l| !text.lines().any(|x| x.trim() == *l)).collect()
}

pub fn add_ignores(root: &Path) -> std::io::Result<()> {
    let path = root.join(".gitignore");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# Zenith: local profiles\n");
    for l in missing_ignores(root) {
        text.push_str(l);
        text.push('\n');
    }
    std::fs::write(path, text)
}

pub enum Action {
    /// Open (creating it from its template when missing).
    Open(Kind),
    AddIgnores,
}

pub fn ui(ctx: &egui::Context, p: &Palette, root: &Path, open: &mut bool) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Project setup")
        .id(egui::Id::new("zenith-project-setup"))
        .open(open)
        .default_width(620.0)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "These files in .zenith/ describe how to work on this project. Commit them: everyone who opens \
                     the folder gets the same run and debug setups, connections and settings.",
                )
                .small()
                .color(p.text_dim),
            );
            ui.add_space(6.0);
            egui::Grid::new("setup-files").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                for k in Kind::ALL {
                    ui.vertical(|ui| {
                        ui.set_min_width(300.0);
                        ui.set_max_width(300.0);
                        ui.label(RichText::new(format!(".zenith/{}", k.file())).monospace().strong());
                        ui.label(RichText::new(k.what()).small().color(p.text_faint));
                    });
                    match status(k, root) {
                        Status::Missing => {
                            ui.label(RichText::new("not set up").small().color(p.text_faint));
                        }
                        Status::Ok(s) => {
                            ui.label(RichText::new(format!("✓ {s}")).small().color(p.success));
                        }
                        Status::Error(e) => {
                            ui.label(
                                RichText::new(format!("⚠ {}", e.lines().next().unwrap_or(&e))).small().color(p.error),
                            )
                            .on_hover_text(&e);
                        }
                    }
                    let label = if k.path(root).exists() { "Open" } else { "Create" };
                    if ui.button(label).clicked() {
                        action = Some(Action::Open(k));
                    }
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            let missing = missing_ignores(root);
            if !missing.is_empty() && root.join(".git").exists() {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("Profiles are personal: keep them out of Git ({}).", missing.join(", ")))
                            .small()
                            .color(p.warning),
                    );
                    if ui.small_button("Add to .gitignore").clicked() {
                        action = Some(Action::AddIgnores);
                    }
                });
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Secrets don't belong here: database passwords and tokens go in .env files (not committed); \
                     run.toml's env_file and DATABASE_URL pick them up.",
                )
                .small()
                .color(p.text_faint),
            );
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_gitignore() {
        let d = std::env::temp_dir().join(format!("zenith-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".zenith")).unwrap();
        assert_eq!(status(Kind::Run, &d), Status::Missing);
        std::fs::write(Kind::Run.path(&d), "detect = false\n[[run]]\nname = \"a\"\ncommand = \"x\"\n").unwrap();
        assert_eq!(status(Kind::Run, &d), Status::Ok("1 configuration".into()));
        std::fs::write(Kind::Databases.path(&d), "[[connections]]\nname = \"x\"\n").unwrap();
        assert_eq!(status(Kind::Databases, &d), Status::Ok("1 connection".into()));
        std::fs::write(Kind::Launch.path(&d), "[[configurations\n").unwrap();
        assert!(matches!(status(Kind::Launch, &d), Status::Error(_)));
        std::fs::write(Kind::Settings.path(&d), "tab_size = 2\nshell = \"x\"\n").unwrap();
        assert!(matches!(status(Kind::Settings, &d), Status::Error(e) if e.contains("shell")));
        std::fs::write(d.join(".gitignore"), "target").unwrap();
        assert_eq!(missing_ignores(&d), [".zenith/profile-*"]);
        add_ignores(&d).unwrap();
        assert!(missing_ignores(&d).is_empty());
        assert!(std::fs::read_to_string(d.join(".gitignore")).unwrap().starts_with("target\n"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
