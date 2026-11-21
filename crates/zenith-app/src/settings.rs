//! User settings (`settings.toml`) and session state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zenith_ai::AgentProfile;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "zenith-midnight" or "zenith-daylight".
    pub theme: String,
    /// Syntax highlighting theme (any bat/two-face theme name, e.g. "OneHalfDark", "Dracula", "Nord").
    pub syntax_theme: String,
    pub ui_font_size: f32,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    pub line_height: f32,
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub show_indent_guides: bool,
    pub show_minimap: bool,
    /// Soft-wrap long lines to the window width.
    pub word_wrap: bool,
    /// Show problem messages at the end of their line.
    pub inline_diagnostics: bool,
    /// Show who last changed the caret's line (git blame) at its end.
    pub inline_blame: bool,
    pub cursor_blink: bool,
    pub auto_save: bool,
    pub format_trim_trailing_whitespace: bool,
    /// Custom shell for new terminals (empty = system default).
    pub shell: String,
    /// Which agent the "Ask AI" shortcuts talk to.
    pub default_agent: String,
    /// Extra / overridden AI agents.
    pub agents: Vec<AgentProfile>,
    /// Language Server Protocol support (completion, diagnostics, go to definition).
    pub lsp_enabled: bool,
    /// Keep the previous version when a file changes on disk (e.g. edited by an AI agent) so it can be reviewed and reverted.
    pub review_external_changes: bool,
    /// Format files with the language server when saving.
    pub format_on_save: bool,
    /// Reopen terminals and AI agents (continuing their last conversation when supported) on startup.
    pub restore_terminals: bool,
    /// Extra / overridden language servers (same `name` overrides a built-in).
    pub language_servers: Vec<zenith_lsp::ServerConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "zenith-midnight".into(),
            syntax_theme: "OneHalfDark".into(),
            ui_font_size: 13.5,
            editor_font_size: 14.0,
            terminal_font_size: 13.0,
            line_height: 1.5,
            tab_size: 4,
            insert_spaces: true,
            show_indent_guides: true,
            show_minimap: true,
            word_wrap: false,
            inline_diagnostics: true,
            inline_blame: true,
            cursor_blink: true,
            auto_save: false,
            format_trim_trailing_whitespace: false,
            shell: String::new(),
            default_agent: "claude".into(),
            agents: vec![],
            lsp_enabled: true,
            review_external_changes: true,
            format_on_save: false,
            restore_terminals: true,
            language_servers: vec![],
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ZENITH_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("dev", "Zenith", "zenith")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".zenith"))
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.toml")
}

pub fn plugins_dir() -> PathBuf {
    config_dir().join("plugins")
}

fn session_path() -> PathBuf {
    config_dir().join("session.json")
}

impl Settings {
    pub fn load() -> (Self, Option<String>) {
        let path = settings_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(s) => (s, None),
                Err(e) => (Self::default(), Some(format!("settings.toml: {e}"))),
            },
            Err(_) => {
                let s = Self::default();
                let _ = s.write_default(&path);
                (s, None)
            }
        }
    }

    fn write_default(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = toml::to_string_pretty(self).unwrap_or_default();
        let header = "# Zenith settings. Saved changes apply immediately.\n\
# Add your own AI agent CLIs like this:\n\
#\n\
# [[agents]]\n\
# id = \"my-agent\"\n\
# name = \"My Agent\"\n\
# command = \"my-agent-cli\"\n\
# args = [\"--flag\"]\n\
#\n\
# Language servers are used automatically when installed (rust-analyzer,\n\
# pyright, typescript-language-server, gopls, clangd...). Add your own:\n\
#\n\
# [[language_servers]]\n\
# name = \"pylsp\"\n\
# command = \"pylsp\"\n\
# languages = [\"python\"]\n\n";
        std::fs::write(path, format!("{header}{body}"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub workspace: Option<PathBuf>,
    pub open_files: Vec<PathBuf>,
    pub active: usize,
    pub recent_workspaces: Vec<PathBuf>,
    pub sidebar_width: Option<f32>,
    pub zoom: f32,
    /// Caret position (char index) per open file.
    pub cursors: Vec<(PathBuf, usize)>,
    pub show_sidebar: Option<bool>,
    pub sidebar: String,
    pub show_terminal: bool,
    pub show_ai: bool,
    pub split: bool,
    /// Working directories of open shell terminals.
    pub terminals: Vec<Option<PathBuf>>,
    /// Ids of open AI agent tabs.
    pub agents: Vec<String>,
    /// Additional folders in the workspace (multi-root).
    pub extra_roots: Vec<PathBuf>,
}

impl Session {
    pub fn load() -> Self {
        std::fs::read_to_string(session_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = session_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, t);
        }
    }
}
