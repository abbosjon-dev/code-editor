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
    /// Keep the headers of the scopes being read at the top of the editor.
    pub sticky_scroll: bool,
    /// Soft-wrap long lines to the window width.
    pub word_wrap: bool,
    /// Show problem messages at the end of their line.
    pub inline_diagnostics: bool,
    /// Show who last changed the caret's line (git blame) at its end.
    pub inline_blame: bool,
    /// Show inlay hints from the language server (types, parameter names).
    pub inlay_hints: bool,
    /// Color symbols by meaning using the language server (semantic tokens).
    pub semantic_highlighting: bool,
    pub cursor_blink: bool,
    /// Vim-style modal editing (normal / insert / visual modes).
    pub vim_mode: bool,
    /// Tree-sitter highlighting and folding for supported languages.
    pub tree_sitter: bool,
    /// Look for a newer release on GitHub (at most once a day; notify only).
    pub check_for_updates: bool,
    /// Git repository for Settings Sync (empty = off).
    pub sync_repo: String,
    /// Pull synced settings when Zenith starts.
    pub sync_on_startup: bool,
    /// Plugin marketplace index (URL or file; empty = the official one).
    pub plugin_registry: String,
    pub auto_save: bool,
    pub format_trim_trailing_whitespace: bool,
    /// Custom shell for new terminals (empty = system default).
    pub shell: String,
    /// Which agent the "Ask AI" shortcuts talk to.
    pub default_agent: String,
    /// Extra / overridden AI agents.
    pub agents: Vec<AgentProfile>,
    /// Mirror the workspace's MCP servers into `.gemini/settings.json`.
    pub mcp_write_gemini: bool,
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
            sticky_scroll: true,
            word_wrap: false,
            inline_diagnostics: true,
            inline_blame: true,
            inlay_hints: true,
            semantic_highlighting: true,
            cursor_blink: true,
            vim_mode: false,
            tree_sitter: true,
            check_for_updates: true,
            sync_repo: String::new(),
            plugin_registry: String::new(),
            sync_on_startup: false,
            auto_save: false,
            format_trim_trailing_whitespace: false,
            shell: String::new(),
            default_agent: "claude".into(),
            agents: vec![],
            mcp_write_gemini: false,
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

/// Per-project overrides: `<workspace>/.zenith/settings.toml`.
pub fn workspace_settings_path(ws: &Path) -> PathBuf {
    ws.join(".zenith").join("settings.toml")
}

/// Keys a workspace may not set: they start programs or reach the network,
/// and opening a cloned repository must not do that by itself.
pub const WORKSPACE_DENIED: &[&str] =
    &["shell", "agents", "language_servers", "sync_repo", "sync_on_startup", "plugin_registry", "check_for_updates"];

/// User settings with a workspace's overrides on top.
pub struct Layered {
    pub settings: Settings,
    /// Keys the workspace file sets (and that were applied).
    pub overridden: Vec<String>,
    /// Keys it tried to set but may not.
    pub ignored: Vec<String>,
    pub error: Option<String>,
}

impl Settings {
    /// Merge `overrides` (a workspace settings.toml) over `self`.
    pub fn layered(&self, overrides: &str) -> Layered {
        let mut out = Layered { settings: self.clone(), overridden: vec![], ignored: vec![], error: None };
        let ws: toml::Table = match toml::from_str(overrides) {
            Ok(t) => t,
            Err(e) => {
                out.error = Some(format!(".zenith/settings.toml: {e}"));
                return out;
            }
        };
        let mut base = toml::Table::try_from(self).unwrap_or_default();
        for (k, v) in ws {
            if WORKSPACE_DENIED.contains(&k.as_str()) {
                out.ignored.push(k);
            } else if base.contains_key(&k) {
                base.insert(k.clone(), v);
                out.overridden.push(k);
            } else {
                out.ignored.push(k);
            }
        }
        match toml::Value::Table(base).try_into::<Settings>() {
            Ok(s) => out.settings = s,
            Err(e) => {
                out.error = Some(format!(".zenith/settings.toml: {e}"));
                out.overridden.clear();
            }
        }
        out
    }

    /// Effective settings for a workspace (user settings when it has none).
    pub fn for_workspace(&self, ws: Option<&Path>) -> Layered {
        match ws.and_then(|w| std::fs::read_to_string(workspace_settings_path(w)).ok()) {
            Some(text) => self.layered(&text),
            None => Layered { settings: self.clone(), overridden: vec![], ignored: vec![], error: None },
        }
    }

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
    /// Breakpoint lines (0-based) per file.
    pub breakpoints: Vec<(PathBuf, Vec<usize>)>,
    /// Breakpoints with a condition / log message / hit count / disabled.
    pub breakpoint_details: Vec<(PathBuf, usize, crate::debug::Bp)>,
    /// Debugger watch expressions.
    pub watches: Vec<String>,
    /// Remote folder (SSH) the workspace mirrors, and the host time of its last sync.
    pub remote: Option<crate::remote::RemoteSpec>,
    pub remote_since: i64,
    /// Recently opened remote folders ("host:/path").
    pub recent_remotes: Vec<String>,
    /// When updates were last checked (Unix seconds).
    pub last_update_check: u64,
    /// Where each recent project was left (for the project switcher).
    pub projects: Vec<(PathBuf, ProjectState)>,
}

/// A project's open files, active tab and carets, restored when switching back.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectState {
    pub open_files: Vec<PathBuf>,
    pub active: usize,
    pub cursors: Vec<(PathBuf, usize)>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_overrides() {
        let user = Settings { tab_size: 4, word_wrap: false, ..Default::default() };
        let l = user.layered("tab_size = 2\nword_wrap = true\nshell = \"evil\"\nnope = 1\n");
        assert_eq!(l.settings.tab_size, 2);
        assert!(l.settings.word_wrap);
        assert_eq!(l.settings.shell, "");
        assert_eq!(l.overridden, ["tab_size", "word_wrap"]);
        assert!(l.ignored.contains(&"shell".to_string()) && l.ignored.contains(&"nope".to_string()));
        // A wrong type is an error and changes nothing.
        let l = user.layered("tab_size = \"two\"\n");
        assert!(l.error.is_some());
        assert_eq!(l.settings.tab_size, 4);
    }
}
