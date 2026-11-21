//! The Zenith application: layout, commands and glue between components.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Key, KeyboardShortcut, Layout, Modifiers, RichText, Sense, Stroke,
};
use syntect::highlighting::Theme;
use zenith_ai::{build_context_prompt, merge_agents, AgentProfile};
use zenith_plugin::{Action, EditorContext, NotifyLevel, PluginEventKind, PluginHost};
use zenith_terminal::CommandSpec;

use crate::commands::Cmd;
use crate::document::Document;
use crate::editor_view::{editor_id, editor_ui, EditorAction};
use crate::file_tree::{FileTree, TreeAction};
use crate::git::Git;
use crate::highlight;
use crate::lsp::{LspManager, LspOutcome};
use crate::palette::{command_items, CommandPalette, Item, Mode, PaletteAction};
use crate::settings::{self, Session, Settings};
use crate::terminal_view::{terminal_ui, TermEvent, TermKind, TermTab};
use crate::theme::{self, Palette};
use crate::workspace::{build_regex, replace_in_text, FileIndex, SearchHit, SearchJob, SearchOpts};

type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarView {
    Explorer,
    Search,
    Git,
    Problems,
    Agents,
    Plugins,
}

struct Toast {
    text: String,
    level: NotifyLevel,
    until: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TabMenu {
    Close,
    CloseOthers,
    CloseRight,
    CloseSaved,
    CopyPath,
    CopyRelPath,
    Reveal,
    OtherPane,
}

enum InputKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    /// Rename the symbol at this char index (LSP).
    RenameSymbol(usize),
    NewBranch,
}

enum Dialog {
    Input { title: String, value: String, kind: InputKind, focus: bool },
    ConfirmDelete(PathBuf),
    UnsavedClose(u64),
    UnsavedQuit,
    ConfirmReplace { files: usize, count: usize },
    AskAgent { question: String, focus: bool },
    About,
}

#[derive(Default)]
struct TermPanel {
    tabs: Vec<TermTab>,
    active: usize,
}

impl TermPanel {
    fn active_mut(&mut self) -> Option<&mut TermTab> {
        self.tabs.get_mut(self.active)
    }
}

struct PendingSend {
    tab_id: u64,
    text: String,
    submit: bool,
    created: Instant,
}

#[derive(Default)]
struct SearchPanel {
    query: String,
    replacement: String,
    show_replace: bool,
    opts: SearchOpts,
    results: Vec<SearchHit>,
    job: Option<SearchJob>,
    focus: bool,
    error: Option<String>,
    /// When the query/options last changed (search runs shortly after typing stops).
    edited_at: Option<Instant>,
}

pub struct ZenithApp {
    settings: Settings,
    colors: Palette,
    syntax_theme: Arc<Theme>,
    session: Session,
    workspace: Option<PathBuf>,
    tree: Option<FileTree>,
    index: FileIndex,
    git: Option<Git>,
    commit_message: String,
    docs: Vec<Document>,
    active: usize,
    next_id: u64,
    untitled: usize,
    sidebar: SidebarView,
    show_sidebar: bool,
    show_terminal: bool,
    show_ai: bool,
    bottom: TermPanel,
    ai: TermPanel,
    focus_terminal: Option<u64>,
    terminal_focused: bool,
    palette: CommandPalette,
    plugins: PluginHost,
    agents: Vec<AgentProfile>,
    installed: HashMap<String, bool>,
    toasts: Vec<Toast>,
    dialog: Option<Dialog>,
    search: SearchPanel,
    cursor_epoch: f64,
    waker: Waker,
    allow_close: bool,
    pending_sends: Vec<PendingSend>,
    lsp: LspManager,
    pending_definition: Option<u64>,
    pending_references: Option<(u64, String)>,
    pending_rename: Option<u64>,
    pending_code_actions: Option<u64>,
    pending_symbols: Option<u64>,
    code_actions: Vec<zenith_lsp::CodeAction>,
    /// Diagnostics for every file the language servers reported on.
    problems: std::collections::BTreeMap<PathBuf, Vec<zenith_lsp::Diagnostic>>,
    /// formatting request -> (document id, save afterwards)
    pending_format: HashMap<u64, (u64, bool)>,
    /// Set while saving the result of format-on-save (avoids a loop).
    saving_formatted: bool,
    last_session_save: Instant,
    style_dirty: bool,
    focus_editor: bool,
    md_cache: egui_commonmark::CommonMarkCache,
    show_settings: bool,
    settings_section: crate::settings_ui::Section,
    /// Split editor: two panes side by side.
    split: bool,
    /// Doc index shown in the pane that does NOT have focus.
    other: usize,
    /// Whether the right pane is the focused one (`active` belongs to it).
    focus_right: bool,
}

impl ZenithApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Vec<String>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let (settings, settings_err) = Settings::load();
        let colors = Palette::by_name(&settings.theme);
        theme::apply_style(&cc.egui_ctx, &colors, settings.ui_font_size);
        let ctx = cc.egui_ctx.clone();
        let waker: Waker = Arc::new(move || ctx.request_repaint());

        let mut plugins = PluginHost::default();
        plugins.set_waker(waker.clone());

        let session = Session::load();
        if session.zoom > 0.0 {
            cc.egui_ctx.set_zoom_factor(session.zoom);
        }
        let settings_lsp = (settings.language_servers.clone(), settings.lsp_enabled);
        let waker_lsp = waker.clone();
        let agents = merge_agents(&settings.agents);
        let installed = agents.iter().map(|a| (a.id.clone(), a.is_installed())).collect();

        let mut app = Self {
            syntax_theme: highlight::load_theme(&settings.syntax_theme),
            settings,
            colors,
            session,
            workspace: None,
            tree: None,
            index: FileIndex::empty(),
            git: None,
            commit_message: String::new(),
            docs: vec![],
            active: 0,
            next_id: 1,
            untitled: 0,
            sidebar: SidebarView::Explorer,
            show_sidebar: true,
            show_terminal: false,
            show_ai: false,
            bottom: TermPanel::default(),
            ai: TermPanel::default(),
            focus_terminal: None,
            terminal_focused: false,
            palette: CommandPalette::default(),
            plugins,
            agents,
            installed,
            toasts: vec![],
            dialog: None,
            search: SearchPanel::default(),
            cursor_epoch: 0.0,
            waker,
            allow_close: false,
            pending_sends: vec![],
            lsp: LspManager::new(&settings_lsp.0, settings_lsp.1, waker_lsp),
            pending_definition: None,
            pending_references: None,
            pending_rename: None,
            pending_code_actions: None,
            pending_symbols: None,
            code_actions: vec![],
            problems: Default::default(),
            pending_format: HashMap::new(),
            saving_formatted: false,
            last_session_save: Instant::now(),
            style_dirty: false,
            focus_editor: false,
            md_cache: Default::default(),
            show_settings: false,
            settings_section: Default::default(),
            split: false,
            other: 0,
            focus_right: false,
        };
        app.load_plugins();
        if let Some(e) = settings_err {
            app.toast(e, NotifyLevel::Error);
        }

        // CLI argument: a folder or file to open. Otherwise restore the last session.
        let arg = args.get(1).map(PathBuf::from);
        match arg {
            Some(p) if p.is_dir() => app.open_workspace(p),
            Some(p) => {
                if let Some(parent) = p.canonicalize().ok().and_then(|c| c.parent().map(Path::to_path_buf)) {
                    app.open_workspace(parent);
                }
                app.open_file(&p);
            }
            None => {
                // open_workspace() saves the (still empty) session, so keep a copy first.
                let saved = app.session.clone();
                if let Some(ws) = saved.workspace.clone().filter(|w| w.is_dir()) {
                    app.open_workspace(ws);
                    for f in &saved.open_files {
                        if f.is_file() {
                            app.open_file(f);
                        }
                    }
                    app.active = saved.active.min(app.docs.len().saturating_sub(1));
                    app.restore_session_state(&saved);
                }
            }
        }
        app
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn toast(&mut self, text: impl Into<String>, level: NotifyLevel) {
        let secs = if level == NotifyLevel::Error { 8 } else { 4 };
        self.toasts.push(Toast { text: text.into(), level, until: Instant::now() + Duration::from_secs(secs) });
    }

    // ------------------------------------------------------------------
    // Workspace & files
    // ------------------------------------------------------------------

    fn open_workspace(&mut self, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        self.index = FileIndex::build(&path);
        self.git = Git::open(&path);
        self.problems.clear();
        self.tree = Some(FileTree::new(path.clone()));
        self.session.recent_workspaces.retain(|p| p != &path);
        self.session.recent_workspaces.insert(0, path.clone());
        self.session.recent_workspaces.truncate(10);
        self.workspace = Some(path);
        self.lsp.restart(&mut self.docs);
        self.load_plugins();
        self.save_session();
    }

    fn load_plugins(&mut self) {
        let mut dirs = vec![settings::plugins_dir()];
        if let Some(ws) = &self.workspace {
            dirs.push(ws.join(".zenith").join("plugins"));
        }
        let mut host = PluginHost::default();
        host.set_waker(self.waker.clone());
        let errors = host.discover(&dirs);
        self.plugins = host;
        for e in errors {
            self.toast(format!("Plugin error: {e}"), NotifyLevel::Error);
        }
    }

    fn open_file(&mut self, path: &Path) -> bool {
        let path = path.canonicalize().unwrap_or(path.to_path_buf());
        if let Some(i) = self.docs.iter().position(|d| d.path() == Some(path.as_path())) {
            self.active = i;
            return true;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.iter().take(8000).any(|b| *b == 0) {
                self.toast(
                    format!("{} is a binary file and can't be opened as text", path.display()),
                    NotifyLevel::Warning,
                );
                return false;
            }
        }
        let id = self.id();
        match Document::open(id, &path, self.syntax_theme.clone(), self.settings.tab_size) {
            Ok(mut doc) => {
                if !self.settings.insert_spaces {
                    doc.buffer.use_spaces = false;
                }
                // Replace a pristine untitled tab, like VS Code.
                if self.docs.len() == 1
                    && self.docs[0].path().is_none()
                    && !self.docs[0].buffer.is_dirty()
                    && self.docs[0].buffer.len_chars() == 0
                {
                    self.docs.clear();
                }
                if let Some(g) = &self.git {
                    doc.set_git_base(g.head_text(&path));
                    doc.blame_rx = g.blame_async(&path);
                }
                self.docs.push(doc);
                self.active = self.docs.len() - 1;
                if let Some(t) = &mut self.tree {
                    t.reveal(&path);
                }
                self.plugins.emit("file_opened", serde_json::json!({ "path": path }));
                self.focus_editor = true;
                true
            }
            Err(e) => {
                self.toast(format!("Can't open {}: {e}", path.display()), NotifyLevel::Error);
                false
            }
        }
    }

    fn new_file(&mut self) {
        self.untitled += 1;
        let id = self.id();
        let mut doc = Document::new_untitled(id, self.untitled, self.syntax_theme.clone());
        doc.buffer.tab_size = self.settings.tab_size;
        doc.buffer.use_spaces = self.settings.insert_spaces;
        self.docs.push(doc);
        self.active = self.docs.len() - 1;
        self.focus_editor = true;
    }

    fn save_doc(&mut self, idx: usize, save_as: bool) -> bool {
        let Some(doc) = self.docs.get_mut(idx) else { return false };
        let result = if doc.path().is_none() || save_as {
            let mut dlg = rfd::FileDialog::new().set_file_name(doc.title());
            if let Some(ws) = &self.workspace {
                dlg = dlg.set_directory(ws);
            }
            match dlg.save_file() {
                Some(p) => doc.save_as(p),
                None => return false,
            }
        } else {
            doc.save(self.settings.format_trim_trailing_whitespace)
        };
        match result {
            Ok(()) => {
                let path = doc.path().map(Path::to_path_buf);
                if let Some(p) = &path {
                    if let Some(t) = &mut self.tree {
                        if let Some(parent) = p.parent() {
                            t.refresh_dir(parent);
                        }
                    }
                    if Some(p.as_path()) == settings::settings_path().canonicalize().ok().as_deref()
                        || p == &settings::settings_path()
                    {
                        self.reload_settings();
                    }
                    self.plugins.emit("file_saved", serde_json::json!({ "path": p }));
                    if let (Some(g), Some(doc)) = (&self.git, self.docs.get_mut(idx)) {
                        doc.blame_rx = g.blame_async(p);
                    }
                    let root = self.workspace.clone();
                    if let Some(doc) = self.docs.get_mut(idx) {
                        self.lsp.sync(doc, root.as_deref(), true);
                        self.lsp.did_save(doc, root.as_deref());
                        if self.settings.format_on_save && !self.saving_formatted && doc.lsp_active {
                            let id = doc.id;
                            if let Some(req) = self.lsp.request_formatting(doc, root.as_deref()) {
                                self.pending_format.insert(req, (id, true));
                            }
                        }
                    }
                }
                true
            }
            Err(e) => {
                self.toast(format!("Save failed: {e}"), NotifyLevel::Error);
                false
            }
        }
    }

    fn reload_settings(&mut self) {
        let (s, err) = Settings::load();
        if let Some(e) = err {
            self.toast(e, NotifyLevel::Error);
            return;
        }
        let theme_changed = s.syntax_theme != self.settings.syntax_theme;
        self.settings = s;
        self.colors = Palette::by_name(&self.settings.theme);
        self.agents = merge_agents(&self.settings.agents);
        self.installed = self.agents.iter().map(|a| (a.id.clone(), a.is_installed())).collect();
        if theme_changed {
            self.set_syntax_theme(self.settings.syntax_theme.clone());
        }
        self.lsp.shutdown();
        self.lsp = LspManager::new(&self.settings.language_servers, self.settings.lsp_enabled, self.waker.clone());
        self.lsp.restart(&mut self.docs);
        self.style_dirty = true;
        self.toast("Settings reloaded", NotifyLevel::Info);
    }

    fn write_settings(&mut self) {
        let path = settings::settings_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        // Rewrite simple top-level keys while keeping comments/agents intact.
        let mut doc: toml::Table = toml::from_str(&text).unwrap_or_default();
        let fresh = toml::Table::try_from(&self.settings).unwrap_or_default();
        for (k, v) in fresh {
            doc.insert(k, v);
        }
        if let Ok(out) = toml::to_string_pretty(&doc) {
            let _ = std::fs::write(path, out);
        }
    }

    fn set_syntax_theme(&mut self, name: String) {
        self.syntax_theme = highlight::load_theme(&name);
        for d in &mut self.docs {
            d.highlight.set_theme(self.syntax_theme.clone());
        }
        self.settings.syntax_theme = name;
    }

    fn close_doc(&mut self, idx: usize, force: bool) {
        if idx >= self.docs.len() {
            return;
        }
        if !force && self.docs[idx].buffer.is_dirty() {
            self.dialog = Some(Dialog::UnsavedClose(self.docs[idx].id));
            return;
        }
        let doc = self.docs.remove(idx);
        self.lsp.did_close(&doc);
        let fix = |i: &mut usize, len: usize| {
            if *i > idx {
                *i -= 1;
            }
            if *i >= len {
                *i = len.saturating_sub(1);
            }
        };
        let len = self.docs.len();
        fix(&mut self.active, len);
        fix(&mut self.other, len);
    }

    fn active_doc(&mut self) -> Option<&mut Document> {
        self.docs.get_mut(self.active)
    }

    fn rel_path(&self, p: &Path) -> String {
        match &self.workspace {
            Some(ws) => p.strip_prefix(ws).unwrap_or(p).to_string_lossy().replace('\\', "/"),
            None => p.to_string_lossy().into_owned(),
        }
    }

    fn editor_context(&self) -> EditorContext {
        let mut ctx = EditorContext {
            workspace: self.workspace.as_ref().map(|p| p.to_string_lossy().into_owned()),
            ..Default::default()
        };
        if let Some(doc) = self.docs.get(self.active) {
            ctx.file = doc.path().map(|p| p.to_string_lossy().into_owned());
            ctx.language = Some(doc.language().to_string());
            ctx.text = Some(doc.buffer.text());
            ctx.selection = doc.buffer.selected_text();
            let pos = doc.buffer.cursor_pos();
            ctx.cursor_line = pos.line;
            ctx.cursor_col = pos.col;
        }
        ctx
    }

    fn save_session(&mut self) {
        self.session.workspace = self.workspace.clone();
        self.session.open_files = self.docs.iter().filter_map(|d| d.path().map(Path::to_path_buf)).collect();
        self.session.cursors =
            self.docs.iter().filter_map(|d| d.path().map(|p| (p.to_path_buf(), d.buffer.sel.head))).collect();
        self.session.active = self.active;
        self.session.show_sidebar = Some(self.show_sidebar);
        self.session.sidebar = match self.sidebar {
            SidebarView::Explorer => "explorer",
            SidebarView::Search => "search",
            SidebarView::Git => "git",
            SidebarView::Problems => "problems",
            SidebarView::Agents => "agents",
            SidebarView::Plugins => "plugins",
        }
        .into();
        self.session.show_terminal = self.show_terminal;
        self.session.show_ai = self.show_ai;
        self.session.split = self.split;
        self.session.terminals = self.bottom.tabs.iter().filter(|t| t.is_alive()).map(|t| t.spec.cwd.clone()).collect();
        self.session.agents = self
            .ai
            .tabs
            .iter()
            .filter(|t| t.is_alive())
            .filter_map(|t| match &t.kind {
                TermKind::Agent { id, .. } => Some(id.clone()),
                TermKind::Shell => None,
            })
            .collect();
        self.session.save();
        self.last_session_save = Instant::now();
    }

    /// Re-apply carets, panels, terminals and agents from the saved session.
    fn restore_session_state(&mut self, sess: &Session) {
        for (path, idx) in &sess.cursors {
            if let Some(doc) = self.docs.iter_mut().find(|d| d.path() == Some(path.as_path())) {
                let idx = (*idx).min(doc.buffer.len_chars());
                doc.buffer.set_cursor(idx, false);
                doc.reveal_line = Some(doc.buffer.char_to_pos(idx).line);
            }
        }
        if let Some(show) = sess.show_sidebar {
            self.show_sidebar = show;
        }
        self.sidebar = match sess.sidebar.as_str() {
            "search" => SidebarView::Search,
            "git" => SidebarView::Git,
            "problems" => SidebarView::Problems,
            "agents" => SidebarView::Agents,
            "plugins" => SidebarView::Plugins,
            _ => SidebarView::Explorer,
        };
        if sess.split && self.docs.len() > 1 {
            self.split = true;
            self.other = (self.active + 1) % self.docs.len();
        }
        if self.settings.restore_terminals {
            for cwd in &sess.terminals {
                self.new_terminal(cwd.clone().filter(|d| d.is_dir()));
            }
            for id in &sess.agents {
                self.start_agent(id, true);
            }
        }
        self.show_terminal = sess.show_terminal && !self.bottom.tabs.is_empty();
        self.show_ai = sess.show_ai && !self.ai.tabs.is_empty();
        self.focus_terminal = None;
        self.focus_editor = true;
    }

    // ------------------------------------------------------------------
    // Terminals & agents
    // ------------------------------------------------------------------

    fn new_terminal(&mut self, cwd: Option<PathBuf>) -> u64 {
        let id = self.id();
        let mut spec = if self.settings.shell.trim().is_empty() {
            CommandSpec::shell()
        } else {
            let mut parts = self.settings.shell.split_whitespace().map(str::to_string);
            CommandSpec::program(parts.next().unwrap_or_default(), parts.collect())
        };
        spec.cwd = cwd.or_else(|| self.workspace.clone());
        let tab = TermTab::spawn(id, TermKind::Shell, spec, self.waker.clone());
        self.bottom.tabs.push(tab);
        self.bottom.active = self.bottom.tabs.len() - 1;
        self.show_terminal = true;
        self.focus_terminal = Some(id);
        id
    }

    fn run_in_terminal(&mut self, command: &str) {
        let id = self.new_terminal(None);
        if let Some(t) = self.bottom.tabs.iter_mut().find(|t| t.id == id) {
            let mut c = command.to_string();
            c.push('\r');
            t.write(c.as_bytes());
        }
    }

    fn launch_agent(&mut self, agent_id: &str) -> Option<u64> {
        self.start_agent(agent_id, false)
    }

    /// Start an agent CLI; `resume` adds its resume arguments (continue the
    /// previous conversation) when it has any.
    fn start_agent(&mut self, agent_id: &str, resume: bool) -> Option<u64> {
        let agent = self.agents.iter().find(|a| a.id == agent_id).cloned()?;
        if !agent.is_installed() {
            self.installed.insert(agent.id.clone(), false);
            self.toast(
                format!("`{}` was not found on PATH. Install it with:  {}", agent.command, agent.install_hint),
                NotifyLevel::Warning,
            );
            self.sidebar = SidebarView::Agents;
            self.show_sidebar = true;
            return None;
        }
        self.installed.insert(agent.id.clone(), true);
        let id = self.id();
        let mut args = agent.args.clone();
        if resume {
            args.extend(agent.resume_args.iter().cloned());
        }
        let mut spec = CommandSpec::program(agent.command.clone(), args).with_cwd(self.workspace.clone());
        spec.env = agent.env.clone();
        let mut tab = TermTab::spawn(
            id,
            TermKind::Agent { id: agent.id.clone(), name: agent.name.clone(), icon: agent.icon.clone() },
            spec,
            self.waker.clone(),
        );
        if resume && !agent.resume_args.is_empty() {
            tab.resumed = Some((Instant::now(), agent.resume_args.clone()));
        }
        self.ai.tabs.push(tab);
        self.ai.active = self.ai.tabs.len() - 1;
        self.show_ai = true;
        self.focus_terminal = Some(id);
        Some(id)
    }

    /// Send text to the active (or default) agent, starting it if needed.
    fn send_to_agent(&mut self, text: String, submit: bool, agent: Option<String>) {
        let wanted = agent.unwrap_or_else(|| self.settings.default_agent.clone());
        let existing = self
            .ai
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_alive())
            .find(|(i, t)| *i == self.ai.active || matches!(&t.kind, TermKind::Agent { id, .. } if *id == wanted))
            .map(|(i, t)| (i, t.id));
        let tab_id = match existing {
            Some((i, id)) => {
                self.ai.active = i;
                id
            }
            None => match self.launch_agent(&wanted) {
                Some(id) => id,
                None => return,
            },
        };
        self.show_ai = true;
        self.focus_terminal = Some(tab_id);
        self.pending_sends.push(PendingSend { tab_id, text, submit, created: Instant::now() });
    }

    fn selection_prompt(&self, question: &str) -> String {
        let Some(doc) = self.docs.get(self.active) else { return question.to_string() };
        let rel = doc.path().map(|p| self.rel_path(p));
        let sel = doc.buffer.selected_text();
        let range = (!sel.is_empty()).then(|| {
            let a = doc.buffer.char_to_pos(doc.buffer.sel.start()).line;
            let b = doc.buffer.char_to_pos(doc.buffer.sel.end()).line;
            (a, b)
        });
        build_context_prompt(rel.as_deref(), range, &sel, question)
    }

    /// An agent started with "continue last conversation" that exits within a
    /// few seconds probably had nothing to continue: start it fresh instead.
    fn retry_failed_resumes(&mut self) {
        let waker = self.waker.clone();
        for tab in &mut self.ai.tabs {
            let Some((at, args)) = &tab.resumed else { continue };
            if at.elapsed() > Duration::from_secs(8) {
                tab.resumed = None;
            } else if tab.term.is_some() && !tab.is_alive() {
                let drop: Vec<String> = args.clone();
                tab.spec.args.retain(|a| !drop.contains(a));
                tab.resumed = None;
                tab.restart(waker.clone());
            }
        }
        if self.ai.tabs.iter().any(|t| t.resumed.is_some()) {
            let w = self.waker.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                w();
            });
        }
    }

    fn flush_pending_sends(&mut self) {
        let mut keep = vec![];
        for p in std::mem::take(&mut self.pending_sends) {
            let tab = self.ai.tabs.iter_mut().find(|t| t.id == p.tab_id);
            let Some(tab) = tab else { continue };
            // Wait until the agent has drawn its UI and gone quiet.
            let ready = tab.term.as_ref().is_some_and(|t| t.idle_for().is_some_and(|d| d > Duration::from_millis(700)));
            if ready || p.created.elapsed() > Duration::from_secs(12) {
                tab.paste(&p.text);
                if p.submit {
                    tab.write(b"\r");
                }
            } else {
                keep.push(p);
            }
        }
        if !keep.is_empty() {
            self.pending_sends = keep;
            let w = self.waker.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(250));
                w();
            });
        }
    }

    // ------------------------------------------------------------------
    // Commands
    // ------------------------------------------------------------------

    fn run(&mut self, ctx: &egui::Context, cmd: Cmd) {
        match cmd {
            Cmd::NewFile => self.new_file(),
            Cmd::OpenFile => {
                let mut d = rfd::FileDialog::new();
                if let Some(ws) = &self.workspace {
                    d = d.set_directory(ws);
                }
                if let Some(files) = d.pick_files() {
                    for f in files {
                        self.open_file(&f);
                    }
                }
            }
            Cmd::OpenFolder => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.open_workspace(dir);
                }
            }
            Cmd::Save => {
                self.save_doc(self.active, false);
            }
            Cmd::SaveAs => {
                self.save_doc(self.active, true);
            }
            Cmd::SaveAll => {
                for i in 0..self.docs.len() {
                    if self.docs[i].buffer.is_dirty() {
                        self.save_doc(i, false);
                    }
                }
            }
            Cmd::CloseTab => self.close_doc(self.active, false),
            Cmd::CloseAll => {
                if self.docs.iter().any(|d| d.buffer.is_dirty()) {
                    self.toast(
                        "Some files have unsaved changes — save or close them individually.",
                        NotifyLevel::Warning,
                    );
                    self.docs.retain(|d| d.buffer.is_dirty());
                    self.active = 0;
                } else {
                    self.docs.clear();
                }
            }
            Cmd::QuickOpen => {
                self.index.poll();
                let items = self
                    .index
                    .files
                    .iter()
                    .map(|f| Item {
                        label: f.clone(),
                        detail: String::new(),
                        action: PaletteAction::OpenFile(f.clone()),
                    })
                    .collect();
                let ph = if self.workspace.is_some() {
                    "Search files by name"
                } else {
                    "Open a folder first (Ctrl+Shift+O)"
                };
                self.palette.show(Mode::Files, items, ph);
            }
            Cmd::CommandPalette => {
                let mut extra: Vec<Item> = self
                    .plugins
                    .commands()
                    .into_iter()
                    .map(|(_, c)| Item {
                        label: c.title.clone(),
                        detail: "plugin".into(),
                        action: PaletteAction::PluginCommand(c.id.clone()),
                    })
                    .collect();
                for a in &self.agents {
                    extra.push(Item {
                        label: format!("AI: Start {}", a.name),
                        detail: a.command.clone(),
                        action: PaletteAction::LaunchAgent(a.id.clone()),
                    });
                }
                for r in &self.session.recent_workspaces {
                    extra.push(Item {
                        label: format!("File: Open Recent — {}", r.display()),
                        detail: String::new(),
                        action: PaletteAction::OpenRecent(r.clone()),
                    });
                }
                self.palette.show(Mode::Commands, command_items(extra), "Type a command");
            }
            Cmd::GotoLine => self.palette.show(Mode::GotoLine, vec![], "Line number"),
            Cmd::Find | Cmd::Replace => {
                let replace = cmd == Cmd::Replace;
                if let Some(doc) = self.active_doc() {
                    doc.find.open = true;
                    doc.find.show_replace |= replace;
                    doc.find.focus_query = true;
                    let sel = doc.buffer.selected_text();
                    if !sel.is_empty() && !sel.contains('\n') {
                        doc.find.query = sel;
                    }
                }
            }
            Cmd::FindInFiles => {
                self.sidebar = SidebarView::Search;
                self.show_sidebar = true;
                self.search.focus = true;
                if let Some(doc) = self.docs.get(self.active) {
                    let sel = doc.buffer.selected_text();
                    if !sel.is_empty() && !sel.contains('\n') {
                        self.search.query = sel;
                    }
                }
            }
            Cmd::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            Cmd::ToggleTerminal => {
                self.show_terminal = !self.show_terminal;
                if self.show_terminal {
                    if self.bottom.tabs.is_empty() {
                        self.new_terminal(None);
                    } else if let Some(t) = self.bottom.tabs.get(self.bottom.active) {
                        self.focus_terminal = Some(t.id);
                    }
                } else {
                    self.focus_editor = true;
                }
            }
            Cmd::ToggleAiPanel => {
                self.show_ai = !self.show_ai;
                if self.show_ai {
                    if let Some(t) = self.ai.tabs.get(self.ai.active) {
                        self.focus_terminal = Some(t.id);
                    }
                }
            }
            Cmd::NewTerminal => {
                self.new_terminal(None);
            }
            Cmd::NextTab | Cmd::PrevTab => {
                let n = self.docs.len();
                if n > 0 {
                    self.active = if cmd == Cmd::NextTab { (self.active + 1) % n } else { (self.active + n - 1) % n };
                }
            }
            Cmd::ZoomIn | Cmd::ZoomOut | Cmd::ZoomReset => {
                let z = ctx.zoom_factor();
                let nz = match cmd {
                    Cmd::ZoomIn => (z + 0.1).min(3.0),
                    Cmd::ZoomOut => (z - 0.1).max(0.5),
                    _ => 1.0,
                };
                ctx.set_zoom_factor(nz);
                self.session.zoom = nz;
            }
            Cmd::ToggleWordWrap => {
                self.settings.word_wrap = !self.settings.word_wrap;
                self.write_settings();
            }
            Cmd::ToggleMinimap => {
                self.settings.show_minimap = !self.settings.show_minimap;
                self.write_settings();
            }
            Cmd::SelectTheme => {
                let items =
                    [("Zenith Midnight (dark)", "zenith-midnight"), ("Zenith Daylight (light)", "zenith-daylight")]
                        .iter()
                        .map(|(l, id)| Item {
                            label: l.to_string(),
                            detail: String::new(),
                            action: PaletteAction::SetTheme(id.to_string()),
                        })
                        .collect();
                self.palette.show(Mode::Pick, items, "Select color theme");
            }
            Cmd::SelectSyntaxTheme => {
                let items = highlight::theme_names()
                    .into_iter()
                    .map(|n| Item {
                        label: n.to_string(),
                        detail: String::new(),
                        action: PaletteAction::SetSyntaxTheme(n.to_string()),
                    })
                    .collect();
                self.palette.show(Mode::Pick, items, "Select syntax theme");
            }
            Cmd::SelectLanguage => {
                let mut names: Vec<String> =
                    highlight::syntax_set().syntaxes().iter().map(|s| s.name.clone()).collect();
                names.sort_by_key(|n| n.to_lowercase());
                names.dedup();
                let items = names
                    .into_iter()
                    .map(|n| Item { label: n.clone(), detail: String::new(), action: PaletteAction::SetLanguage(n) })
                    .collect();
                self.palette.show(Mode::Pick, items, "Select language mode");
            }
            Cmd::OpenSettings => self.show_settings = true,
            Cmd::OpenSettingsFile => {
                let p = settings::settings_path();
                if !p.exists() {
                    let _ = Settings::load();
                }
                self.open_file(&p);
            }
            Cmd::OpenPluginsFolder => {
                let dir = settings::plugins_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_in_os(&dir);
            }
            Cmd::ReloadPlugins => {
                self.load_plugins();
                self.toast(format!("{} plugin(s) loaded", self.plugins.plugins.len()), NotifyLevel::Info);
            }
            Cmd::AskAgent => self.dialog = Some(Dialog::AskAgent { question: String::new(), focus: true }),
            Cmd::SendSelectionToAgent => {
                let prompt = self.selection_prompt("");
                self.send_to_agent(prompt, false, None);
            }
            Cmd::SendFileToAgent => {
                if let Some(p) = self.docs.get(self.active).and_then(|d| d.path()).map(|p| self.rel_path(p)) {
                    self.send_to_agent(format!("@{p} "), false, None);
                }
            }
            Cmd::ShowPlugins => {
                self.sidebar = SidebarView::Plugins;
                self.show_sidebar = true;
            }
            Cmd::ShowExplorer => {
                self.sidebar = SidebarView::Explorer;
                self.show_sidebar = true;
            }
            Cmd::MarkdownPreview => {
                if let Some(doc) = self.active_doc() {
                    if doc.language().starts_with("Markdown") {
                        doc.md_preview = !doc.md_preview;
                    }
                }
            }
            Cmd::GitOpenChanges => {
                if let Some(doc) = self.active_doc() {
                    doc.show_git_diff = !doc.show_git_diff;
                }
            }
            Cmd::GitSwitchBranch => {
                let Some(g) = &self.git else {
                    self.toast("This folder is not a Git repository", NotifyLevel::Info);
                    return;
                };
                let current = g.branch.clone().unwrap_or_default();
                let mut items: Vec<Item> = g
                    .branches()
                    .into_iter()
                    .map(|b| Item {
                        label: format!("⎇ {b}"),
                        detail: if b == current { "current".into() } else { String::new() },
                        action: PaletteAction::GitCheckout(b),
                    })
                    .collect();
                items.push(Item {
                    label: "+ Create new branch…".into(),
                    detail: String::new(),
                    action: PaletteAction::GitNewBranch,
                });
                self.palette.show(Mode::Pick, items, "Switch to branch");
            }
            Cmd::ShowProblems => {
                self.sidebar = SidebarView::Problems;
                self.show_sidebar = true;
            }
            Cmd::ShowSourceControl => {
                self.sidebar = SidebarView::Git;
                self.show_sidebar = true;
                if let Some(g) = &mut self.git {
                    g.refresh();
                }
            }
            Cmd::ShowAgents => {
                self.sidebar = SidebarView::Agents;
                self.show_sidebar = true;
            }
            Cmd::ReloadFile => {
                if let Some(doc) = self.active_doc() {
                    if let Err(e) = doc.reload_from_disk() {
                        self.toast(format!("Reload failed: {e}"), NotifyLevel::Error);
                    }
                }
            }
            Cmd::GotoDefinition => {
                if let Some(idx) = self.docs.get(self.active).map(|d| d.buffer.sel.head) {
                    self.goto_definition(idx);
                }
            }
            Cmd::NextProblem => {
                if let Some(msg) = self.active_doc().and_then(|d| d.next_diagnostic()) {
                    self.toast(msg, NotifyLevel::Info);
                }
            }
            Cmd::RestartLanguageServers => {
                self.lsp =
                    LspManager::new(&self.settings.language_servers, self.settings.lsp_enabled, self.waker.clone());
                self.lsp.restart(&mut self.docs);
                self.toast("Language servers restarted", NotifyLevel::Info);
            }
            Cmd::SplitEditor => {
                if self.split {
                    self.split = false;
                    self.focus_right = false;
                } else if !self.docs.is_empty() {
                    self.split = true;
                    self.other = self.active;
                    self.focus_right = true;
                }
                self.focus_editor = true;
            }
            Cmd::FocusOtherPane => {
                if self.split {
                    let r = !self.focus_right;
                    self.focus_pane(r);
                    self.focus_editor = true;
                }
            }
            Cmd::CodeActions | Cmd::GotoSymbol | Cmd::TriggerParameterHints => {
                let root = self.workspace.clone();
                let Some(doc) = self.docs.get_mut(self.active) else { return };
                if !doc.lsp_active {
                    self.toast("No language server for this file", NotifyLevel::Info);
                    return;
                }
                match cmd {
                    Cmd::CodeActions => self.pending_code_actions = self.lsp.request_code_actions(doc, root.as_deref()),
                    Cmd::GotoSymbol => self.pending_symbols = self.lsp.request_symbols(doc, root.as_deref()),
                    _ => self.lsp.request_signature_help(doc, root.as_deref()),
                }
            }
            Cmd::RenameSymbol | Cmd::FindReferences | Cmd::FormatDocument => {
                let root = self.workspace.clone();
                let Some(doc) = self.docs.get_mut(self.active) else { return };
                if !doc.lsp_active {
                    self.toast("No language server for this file", NotifyLevel::Info);
                    return;
                }
                let idx = doc.buffer.sel.head;
                let word = doc.word_range_at(idx).map(|r| doc.buffer.rope().slice(r).to_string()).unwrap_or_default();
                match cmd {
                    Cmd::RenameSymbol => {
                        if word.is_empty() {
                            self.toast("Place the cursor on a symbol to rename", NotifyLevel::Info);
                        } else {
                            self.dialog = Some(Dialog::Input {
                                title: format!("Rename `{word}`"),
                                value: word,
                                kind: InputKind::RenameSymbol(idx),
                                focus: true,
                            });
                        }
                    }
                    Cmd::FindReferences => {
                        if let Some(r) = self.lsp.request_references(doc, root.as_deref(), idx) {
                            self.pending_references = Some((r, word));
                        }
                    }
                    _ => {
                        let id = doc.id;
                        if let Some(r) = self.lsp.request_formatting(doc, root.as_deref()) {
                            self.pending_format.insert(r, (id, false));
                        }
                    }
                }
            }
            Cmd::Fold | Cmd::Unfold | Cmd::FoldAll | Cmd::UnfoldAll => {
                if let Some(doc) = self.active_doc() {
                    match cmd {
                        Cmd::Fold => doc.fold_at_cursor(),
                        Cmd::Unfold => doc.unfold_at_cursor(),
                        Cmd::FoldAll => doc.fold_all(),
                        _ => doc.unfold_all(),
                    }
                }
                self.focus_editor = true;
            }
            Cmd::About => self.dialog = Some(Dialog::About),
            Cmd::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    fn handle_palette(&mut self, ctx: &egui::Context, action: PaletteAction) {
        match action {
            PaletteAction::Command(c) => self.run(ctx, c),
            PaletteAction::PluginCommand(id) => {
                let ectx = self.editor_context();
                if let Err(e) = self.plugins.execute(&id, &ectx) {
                    self.toast(format!("{e:#}"), NotifyLevel::Error);
                }
            }
            PaletteAction::OpenFile(rel) => {
                if let Some(ws) = self.workspace.clone() {
                    self.open_file(&ws.join(rel));
                }
            }
            PaletteAction::GotoLine(n) => {
                if let Some(doc) = self.active_doc() {
                    doc.goto_line(n);
                }
                self.focus_editor = true;
            }
            PaletteAction::SetTheme(t) => {
                self.settings.theme = t;
                self.colors = Palette::by_name(&self.settings.theme);
                // Pair UI theme with a matching syntax theme.
                let syn = if self.colors.dark { "OneHalfDark" } else { "OneHalfLight" };
                self.set_syntax_theme(syn.into());
                self.style_dirty = true;
                self.write_settings();
            }
            PaletteAction::SetSyntaxTheme(t) => {
                self.set_syntax_theme(t);
                self.write_settings();
            }
            PaletteAction::SetLanguage(name) => {
                if let (Some(syn), Some(doc)) = (highlight::syntax_by_name(&name), self.docs.get_mut(self.active)) {
                    doc.highlight.set_syntax(syn);
                }
            }
            PaletteAction::LaunchAgent(id) => {
                self.launch_agent(&id);
            }
            PaletteAction::OpenRecent(p) => self.open_workspace(p),
            PaletteAction::GotoPosition(line, character) => {
                if let Some(doc) = self.active_doc() {
                    let idx = doc.lsp_to_char(zenith_lsp::LspPos { line: line as u32, character: character as u32 });
                    doc.buffer.clear_extra();
                    doc.buffer.set_cursor(idx, false);
                    doc.reveal_line = Some(line);
                }
                self.focus_editor = true;
            }
            PaletteAction::GitCheckout(b) => self.git_switch(&b, false),
            PaletteAction::GitNewBranch => {
                self.dialog = Some(Dialog::Input {
                    title: "New branch name".into(),
                    value: String::new(),
                    kind: InputKind::NewBranch,
                    focus: true,
                })
            }
            PaletteAction::CodeAction(i) => {
                let Some(action) = self.code_actions.get(i).cloned() else { return };
                self.apply_edits_quiet(action.edits, false);
                if let Some(cmd) = &action.command {
                    let root = self.workspace.clone();
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        self.lsp.execute_command(doc, root.as_deref(), cmd);
                    }
                }
                self.focus_editor = true;
            }
        }
    }

    fn handle_plugin_events(&mut self) {
        for ev in self.plugins.poll() {
            match ev.kind {
                PluginEventKind::Action(a) => self.apply_plugin_action(&ev.plugin_id, a),
                PluginEventKind::Error(e) => self.toast(format!("[{}] {e}", ev.plugin_id), NotifyLevel::Error),
                PluginEventKind::Log(l) => log::info!("[plugin {}] {l}", ev.plugin_id),
                PluginEventKind::Exited => log::info!("plugin {} exited", ev.plugin_id),
            }
        }
    }

    fn apply_plugin_action(&mut self, plugin: &str, a: Action) {
        match a {
            Action::Notify { message, level } => self.toast(format!("{plugin}: {message}"), level),
            Action::ReplaceSelection { text } | Action::Insert { text } => {
                if let Some(doc) = self.active_doc() {
                    doc.buffer.insert(&text);
                }
            }
            Action::SetText { text } => {
                if let Some(doc) = self.active_doc() {
                    doc.buffer.set_text(&text);
                }
            }
            Action::OpenFile { path, line } => {
                let p = PathBuf::from(&path);
                let p = if p.is_relative() { self.workspace.clone().map(|w| w.join(&p)).unwrap_or(p) } else { p };
                if self.open_file(&p) {
                    if let (Some(l), Some(doc)) = (line, self.docs.get_mut(self.active)) {
                        doc.goto_line(l.saturating_sub(1));
                    }
                }
            }
            Action::RunInTerminal { command } => self.run_in_terminal(&command),
            Action::SendToAgent { text, agent } => self.send_to_agent(text, true, agent),
        }
    }

    fn handle_tree_action(&mut self, ctx: &egui::Context, a: TreeAction) {
        match a {
            TreeAction::Open(p) => {
                self.open_file(&p);
            }
            TreeAction::NewFile(dir) => {
                self.dialog = Some(Dialog::Input {
                    title: "New File".into(),
                    value: String::new(),
                    kind: InputKind::NewFile(dir),
                    focus: true,
                })
            }
            TreeAction::NewFolder(dir) => {
                self.dialog = Some(Dialog::Input {
                    title: "New Folder".into(),
                    value: String::new(),
                    kind: InputKind::NewFolder(dir),
                    focus: true,
                })
            }
            TreeAction::Rename(p) => {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.dialog = Some(Dialog::Input {
                    title: "Rename".into(),
                    value: name,
                    kind: InputKind::Rename(p),
                    focus: true,
                });
            }
            TreeAction::Delete(p) => self.dialog = Some(Dialog::ConfirmDelete(p)),
            TreeAction::CopyPath(p) => ctx.copy_text(p.to_string_lossy().into_owned()),
            TreeAction::RevealInTerminal(dir) => {
                self.new_terminal(Some(dir));
            }
        }
    }

    // ------------------------------------------------------------------
    // Language servers
    // ------------------------------------------------------------------

    fn handle_lsp(&mut self) {
        let root = self.workspace.clone();
        for doc in &mut self.docs {
            self.lsp.sync(doc, root.as_deref(), false);
        }
        for outcome in self.lsp.poll(&mut self.docs, &mut self.pending_definition) {
            match outcome {
                LspOutcome::GotoLocation { path, line, character } => {
                    if self.open_file(&path) {
                        if let Some(doc) = self.docs.get_mut(self.active) {
                            let idx = doc.lsp_to_char(zenith_lsp::LspPos { line, character });
                            doc.buffer.clear_extra();
                            doc.buffer.set_cursor(idx, false);
                            doc.reveal_line = Some(line as usize);
                        }
                        self.focus_editor = true;
                    }
                }
                LspOutcome::Message(m) => self.toast(m, NotifyLevel::Info),
                LspOutcome::References { request, locations } => {
                    if self.pending_references.as_ref().is_some_and(|(r, _)| *r == request) {
                        let (_, name) = self.pending_references.take().unwrap();
                        self.show_references(name, locations);
                    }
                }
                LspOutcome::WorkspaceEdit { request, edits, error } => {
                    if self.pending_rename == Some(request) {
                        self.pending_rename = None;
                        match error {
                            Some(e) => self.toast(format!("Rename failed: {e}"), NotifyLevel::Error),
                            None => self.apply_workspace_edit(edits),
                        }
                    }
                }
                LspOutcome::ApplyEdit { edits } => self.apply_edits_quiet(edits, false),
                LspOutcome::Diagnostics { path, mut diagnostics } => {
                    // Hints are mostly "related information" noise in a list view.
                    diagnostics.retain(|d| d.severity != zenith_lsp::Severity::Hint);
                    if diagnostics.is_empty() {
                        self.problems.remove(&path);
                    } else {
                        self.problems.insert(path, diagnostics);
                    }
                }
                LspOutcome::CodeActions { request, actions } => {
                    if self.pending_code_actions == Some(request) {
                        self.pending_code_actions = None;
                        if actions.is_empty() {
                            self.toast("No code actions available here", NotifyLevel::Info);
                        } else {
                            let items = actions
                                .iter()
                                .enumerate()
                                .map(|(i, a)| Item {
                                    label: format!("{}{}", if a.preferred { "★ " } else { "" }, a.title),
                                    detail: a.kind.clone(),
                                    action: PaletteAction::CodeAction(i),
                                })
                                .collect();
                            self.code_actions = actions;
                            self.palette.show(Mode::Pick, items, "Quick fix / code action");
                        }
                    }
                }
                LspOutcome::Symbols { request, symbols } => {
                    if self.pending_symbols == Some(request) {
                        self.pending_symbols = None;
                        let items = symbols
                            .iter()
                            .map(|s| Item {
                                label: format!("{}{}", "  ".repeat(s.depth), s.name),
                                detail: format!("{}  {}", s.kind_label(), s.line + 1),
                                action: PaletteAction::GotoPosition(s.line as usize, s.character as usize),
                            })
                            .collect();
                        self.palette.show(Mode::Pick, items, "Go to symbol in file");
                    }
                }
                LspOutcome::Formatting { request, edits } => {
                    if let Some((doc_id, save)) = self.pending_format.remove(&request) {
                        if let Some(i) = self.docs.iter().position(|d| d.id == doc_id) {
                            let changed = !edits.is_empty();
                            self.docs[i].apply_lsp_edits(&edits);
                            if save && changed {
                                self.saving_formatted = true;
                                self.save_doc(i, false);
                                self.saving_formatted = false;
                            }
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Git
    // ------------------------------------------------------------------

    fn handle_git(&mut self) {
        let Some(g) = &mut self.git else { return };
        if g.poll() {
            let map = g.status.iter().map(|e| (e.path.clone(), e.letter())).collect();
            if let Some(t) = &mut self.tree {
                t.set_git_status(map);
            }
        }
        if let Some(doc) = self.docs.get_mut(self.active) {
            doc.update_git_hunks();
        }
        for d in &mut self.docs {
            if let Some(rx) = &d.blame_rx {
                if let Ok(b) = rx.try_recv() {
                    d.blame = Some(b);
                    d.blame_rx = None;
                }
            }
        }
    }

    /// Check out (or create) a branch, then refresh open files from disk.
    fn git_switch(&mut self, branch: &str, create: bool) {
        let Some(g) = &mut self.git else { return };
        let res = if create { g.create_branch(branch) } else { g.checkout(branch) };
        match res {
            Ok(()) => {
                // Clean buffers follow the new branch silently (not an agent edit).
                for d in &mut self.docs {
                    if !d.buffer.is_dirty() && d.changed_on_disk() {
                        let _ = d.reload_from_disk();
                    }
                }
                self.reload_git_bases();
                if let Some(t) = &mut self.tree {
                    t.refresh();
                }
                self.toast(format!("Switched to branch {branch}"), NotifyLevel::Info);
            }
            Err(e) => self.toast(format!("git: {e}"), NotifyLevel::Error),
        }
    }

    /// Re-read HEAD contents for open files (after commit / checkout).
    fn reload_git_bases(&mut self) {
        let Some(g) = &self.git else { return };
        for d in &mut self.docs {
            if let Some(p) = d.path().map(Path::to_path_buf) {
                d.set_git_base(g.head_text(&p));
                d.blame_rx = g.blame_async(&p);
            }
        }
    }

    fn git_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            Self::section_header(ui, p, "SOURCE CONTROL");
            if let Some(g) = &mut self.git {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                        g.refresh();
                    }
                });
            }
        });
        let Some(g) = &mut self.git else {
            ui.label(RichText::new("This folder is not a Git repository.").color(p.text_dim));
            if self.workspace.is_some() && ui.button("Initialize Repository").clicked() {
                if let Some(ws) = self.workspace.clone() {
                    match crate::git::init(&ws) {
                        Ok(()) => self.git = Git::open(&ws),
                        Err(e) => self.toast(e, NotifyLevel::Error),
                    }
                }
            }
            return;
        };
        if let Some(b) = &g.branch {
            ui.label(RichText::new(format!("⎇ {b}")).color(p.accent));
        }
        ui.add_space(4.0);
        let staged_n = g.status.iter().filter(|e| e.is_staged()).count();
        let msg = ui.add(
            egui::TextEdit::multiline(&mut self.commit_message)
                .hint_text(if staged_n > 0 {
                    "Message (Ctrl+Enter to commit staged)"
                } else {
                    "Message (Ctrl+Enter to commit all)"
                })
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        let ctrl_enter = msg.has_focus()
            && ui.input(|i| {
                i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if modifiers.command))
            });
        let mut commit = ctrl_enter;
        let mut ai_commit = false;
        let mut result: Option<Result<String, String>> = None;
        ui.horizontal(|ui| {
            let w = ui.available_width();
            let can = !self.commit_message.trim().is_empty() && !g.status.is_empty();
            if ui
                .add_enabled(
                    can,
                    egui::Button::new(RichText::new("✓ Commit").color(Color32::WHITE))
                        .fill(p.accent)
                        .min_size(egui::vec2(w * 0.5, 26.0)),
                )
                .clicked()
            {
                commit = true;
            }
            if ui
                .add_enabled(
                    !g.status.is_empty(),
                    egui::Button::new("✳ Commit with AI").min_size(egui::vec2(ui.available_width(), 26.0)),
                )
                .on_hover_text("Ask your AI agent to review the diff and write the commit")
                .clicked()
            {
                ai_commit = true;
            }
        });
        ui.horizontal(|ui| {
            if ui.small_button("⬆ Push").clicked() {
                result = Some(g.push());
            }
            if ui.small_button("⬇ Pull").clicked() {
                result = Some(g.pull());
                g.refresh();
            }
            if ui.small_button("+ Stage all").clicked() {
                result = Some(g.stage_all().map(|_| String::new()));
            }
        });
        if commit && !self.commit_message.trim().is_empty() {
            let r = g.commit(self.commit_message.trim());
            if r.is_ok() {
                self.commit_message.clear();
            }
            result = Some(r);
        }
        ui.add_space(6.0);

        let mut open = None;
        let mut action: Option<(bool, String)> = None; // (stage?, rel)
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (title, staged) in [("STAGED CHANGES", true), ("CHANGES", false)] {
                let list: Vec<_> =
                    g.status.iter().filter(|e| if staged { e.is_staged() } else { e.is_unstaged() }).collect();
                if list.is_empty() {
                    continue;
                }
                ui.label(RichText::new(format!("{title}  {}", list.len())).size(11.0).color(p.text_dim).strong());
                for e in list {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, p.bg_hover);
                    }
                    let letter = e.letter();
                    let color = match letter {
                        'U' | 'A' => p.success,
                        'D' => p.error,
                        _ => p.warning,
                    };
                    let (dir, name) = e.rel.rsplit_once('/').unwrap_or(("", &e.rel));
                    let (icon, icolor) = theme::file_icon(name);
                    ui.painter().text(
                        egui::pos2(rect.min.x + 10.0, rect.center().y),
                        Align2::CENTER_CENTER,
                        icon,
                        FontId::proportional(11.0),
                        icolor,
                    );
                    let r = ui.painter().text(
                        egui::pos2(rect.min.x + 22.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        name,
                        FontId::proportional(13.0),
                        p.text,
                    );
                    ui.painter()
                        .with_clip_rect(egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x - 44.0, rect.max.y)))
                        .text(
                            egui::pos2(r.max.x + 6.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            dir,
                            FontId::proportional(11.5),
                            p.text_dim,
                        );
                    ui.painter().text(
                        rect.right_center() - egui::vec2(8.0, 0.0),
                        Align2::RIGHT_CENTER,
                        letter.to_string(),
                        FontId::monospace(12.0),
                        color,
                    );
                    let btn = egui::Rect::from_center_size(
                        rect.right_center() - egui::vec2(30.0, 0.0),
                        egui::vec2(18.0, 18.0),
                    );
                    let b = ui.interact(btn, ui.id().with((&e.rel, staged)), Sense::click());
                    if resp.hovered() || b.hovered() {
                        ui.painter().rect_filled(btn, 4.0, if b.hovered() { p.bg_active } else { p.bg_hover });
                        ui.painter().text(
                            btn.center(),
                            Align2::CENTER_CENTER,
                            if staged { "−" } else { "+" },
                            FontId::monospace(13.0),
                            p.text,
                        );
                    }
                    if b.on_hover_text(if staged { "Unstage" } else { "Stage" }).clicked() {
                        action = Some((!staged, e.rel.clone()));
                    } else if resp.clicked() {
                        open = Some(e.path.clone());
                    }
                }
                ui.add_space(8.0);
            }
            if g.status.is_empty() {
                ui.label(RichText::new("No changes.").color(p.text_dim));
            }
        });
        if let Some((stage, rel)) = action {
            result = Some(if stage { g.stage(&rel) } else { g.unstage(&rel) }.map(|_| String::new()));
        }
        let committed = commit && matches!(result, Some(Ok(_)));
        if let Some(r) = result {
            match r {
                Ok(m) if !m.trim().is_empty() => self.toast(m.trim().to_string(), NotifyLevel::Info),
                Ok(_) => {}
                Err(e) => self.toast(e, NotifyLevel::Error),
            }
        }
        if committed {
            self.reload_git_bases();
        }
        if let Some(path) = open {
            if self.open_file(&path) {
                // Show what changed; "Open file" in the diff goes back to editing.
                if let Some(doc) = self.docs.get_mut(self.active) {
                    doc.show_git_diff = doc.git_base.is_some();
                }
            }
        }
        if ai_commit {
            let msg = if self.commit_message.trim().is_empty() {
                String::new()
            } else {
                format!(" Use this as a hint for the message: \"{}\".", self.commit_message.trim())
            };
            self.send_to_agent(
                format!("Review my uncommitted changes (git status / git diff), then stage them and create a git commit with a clear, concise message.{msg}"),
                true,
                None,
            );
        }
        let _ = ctx;
    }

    /// Apply a rename: open documents are edited in their buffers (undoable),
    /// other files are rewritten on disk.
    fn apply_workspace_edit(&mut self, edits: Vec<(PathBuf, Vec<zenith_lsp::TextEdit>)>) {
        self.apply_edits_quiet(edits, true);
    }

    /// Apply edits from the language server; `announce` shows a rename summary.
    fn apply_edits_quiet(&mut self, edits: Vec<(PathBuf, Vec<zenith_lsp::TextEdit>)>, announce: bool) {
        let mut files = 0;
        let mut total = 0;
        for (path, file_edits) in edits {
            if file_edits.is_empty() {
                continue;
            }
            files += 1;
            total += file_edits.len();
            let canon = path.canonicalize().unwrap_or(path.clone());
            if let Some(doc) = self.docs.iter_mut().find(|d| d.path() == Some(canon.as_path())) {
                doc.apply_lsp_edits(&file_edits);
            } else {
                let res = std::fs::read_to_string(&path)
                    .and_then(|t| std::fs::write(&path, zenith_lsp::apply_text_edits(&t, &file_edits)));
                if let Err(e) = res {
                    self.toast(format!("{}: {e}", path.display()), NotifyLevel::Error);
                }
            }
        }
        if !announce {
            return;
        }
        if total == 0 {
            self.toast("Nothing to rename here", NotifyLevel::Info);
        } else {
            self.toast(format!("Renamed {total} occurrence(s) in {files} file(s)"), NotifyLevel::Info);
        }
    }

    /// Show reference locations in the Search panel.
    fn show_references(&mut self, name: String, locations: Vec<zenith_lsp::Location>) {
        let mut hits = vec![];
        for l in &locations {
            let line = l.range.start.line as usize;
            let canon = l.path.canonicalize().unwrap_or(l.path.clone());
            let text = match self.docs.iter().find(|d| d.path() == Some(canon.as_path())) {
                Some(d) => d.buffer.line_text(line),
                None => std::fs::read_to_string(&l.path)
                    .ok()
                    .and_then(|t| t.lines().nth(line).map(str::to_string))
                    .unwrap_or_default(),
            };
            let shown: String = text.trim().chars().take(200).collect();
            let matches = shown
                .match_indices(name.as_str())
                .map(|(b, m)| {
                    let s = shown[..b].chars().count();
                    (s, s + m.chars().count())
                })
                .collect();
            hits.push(SearchHit { rel: self.rel_path(&canon), path: canon, line, text: shown, matches });
        }
        hits.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
        self.toast(format!("{} reference(s) to `{name}`", hits.len()), NotifyLevel::Info);
        self.search.job = None;
        self.search.query = name;
        self.search.results = hits;
        self.sidebar = SidebarView::Search;
        self.show_sidebar = true;
    }

    fn problems_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        use zenith_lsp::Severity;
        let count = |s: Severity| self.problems.values().flatten().filter(|d| d.severity == s).count();
        let (e, w) = (count(Severity::Error), count(Severity::Warning));
        Self::section_header(ui, p, "PROBLEMS");
        ui.label(RichText::new(format!("{e} errors, {w} warnings")).small().color(p.text_dim));
        ui.add_space(6.0);
        if self.problems.is_empty() {
            ui.label(RichText::new("No problems have been detected.").color(p.text_dim));
            return;
        }
        let mut open: Option<(PathBuf, zenith_lsp::LspPos)> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (path, diags) in &self.problems {
                let rel = self.rel_path(path);
                let (dir, name) = rel.rsplit_once('/').unwrap_or(("", &rel));
                let (icon, color) = theme::file_icon(name);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon).color(color));
                    ui.label(RichText::new(name).strong());
                    ui.label(RichText::new(dir).small().color(p.text_dim));
                    ui.label(RichText::new(diags.len().to_string()).small().color(p.text_faint));
                });
                let mut sorted: Vec<&zenith_lsp::Diagnostic> = diags.iter().collect();
                sorted.sort_by_key(|d| (d.severity, d.range.start));
                for d in sorted {
                    let (glyph, c) = match d.severity {
                        Severity::Error => ("⊗", p.error),
                        Severity::Warning => ("⚠", p.warning),
                        _ => ("ℹ", p.accent),
                    };
                    let msg = d.message.lines().next().unwrap_or_default();
                    let text = format!("{msg}  [{}:{}]", d.range.start.line + 1, d.range.start.character + 1);
                    let r = ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(RichText::new(glyph).color(c));
                        ui.add(egui::Label::new(RichText::new(text).size(12.5)).truncate().sense(Sense::click()))
                    });
                    let resp = r.inner;
                    if resp.hovered() {
                        ui.painter().rect_filled(r.response.rect.expand(1.0), 3.0, p.bg_hover.gamma_multiply(0.5));
                    }
                    if resp.on_hover_text(&d.message).clicked() {
                        open = Some((path.clone(), d.range.start));
                    }
                }
                ui.add_space(6.0);
            }
        });
        if let Some((path, pos)) = open {
            if self.open_file(&path) {
                if let Some(doc) = self.docs.get_mut(self.active) {
                    let idx = doc.lsp_to_char(pos);
                    doc.buffer.clear_extra();
                    doc.buffer.set_cursor(idx, false);
                    doc.reveal_line = Some(pos.line as usize);
                }
                self.focus_editor = true;
            }
        }
    }

    fn goto_definition(&mut self, idx: usize) {
        let root = self.workspace.clone();
        let Some(doc) = self.docs.get_mut(self.active) else { return };
        if !doc.lsp_active {
            self.toast("No language server for this file (see Settings → language_servers)", NotifyLevel::Info);
            return;
        }
        self.pending_definition = self.lsp.request_definition(doc, root.as_deref(), idx);
    }

    // ------------------------------------------------------------------
    // Shortcuts
    // ------------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) -> Vec<Cmd> {
        let cmd = Modifiers::COMMAND;
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let ca = Modifiers::COMMAND | Modifiers::ALT;
        let ctrl = Modifiers::CTRL;
        let mac = cfg!(target_os = "macos");
        // When a terminal has focus on Linux/Windows, plain Ctrl+letter must
        // reach the shell, so only Ctrl+Shift / special combos are global.
        let term = self.terminal_focused && !mac;
        let mut table: Vec<(Modifiers, Key, Cmd, bool)> = vec![
            // (mods, key, command, allowed while terminal focused)
            (cs, Key::P, Cmd::CommandPalette, true),
            (cs, Key::O, Cmd::OpenFolder, true),
            (cs, Key::S, Cmd::SaveAs, false),
            (cs, Key::F, Cmd::FindInFiles, true),
            (cs, Key::A, Cmd::ToggleAiPanel, true),
            (cs, Key::L, Cmd::SendSelectionToAgent, false),
            (cs, Key::I, Cmd::AskAgent, true),
            (cs, Key::E, Cmd::ShowExplorer, true),
            (cs, Key::G, Cmd::ShowSourceControl, true),
            (cs, Key::M, Cmd::ShowProblems, true),
            (ca, Key::P, Cmd::MarkdownPreview, false),
            (Modifiers::ALT, Key::Z, Cmd::ToggleWordWrap, false),
            (Modifiers::SHIFT | Modifiers::ALT, Key::F, Cmd::FormatDocument, false),
            (Modifiers::SHIFT, Key::F12, Cmd::FindReferences, false),
            (Modifiers::NONE, Key::F2, Cmd::RenameSymbol, false),
            (cs, Key::Space, Cmd::TriggerParameterHints, false),
            (cmd, Key::Period, Cmd::CodeActions, false),
            (cmd, Key::T, Cmd::GotoSymbol, false),
            (cs, Key::Backtick, Cmd::NewTerminal, true),
            (ctrl | Modifiers::SHIFT, Key::Tab, Cmd::PrevTab, false),
            (ctrl, Key::Tab, Cmd::NextTab, false),
            (ca, Key::S, Cmd::SaveAll, false),
            (ctrl, Key::Backtick, Cmd::ToggleTerminal, true),
            (cmd, Key::P, Cmd::QuickOpen, false),
            (cmd, Key::O, Cmd::OpenFile, false),
            (cmd, Key::S, Cmd::Save, false),
            (cmd, Key::N, Cmd::NewFile, false),
            (cmd, Key::W, Cmd::CloseTab, false),
            (cmd, Key::G, Cmd::GotoLine, false),
            (cmd, Key::F, Cmd::Find, false),
            (cmd, Key::H, Cmd::Replace, false),
            (cmd, Key::B, Cmd::ToggleSidebar, false),
            (ctrl | Modifiers::ALT, Key::Backslash, Cmd::FocusOtherPane, true),
            (cmd, Key::Backslash, Cmd::SplitEditor, true),
            (cmd, Key::Equals, Cmd::ZoomIn, true),
            (cmd, Key::Plus, Cmd::ZoomIn, true),
            (cmd, Key::Minus, Cmd::ZoomOut, true),
            (cmd, Key::Num0, Cmd::ZoomReset, true),
            (cmd, Key::Comma, Cmd::OpenSettings, true),
            (cmd, Key::Q, Cmd::Quit, false),
        ];
        if mac {
            table.retain(|(_, k, _, _)| *k != Key::H); // Cmd+H hides the app on macOS
            table.push((ca, Key::F, Cmd::Replace, false));
        }
        let mut out = vec![];
        ctx.input_mut(|i| {
            for (m, k, c, in_term) in table {
                if term && !in_term {
                    continue;
                }
                if i.consume_shortcut(&KeyboardShortcut::new(m, k)) {
                    out.push(c);
                }
            }
            if i.consume_key(Modifiers::NONE, Key::F1) {
                out.push(Cmd::CommandPalette);
            }
            // Some platforms also send the typed character for Alt/Ctrl
            // combinations; it must not reach the editor once handled.
            if !out.is_empty() {
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
            }
        });
        out
    }

    // ------------------------------------------------------------------
    // UI pieces
    // ------------------------------------------------------------------

    fn menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut cmds = vec![];
        egui::menu::bar(ui, |ui| {
            ui.label(RichText::new("◆").color(self.colors.accent).size(15.0));
            let mut item = |ui: &mut egui::Ui, c: Cmd, label: &str| {
                if ui.add(egui::Button::new(label).shortcut_text(c.shortcut())).clicked() {
                    cmds.push(c);
                    ui.close_menu();
                }
            };
            ui.menu_button("File", |ui| {
                ui.set_min_width(240.0);
                item(ui, Cmd::NewFile, "New File");
                item(ui, Cmd::OpenFile, "Open File…");
                item(ui, Cmd::OpenFolder, "Open Folder…");
                ui.menu_button("Open Recent", |ui| {
                    for r in self.session.recent_workspaces.clone() {
                        if ui.button(r.display().to_string()).clicked() {
                            self.open_workspace(r);
                            ui.close_menu();
                        }
                    }
                });
                ui.separator();
                item(ui, Cmd::Save, "Save");
                item(ui, Cmd::SaveAs, "Save As…");
                item(ui, Cmd::SaveAll, "Save All");
                ui.separator();
                item(ui, Cmd::OpenSettings, "Settings");
                ui.separator();
                item(ui, Cmd::CloseTab, "Close Editor");
                item(ui, Cmd::Quit, "Quit");
            });
            ui.menu_button("Edit", |ui| {
                ui.set_min_width(240.0);
                item(ui, Cmd::Find, "Find");
                item(ui, Cmd::Replace, "Replace");
                item(ui, Cmd::FindInFiles, "Find in Files");
                ui.separator();
                item(ui, Cmd::SelectLanguage, "Change Language Mode…");
            });
            ui.menu_button("View", |ui| {
                ui.set_min_width(240.0);
                item(ui, Cmd::CommandPalette, "Command Palette…");
                item(ui, Cmd::QuickOpen, "Quick Open…");
                ui.separator();
                item(ui, Cmd::ToggleSidebar, "Toggle Sidebar");
                item(ui, Cmd::ToggleTerminal, "Toggle Terminal");
                item(ui, Cmd::ToggleAiPanel, "Toggle AI Panel");
                item(ui, Cmd::ToggleMinimap, "Toggle Minimap");
                item(ui, Cmd::ToggleWordWrap, "Toggle Word Wrap");
                item(ui, Cmd::SplitEditor, "Split Editor");
                ui.separator();
                item(ui, Cmd::SelectTheme, "Color Theme…");
                item(ui, Cmd::SelectSyntaxTheme, "Syntax Theme…");
                ui.separator();
                item(ui, Cmd::ZoomIn, "Zoom In");
                item(ui, Cmd::ZoomOut, "Zoom Out");
                item(ui, Cmd::ZoomReset, "Reset Zoom");
            });
            ui.menu_button("Go", |ui| {
                ui.set_min_width(240.0);
                item(ui, Cmd::QuickOpen, "Go to File…");
                item(ui, Cmd::GotoLine, "Go to Line…");
                item(ui, Cmd::NextTab, "Next Editor");
                item(ui, Cmd::PrevTab, "Previous Editor");
            });
            ui.menu_button("Terminal", |ui| {
                ui.set_min_width(240.0);
                item(ui, Cmd::NewTerminal, "New Terminal");
                item(ui, Cmd::ToggleTerminal, "Toggle Terminal");
            });
            ui.menu_button("AI", |ui| {
                ui.set_min_width(260.0);
                for a in self.agents.clone() {
                    let ok = self.installed.get(&a.id).copied().unwrap_or(false);
                    let label = format!("{}  Start {}", a.icon, a.name);
                    if ui.add(egui::Button::new(label).shortcut_text(if ok { "" } else { "not installed" })).clicked() {
                        self.launch_agent(&a.id);
                        ui.close_menu();
                    }
                }
                ui.separator();
                item(ui, Cmd::AskAgent, "Ask About Selection…");
                item(ui, Cmd::SendSelectionToAgent, "Send Selection");
                item(ui, Cmd::SendFileToAgent, "Send Current File");
                item(ui, Cmd::ToggleAiPanel, "Toggle AI Panel");
            });
            ui.menu_button("Plugins", |ui| {
                ui.set_min_width(240.0);
                item(ui, Cmd::ShowPlugins, "Manage Plugins");
                item(ui, Cmd::ReloadPlugins, "Reload Plugins");
                item(ui, Cmd::OpenPluginsFolder, "Open Plugins Folder");
                let cmds_list: Vec<(String, String)> =
                    self.plugins.commands().into_iter().map(|(_, c)| (c.id.clone(), c.title.clone())).collect();
                if !cmds_list.is_empty() {
                    ui.separator();
                }
                for (id, title) in cmds_list {
                    if ui.button(title).clicked() {
                        let ectx = self.editor_context();
                        if let Err(e) = self.plugins.execute(&id, &ectx) {
                            self.toast(format!("{e:#}"), NotifyLevel::Error);
                        }
                        ui.close_menu();
                    }
                }
            });
            ui.menu_button("Help", |ui| {
                item(ui, Cmd::About, "About Zenith");
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let p = &self.colors;
                let toggle = |ui: &mut egui::Ui, on: bool, glyph: &str, tip: &str| -> bool {
                    ui.add(
                        egui::Button::new(RichText::new(glyph).color(if on { p.accent } else { p.text_dim }))
                            .frame(false),
                    )
                    .on_hover_text(tip)
                    .clicked()
                };
                if toggle(ui, self.show_ai, "✳", "Toggle AI panel") {
                    cmds.push(Cmd::ToggleAiPanel);
                }
                if toggle(ui, self.show_terminal, "⌨", "Toggle terminal") {
                    cmds.push(Cmd::ToggleTerminal);
                }
                if toggle(ui, self.show_sidebar, "◧", "Toggle sidebar") {
                    cmds.push(Cmd::ToggleSidebar);
                }
                ui.add_space(8.0);
                let title = match &self.workspace {
                    Some(w) => w.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    None => "Zenith".into(),
                };
                let search = egui::Button::new(RichText::new(format!("🔍  {title}")).color(p.text_dim))
                    .min_size(egui::vec2(260.0, 22.0))
                    .fill(p.bg_elevated);
                if ui.add(search).on_hover_text("Quick open / command palette").clicked() {
                    cmds.push(Cmd::QuickOpen);
                }
            });
        });
        for c in cmds {
            self.run(ctx, c);
        }
    }

    fn activity_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.colors.clone();
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            for (view, glyph, tip) in [
                (SidebarView::Explorer, "🗐", "Explorer (Ctrl+Shift+E)"),
                (SidebarView::Search, "🔍", "Search (Ctrl+Shift+F)"),
                (SidebarView::Git, "⎇", "Source Control (Ctrl+Shift+G)"),
                (SidebarView::Problems, "⚠", "Problems (Ctrl+Shift+M)"),
                (SidebarView::Agents, "✳", "AI Agents"),
                (SidebarView::Plugins, "⊞", "Plugins"),
            ] {
                let active = self.show_sidebar && self.sidebar == view;
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(44.0, 44.0), Sense::click());
                if active {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
                        0.0,
                        p.accent,
                    );
                }
                let color = if active || resp.hovered() { p.text } else { p.text_faint };
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(19.0), color);
                if resp.on_hover_text(tip).clicked() {
                    if active {
                        self.show_sidebar = false;
                    } else {
                        self.sidebar = view;
                        self.show_sidebar = true;
                    }
                }
            }
            ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                ui.add_space(8.0);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(44.0, 40.0), Sense::click());
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "⚙",
                    FontId::proportional(18.0),
                    if resp.hovered() { p.text } else { p.text_faint },
                );
                if resp.on_hover_text("Settings").clicked() {
                    self.run(ui.ctx(), Cmd::OpenSettings);
                }
            });
        });
    }

    fn section_header(ui: &mut egui::Ui, p: &Palette, title: &str) {
        ui.add_space(8.0);
        ui.label(RichText::new(title).size(11.0).color(p.text_dim).strong());
        ui.add_space(4.0);
    }

    fn sidebar_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let p = self.colors.clone();
        match self.sidebar {
            SidebarView::Explorer => {
                ui.horizontal(|ui| {
                    Self::section_header(ui, &p, "EXPLORER");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(root) = self.workspace.clone() {
                            if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                                if let Some(t) = &mut self.tree {
                                    t.refresh();
                                }
                                self.index = FileIndex::build(&root);
                            }
                            if ui.small_button("🗀+").on_hover_text("New folder").clicked() {
                                self.handle_tree_action(ctx, TreeAction::NewFolder(root.clone()));
                            }
                            if ui.small_button("🗋+").on_hover_text("New file").clicked() {
                                self.handle_tree_action(ctx, TreeAction::NewFile(root));
                            }
                        }
                    });
                });
                if let Some(ws) = &self.workspace {
                    let name = ws.file_name().map(|n| n.to_string_lossy().to_uppercase()).unwrap_or_default();
                    ui.label(RichText::new(name).strong().size(12.0));
                    ui.add_space(2.0);
                }
                let active_path = self.docs.get(self.active).and_then(|d| d.path()).map(Path::to_path_buf);
                let action = match &mut self.tree {
                    Some(tree) => tree.ui(ui, &p, active_path.as_deref()),
                    None => {
                        ui.add_space(12.0);
                        ui.label(RichText::new("No folder opened.").color(p.text_dim));
                        ui.add_space(8.0);
                        if ui
                            .add(
                                egui::Button::new("Open Folder")
                                    .fill(p.accent)
                                    .min_size(egui::vec2(ui.available_width(), 30.0)),
                            )
                            .clicked()
                        {
                            self.run(ctx, Cmd::OpenFolder);
                        }
                        None
                    }
                };
                if let Some(a) = action {
                    self.handle_tree_action(ctx, a);
                }
            }
            SidebarView::Search => self.search_ui(ui, &p),
            SidebarView::Agents => self.agents_ui(ctx, ui, &p),
            SidebarView::Plugins => self.plugins_ui(ctx, ui, &p),
            SidebarView::Git => self.git_ui(ctx, ui, &p),
            SidebarView::Problems => self.problems_ui(ui, &p),
        }
    }

    fn search_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        Self::section_header(ui, p, "SEARCH");
        let Some(root) = self.workspace.clone() else {
            ui.label(RichText::new("Open a folder to search in files.").color(p.text_dim));
            return;
        };
        let mut start = false;
        let mut changed = false;
        let mut replace_all = false;
        // Right-to-left so the text field takes exactly the remaining width
        // (otherwise the panel grows a little every frame).
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 28.0),
            Layout::right_to_left(Align::Center),
            |ui| {
                changed |= ui
                    .toggle_value(&mut self.search.opts.regex, ".*")
                    .on_hover_text("Use regular expression")
                    .changed();
                changed |= ui
                    .toggle_value(&mut self.search.opts.whole_word, "ab|")
                    .on_hover_text("Match whole word")
                    .changed();
                changed |=
                    ui.toggle_value(&mut self.search.opts.case_sensitive, "Aa").on_hover_text("Match case").changed();
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.search.query)
                        .hint_text("Search")
                        .desired_width(ui.available_width() - 26.0),
                );
                if self.search.focus {
                    r.request_focus();
                    self.search.focus = false;
                }
                changed |= r.changed();
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    start = true;
                }
                let arrow = if self.search.show_replace { "▾" } else { "▸" };
                if ui.add(egui::Button::new(arrow).frame(false)).on_hover_text("Toggle replace").clicked() {
                    self.search.show_replace = !self.search.show_replace;
                }
            },
        );
        if self.search.show_replace {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 28.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    let can = !self.search.results.is_empty();
                    if ui.add_enabled(can, egui::Button::new("↔ All")).on_hover_text("Replace in all files").clicked()
                    {
                        replace_all = true;
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search.replacement)
                            .hint_text(if self.search.opts.regex { "Replace ($1 = group)" } else { "Replace" })
                            .desired_width(ui.available_width()),
                    );
                },
            );
        }
        if changed {
            self.search.edited_at = Some(Instant::now());
        }
        // Search as you type, shortly after typing stops.
        if self.search.edited_at.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
            self.search.edited_at = None;
            start = true;
        } else if self.search.edited_at.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if start {
            self.search.results.clear();
            self.search.job = None;
            self.search.error = None;
            if !self.search.query.is_empty() {
                match SearchJob::start(&root, &self.search.query, self.search.opts) {
                    Ok(job) => self.search.job = Some(job),
                    Err(e) => self.search.error = Some(e),
                }
            }
        }
        if let Some(job) = &mut self.search.job {
            job.poll(&mut self.search.results);
            if !job.done {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }
        if let Some(e) = &self.search.error {
            ui.label(RichText::new(e).small().color(p.error));
        }
        let searching = self.search.job.as_ref().is_some_and(|j| !j.done);
        let n: usize = self.search.results.iter().map(|h| h.matches.len().max(1)).sum();
        let files = {
            let mut v: Vec<&str> = self.search.results.iter().map(|h| h.rel.as_str()).collect();
            v.dedup();
            v.len()
        };
        if n > 0 || searching {
            ui.label(
                RichText::new(format!("{n} results in {files} files{}", if searching { " — searching…" } else { "" }))
                    .small()
                    .color(p.text_dim),
            );
        } else if self.search.job.is_some() {
            ui.label(RichText::new("No results").small().color(p.text_dim));
        }
        ui.add_space(4.0);
        let mut open: Option<(PathBuf, usize)> = None;
        let mut replace_file: Option<PathBuf> = None;
        let show_rep = self.search.show_replace && !self.search.results.is_empty();
        let replacement = self.search.replacement.clone();
        let font = FontId::monospace(12.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut last_file = "";
            for hit in &self.search.results {
                if hit.rel != last_file {
                    last_file = &hit.rel;
                    ui.add_space(4.0);
                    let (dir, name) = hit.rel.rsplit_once('/').unwrap_or(("", &hit.rel));
                    let (icon, color) = theme::file_icon(name);
                    let count: usize =
                        self.search.results.iter().filter(|h| h.rel == hit.rel).map(|h| h.matches.len().max(1)).sum();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon).color(color));
                        ui.label(RichText::new(name).strong());
                        ui.label(RichText::new(dir).small().color(p.text_dim));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if show_rep
                                && ui
                                    .add(egui::Button::new("↔").frame(false))
                                    .on_hover_text("Replace in this file")
                                    .clicked()
                            {
                                replace_file = Some(hit.path.clone());
                            }
                            ui.label(RichText::new(count.to_string()).small().color(p.text_faint));
                        });
                    });
                }
                // Line text with matches highlighted (and the replacement preview).
                let mut job = egui::text::LayoutJob::default();
                let fmt = |color: Color32, bg: Color32, strike: bool| egui::TextFormat {
                    font_id: font.clone(),
                    color,
                    background: bg,
                    strikethrough: if strike { Stroke::new(1.0_f32, color) } else { Stroke::NONE },
                    ..Default::default()
                };
                job.append(&format!("{:>5}  ", hit.line + 1), 0.0, fmt(p.text_faint, Color32::TRANSPARENT, false));
                let chars: Vec<char> = hit.text.chars().collect();
                let mut at = 0;
                for &(a, b) in &hit.matches {
                    job.append(
                        &chars[at..a].iter().collect::<String>(),
                        0.0,
                        fmt(p.text_dim, Color32::TRANSPARENT, false),
                    );
                    let m: String = chars[a..b].iter().collect();
                    if show_rep {
                        job.append(&m, 0.0, fmt(p.error, p.error.gamma_multiply(0.15), true));
                        job.append(&replacement, 0.0, fmt(p.success, p.success.gamma_multiply(0.15), false));
                    } else {
                        job.append(&m, 0.0, fmt(p.text, p.find_match.gamma_multiply(0.6), false));
                    }
                    at = b;
                }
                job.append(&chars[at..].iter().collect::<String>(), 0.0, fmt(p.text_dim, Color32::TRANSPARENT, false));
                job.wrap.max_rows = 1;
                job.wrap.max_width = ui.available_width();
                job.wrap.break_anywhere = true;
                let r = ui.add(egui::Label::new(job).sense(Sense::click()));
                if r.hovered() {
                    ui.painter().rect_filled(r.rect.expand(1.0), 3.0, p.bg_hover.gamma_multiply(0.5));
                }
                if r.clicked() {
                    open = Some((hit.path.clone(), hit.line));
                }
            }
        });
        if let Some((path, line)) = open {
            if self.open_file(&path) {
                if let Some(doc) = self.docs.get_mut(self.active) {
                    doc.goto_line(line);
                    if !self.search.opts.regex {
                        doc.find.query = self.search.query.clone();
                        doc.find.opts.case_sensitive = self.search.opts.case_sensitive;
                        doc.find.opts.whole_word = self.search.opts.whole_word;
                        doc.find.open = true;
                    }
                }
            }
        }
        if let Some(path) = replace_file {
            self.replace_in_files(Some(path));
        }
        if replace_all {
            let count = n;
            self.dialog = Some(Dialog::ConfirmReplace { files, count });
        }
    }

    /// Replace search matches in one file or in every file with results.
    /// Open files are edited in their buffers (undoable, left unsaved); other
    /// files are rewritten on disk.
    fn replace_in_files(&mut self, only: Option<PathBuf>) {
        let opts = self.search.opts;
        let re = match build_regex(&self.search.query, opts) {
            Ok(r) => r,
            Err(e) => return self.toast(e, NotifyLevel::Error),
        };
        let mut paths: Vec<PathBuf> = self.search.results.iter().map(|h| h.path.clone()).collect();
        paths.dedup();
        if let Some(o) = &only {
            paths.retain(|p| p == o);
        }
        let rep = self.search.replacement.clone();
        let (mut total, mut files) = (0, 0);
        for path in paths {
            let canon = path.canonicalize().unwrap_or(path.clone());
            let n = if let Some(doc) = self.docs.iter_mut().find(|d| d.path() == Some(canon.as_path())) {
                let (new, n) = replace_in_text(&doc.buffer.text(), &re, &rep, opts);
                if n > 0 {
                    let sel = doc.buffer.sel;
                    doc.buffer.set_text(&new);
                    doc.buffer.sel = sel;
                    doc.buffer.sel.clamp(doc.buffer.len_chars());
                }
                n
            } else {
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        let (new, n) = replace_in_text(&text, &re, &rep, opts);
                        if n > 0 {
                            if let Err(e) = std::fs::write(&path, new) {
                                self.toast(format!("{}: {e}", path.display()), NotifyLevel::Error);
                                continue;
                            }
                        }
                        n
                    }
                    Err(e) => {
                        self.toast(format!("{}: {e}", path.display()), NotifyLevel::Error);
                        continue;
                    }
                }
            };
            if n > 0 {
                total += n;
                files += 1;
            }
        }
        self.toast(format!("Replaced {total} occurrence(s) in {files} file(s)"), NotifyLevel::Info);
        // Refresh the results.
        self.search.edited_at = Some(Instant::now() - Duration::from_secs(1));
    }

    fn agents_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, p: &Palette) {
        Self::section_header(ui, p, "AI AGENTS");
        ui.label(
            RichText::new("Agents run as real CLIs in Zenith's terminal, inside your workspace.")
                .small()
                .color(p.text_dim),
        );
        ui.add_space(8.0);
        let mut launch = None;
        let mut install = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for a in &self.agents {
                let ok = self.installed.get(&a.id).copied().unwrap_or(false);
                egui::Frame::new()
                    .fill(p.bg_elevated)
                    .corner_radius(8)
                    .inner_margin(10)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&a.icon).size(18.0).color(p.accent));
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&a.name).strong());
                                ui.label(
                                    RichText::new(format!("$ {}", a.command)).monospace().small().color(p.text_dim),
                                );
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (dot, tip) =
                                    if ok { (p.success, "installed") } else { (p.text_faint, "not installed") };
                                ui.label(RichText::new("●").color(dot)).on_hover_text(tip);
                            });
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ok {
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new("▶ Start").color(Color32::WHITE))
                                            .fill(p.accent),
                                    )
                                    .clicked()
                                {
                                    launch = Some(a.id.clone());
                                }
                            } else if !a.install_hint.is_empty() {
                                if ui.button("⬇ Install").on_hover_text(&a.install_hint).clicked() {
                                    install = Some(a.install_hint.clone());
                                }
                                ui.label(RichText::new("not installed").small().color(p.text_faint));
                            }
                        });
                    });
                ui.add_space(6.0);
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("⟳ Re-check").clicked() {
                    self.installed = self.agents.iter().map(|a| (a.id.clone(), a.is_installed())).collect();
                }
                if ui.button("+ Add custom agent").on_hover_text("Add an [[agents]] entry in settings.toml").clicked() {
                    self.run(ctx, Cmd::OpenSettingsFile);
                }
            });
            ui.add_space(10.0);
            Self::section_header(ui, p, "QUICK ACTIONS");
            for (label, cmd) in [
                ("Ask about selection…", Cmd::AskAgent),
                ("Send selection to agent", Cmd::SendSelectionToAgent),
                ("Send current file to agent", Cmd::SendFileToAgent),
            ] {
                if ui.add(egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 26.0))).clicked() {
                    self.run(ctx, cmd);
                }
            }
        });
        if let Some(id) = launch {
            self.launch_agent(&id);
        }
        if let Some(cmd) = install {
            self.run_in_terminal(&cmd);
            self.toast("Installing in terminal — click Re-check when it finishes.", NotifyLevel::Info);
        }
    }

    fn plugins_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, p: &Palette) {
        Self::section_header(ui, p, "PLUGINS");
        ui.horizontal_wrapped(|ui| {
            if ui.button("⟳ Reload").clicked() {
                self.run(ctx, Cmd::ReloadPlugins);
            }
            if ui.button("🗁 Folder").clicked() {
                self.run(ctx, Cmd::OpenPluginsFolder);
            }
            if ui.button("+ Create sample").on_hover_text("Scaffold an example plugin you can edit").clicked() {
                match crate::sample_plugin::scaffold(&settings::plugins_dir()) {
                    Ok(dir) => {
                        self.load_plugins();
                        self.toast(format!("Sample plugin created in {}", dir.display()), NotifyLevel::Info);
                        self.open_file(&dir.join("main.py"));
                    }
                    Err(e) => self.toast(format!("Couldn't create sample: {e}"), NotifyLevel::Error),
                }
            }
        });
        ui.add_space(6.0);
        if self.plugins.plugins.is_empty() {
            ui.label(RichText::new("No plugins installed.\nPlugins are folders with a plugin.toml in the plugins folder (or .zenith/plugins in your workspace). Any language works.").color(p.text_dim));
        }
        let mut toggle = None;
        let mut exec = None;
        let mut install_server: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, pl) in self.plugins.plugins.iter_mut().enumerate() {
                let running = pl.is_running();
                egui::Frame::new()
                    .fill(p.bg_elevated)
                    .corner_radius(8)
                    .inner_margin(10)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&pl.manifest.name).strong());
                            ui.label(RichText::new(&pl.manifest.version).small().color(p.text_dim));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let mut en = pl.enabled;
                                if ui.checkbox(&mut en, "").on_hover_text("Enabled").changed() {
                                    toggle = Some((i, en));
                                }
                                if running {
                                    ui.label(RichText::new("●").color(p.success)).on_hover_text("running");
                                }
                            });
                        });
                        if !pl.manifest.description.is_empty() {
                            ui.label(RichText::new(&pl.manifest.description).small().color(p.text_dim));
                        }
                        for c in &pl.manifest.commands {
                            if ui
                                .add(egui::Button::new(RichText::new(format!("▸ {}", c.title)).small()).frame(false))
                                .clicked()
                            {
                                exec = Some(c.id.clone());
                            }
                        }
                    });
                ui.add_space(6.0);
            }
            ui.add_space(8.0);
            Self::section_header(ui, p, "LANGUAGE SERVERS");
            let running = self.lsp.running();
            for cfg in self.lsp.configs() {
                let (dot, tip) = if running.contains(&cfg.name) {
                    (p.success, "running")
                } else if cfg.is_installed() {
                    (p.accent, "installed — starts when you open a matching file")
                } else {
                    (p.text_faint, "not installed")
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(dot)).on_hover_text(tip);
                    ui.label(RichText::new(&cfg.name).strong());
                    ui.label(RichText::new(cfg.languages.join(", ")).small().color(p.text_dim));
                    if dot == p.text_faint && !cfg.install_hint.is_empty() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("⬇").on_hover_text(&cfg.install_hint).clicked() {
                                install_server = Some(cfg.install_hint.clone());
                            }
                        });
                    }
                });
            }
        });
        if let Some(cmd) = install_server {
            self.run_in_terminal(&cmd);
            self.toast("Installing in terminal — then run “Language Servers: Restart”.", NotifyLevel::Info);
        }
        if let Some((i, en)) = toggle {
            self.plugins.set_enabled(i, en);
        }
        if let Some(id) = exec {
            let ectx = self.editor_context();
            if let Err(e) = self.plugins.execute(&id, &ectx) {
                self.toast(format!("{e:#}"), NotifyLevel::Error);
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, right: bool) {
        let shown = self.pane_doc(right);
        let p = self.colors.clone();
        let mut activate = None;
        let mut close = None;
        let mut menu: Option<(TabMenu, usize)> = None;
        // Drag & drop reordering.
        let mut tab_rects: Vec<egui::Rect> = vec![];
        let mut dragging: Option<usize> = None;
        let mut dropped: Option<usize> = None;
        egui::ScrollArea::horizontal()
            .id_salt(("tabs", right))
            .drag_to_scroll(false)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (i, doc) in self.docs.iter().enumerate() {
                        let title = doc.title();
                        let dirty = doc.buffer.is_dirty();
                        let active = i == shown;
                        let font = FontId::proportional(13.0);
                        let tw = ui.fonts(|f| f.layout_no_wrap(title.clone(), font.clone(), p.text).size().x);
                        let w = tw + 64.0;
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 36.0), Sense::click_and_drag());
                        tab_rects.push(rect);
                        if resp.dragged() {
                            dragging = Some(i);
                        }
                        if resp.drag_stopped() {
                            dropped = Some(i);
                        }
                        let bg = if active {
                            p.bg
                        } else if resp.hovered() {
                            p.bg_hover
                        } else {
                            p.bg_panel
                        };
                        ui.painter().rect_filled(rect, 0.0, bg);
                        if active {
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(rect.min, egui::vec2(w, 2.0)),
                                0.0,
                                p.accent,
                            );
                        }
                        ui.painter().vline(rect.max.x - 0.5, rect.y_range(), Stroke::new(1.0_f32, p.border));
                        let (glyph, color) = theme::file_icon(&title);
                        ui.painter().text(
                            egui::pos2(rect.min.x + 16.0, rect.center().y),
                            Align2::CENTER_CENTER,
                            glyph,
                            FontId::proportional(11.0),
                            color,
                        );
                        ui.painter().text(
                            egui::pos2(rect.min.x + 28.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            &title,
                            font,
                            if active { p.text } else { p.text_dim },
                        );
                        let close_rect = egui::Rect::from_center_size(
                            egui::pos2(rect.max.x - 18.0, rect.center().y),
                            egui::vec2(18.0, 18.0),
                        );
                        let close_resp =
                            ui.interact(close_rect, ui.id().with(("tab-close", doc.id, right)), Sense::click());
                        if close_resp.hovered() {
                            ui.painter().rect_filled(close_rect, 4.0, p.bg_active);
                        }
                        if dirty && !close_resp.hovered() {
                            ui.painter().circle_filled(close_rect.center(), 4.0, p.text_dim);
                        } else if resp.hovered() || active || close_resp.hovered() {
                            ui.painter().text(
                                close_rect.center(),
                                Align2::CENTER_CENTER,
                                "✕",
                                FontId::proportional(11.0),
                                p.text_dim,
                            );
                        }
                        if close_resp.clicked() || resp.middle_clicked() {
                            close = Some(i);
                        } else if resp.clicked() {
                            activate = Some(i);
                        }
                        let has_path = doc.path().is_some();
                        resp.context_menu(|ui| {
                            for (label, action, enabled) in [
                                ("Close", TabMenu::Close, true),
                                ("Close Others", TabMenu::CloseOthers, true),
                                ("Close to the Right", TabMenu::CloseRight, true),
                                ("Close Saved", TabMenu::CloseSaved, true),
                                ("Copy Path", TabMenu::CopyPath, has_path),
                                ("Copy Relative Path", TabMenu::CopyRelPath, has_path),
                                ("Reveal in Explorer", TabMenu::Reveal, has_path),
                                ("Open in Other Pane", TabMenu::OtherPane, true),
                            ] {
                                if matches!(action, TabMenu::CopyPath | TabMenu::OtherPane) {
                                    ui.separator();
                                }
                                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                                    menu = Some((action, i));
                                    ui.close_menu();
                                }
                            }
                        });
                        let path = doc.path().map(|p| p.display().to_string());
                        if let Some(path) = path {
                            resp.on_hover_text(path);
                        }
                    }
                });
            });
        if let Some(i) = activate {
            self.focus_pane(right);
            self.active = i;
            self.focus_editor = true;
        }
        if let Some(i) = close {
            self.close_doc(i, false);
        }
        // Insertion marker while dragging, and the move on release.
        let pointer = ui.ctx().pointer_latest_pos();
        let target = |from: usize, x: f32| -> usize {
            let mut t = tab_rects.iter().enumerate().filter(|(j, r)| *j != from && r.center().x < x).count();
            t = t.min(tab_rects.len().saturating_sub(1));
            t
        };
        if let (Some(from), Some(pos)) = (dragging, pointer) {
            let t = target(from, pos.x);
            if t != from {
                let x = if t > from { tab_rects[t].max.x } else { tab_rects[t].min.x };
                let r = tab_rects[from];
                ui.painter().vline(x, r.y_range(), Stroke::new(2.0_f32, p.accent));
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if let (Some(from), Some(pos)) = (dropped, pointer) {
            let to = target(from, pos.x);
            if to != from {
                self.move_tab(from, to);
            }
        }
        if let Some((action, i)) = menu {
            self.tab_menu_action(ui.ctx(), action, i, right);
        }
    }

    /// Move a tab, keeping both panes on the documents they were showing.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.docs.len() || to >= self.docs.len() || from == to {
            return;
        }
        let doc = self.docs.remove(from);
        self.docs.insert(to, doc);
        let remap = |i: usize| -> usize {
            if i == from {
                to
            } else if from < i && i <= to {
                i - 1
            } else if to <= i && i < from {
                i + 1
            } else {
                i
            }
        };
        self.active = remap(self.active);
        self.other = remap(self.other);
    }

    fn tab_menu_action(&mut self, ctx: &egui::Context, action: TabMenu, i: usize, right: bool) {
        let Some(doc) = self.docs.get(i) else { return };
        let id = doc.id;
        let path = doc.path().map(Path::to_path_buf);
        // Bulk closes keep unsaved files open instead of prompting for each one.
        let mut close_ids = |pred: &dyn Fn(usize, &Document) -> bool| {
            let ids: Vec<u64> = self.docs.iter().enumerate().filter(|(j, d)| pred(*j, d)).map(|(_, d)| d.id).collect();
            let mut kept = 0;
            for id in ids {
                if let Some(j) = self.docs.iter().position(|d| d.id == id) {
                    if self.docs[j].buffer.is_dirty() {
                        kept += 1;
                    } else {
                        self.close_doc(j, true);
                    }
                }
            }
            if kept > 0 {
                self.toast(format!("{kept} file(s) with unsaved changes were kept open"), NotifyLevel::Info);
            }
            if let Some(j) = self.docs.iter().position(|d| d.id == id) {
                self.focus_pane(right);
                self.active = j;
            }
        };
        match action {
            TabMenu::Close => self.close_doc(i, false),
            TabMenu::CloseOthers => close_ids(&|j, _| j != i),
            TabMenu::CloseRight => close_ids(&|j, _| j > i),
            TabMenu::CloseSaved => close_ids(&|_, d| !d.buffer.is_dirty()),
            TabMenu::CopyPath => {
                if let Some(p) = path {
                    ctx.copy_text(p.display().to_string());
                }
            }
            TabMenu::CopyRelPath => {
                if let Some(p) = path {
                    let rel = self.rel_path(&p);
                    ctx.copy_text(rel);
                }
            }
            TabMenu::Reveal => {
                if let (Some(p), Some(t)) = (path, &mut self.tree) {
                    t.reveal(&p);
                    self.sidebar = SidebarView::Explorer;
                    self.show_sidebar = true;
                }
            }
            TabMenu::OtherPane => {
                if !self.split {
                    self.split = true;
                    self.focus_right = false;
                    self.other = i;
                } else if right == self.focus_right {
                    self.other = i;
                } else {
                    self.active = i;
                }
            }
        }
    }

    fn pane_doc(&self, right: bool) -> usize {
        if !self.split || right == self.focus_right {
            self.active
        } else {
            self.other
        }
    }

    /// Make a pane the focused one; `active` always refers to the focused pane.
    fn focus_pane(&mut self, right: bool) {
        if self.split && right != self.focus_right {
            std::mem::swap(&mut self.active, &mut self.other);
            self.focus_right = right;
        }
    }

    fn editor_pane(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, right: bool) {
        let p = self.colors.clone();
        let shown = self.pane_doc(right);
        egui::Frame::new().fill(p.bg_panel).show(ui, |ui| {
            ui.set_width(ui.available_width());
            self.tab_bar(ui, right);
        });
        // Breadcrumbs.
        let is_md = self.docs.get(shown).is_some_and(|d| d.language().starts_with("Markdown"));
        if let Some(path) = self.docs.get(shown).and_then(|d| d.path()).map(|p| self.rel_path(p)) {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                let crumbs = path.split('/').collect::<Vec<_>>().join("  ›  ");
                ui.label(RichText::new(crumbs).size(12.0).color(p.text_dim));
                if is_md {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(10.0);
                        let on = self.docs.get(shown).is_some_and(|d| d.md_preview);
                        let label = RichText::new(if on { "◧ Hide preview" } else { "◧ Preview" })
                            .size(12.0)
                            .color(if on { p.accent } else { p.text_dim });
                        if ui
                            .add(egui::Button::new(label).frame(false))
                            .on_hover_text("Markdown preview (Ctrl+Alt+P)")
                            .clicked()
                        {
                            if let Some(d) = self.docs.get_mut(shown) {
                                d.md_preview = !on;
                            }
                        }
                    });
                }
            });
        }
        let review = self.settings.review_external_changes;
        let mut conflict = false;
        if let Some(doc) = self.docs.get_mut(shown) {
            if doc.changed_on_disk() {
                if doc.buffer.is_dirty() {
                    conflict = true;
                } else {
                    // Clean buffer modified externally (e.g. by an AI agent): load it,
                    // keeping the old version for review.
                    let res = if review { doc.reload_with_review() } else { doc.reload_from_disk() };
                    if let Err(e) = res {
                        log::warn!("reload failed: {e}");
                    }
                }
            }
        }
        if let Some(doc) = self.docs.get_mut(shown) {
            let act = crate::review::banner(ui, doc, &p, conflict);
            if let Some(a) = act {
                if let Err(e) = crate::review::apply(doc, a) {
                    self.toast(format!("{e}"), NotifyLevel::Error);
                }
            }
        }
        if self.docs.get(shown).is_some_and(|d| d.show_git_diff) {
            let fs = self.settings.editor_font_size;
            if let Some(doc) = self.docs.get_mut(shown) {
                if let Some(crate::diff_view::DiffAction::Close) = crate::diff_view::ui(ui, doc, &p, fs) {
                    doc.show_git_diff = false;
                    self.focus_editor = true;
                }
            }
            return;
        }
        let showing_diff = self.docs.get(shown).is_some_and(|d| d.review.as_ref().is_some_and(|r| r.show));
        if showing_diff {
            let fs = self.settings.editor_font_size;
            if let Some(doc) = self.docs.get_mut(shown) {
                if let Some(a) = crate::review::diff_view(ui, doc, &p, fs) {
                    if let Err(e) = crate::review::apply(doc, a) {
                        self.toast(format!("{e}"), NotifyLevel::Error);
                    }
                }
            }
            return;
        }
        let focused_pane = !self.split || right == self.focus_right;
        let focus_editor = focused_pane && std::mem::take(&mut self.focus_editor);
        let epoch = self.cursor_epoch;
        let pane = right as usize;
        let actions = if let Some(doc) = self.docs.get_mut(shown) {
            let id = editor_id(ui, doc.id, pane);
            if focus_editor {
                ui.memory_mut(|m| m.request_focus(id));
            }
            let actions = if doc.md_preview && is_md {
                // Editor on the left, live rendered Markdown on the right.
                let full = ui.available_rect_before_wrap();
                let half = (full.width() / 2.0).floor();
                let left = egui::Rect::from_min_size(full.min, egui::vec2(half, full.height()));
                let right_r = egui::Rect::from_min_max(egui::pos2(full.min.x + half, full.min.y), full.max);
                let acts = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
                        editor_ui(ui, doc, &p, &self.settings, epoch, pane)
                    })
                    .inner;
                ui.painter().vline(right_r.min.x, right_r.y_range(), Stroke::new(1.0_f32, p.border));
                ui.scope_builder(egui::UiBuilder::new().max_rect(right_r.shrink2(egui::vec2(1.0, 0.0))), |ui| {
                    egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(24, 12)).show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt(("md-preview", doc.id, pane))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_max_width(760.0);
                                let text = doc.buffer.text();
                                // Relative image paths resolve next to the Markdown file.
                                let base = doc
                                    .path()
                                    .and_then(|p| p.parent())
                                    .map(|d| format!("file://{}/", d.display()))
                                    .unwrap_or_default();
                                egui_commonmark::CommonMarkViewer::new()
                                    .default_implicit_uri_scheme(&base)
                                    .max_image_width(Some(ui.available_width() as usize))
                                    .show(ui, &mut self.md_cache, &text);
                            });
                    });
                });
                ui.allocate_rect(full, Sense::hover());
                acts
            } else {
                editor_ui(ui, doc, &p, &self.settings, epoch, pane)
            };
            if !focused_pane && ui.memory(|m| m.has_focus(id)) {
                self.focus_pane(right);
            }
            actions
        } else {
            vec![]
        };
        for a in actions {
            match a {
                EditorAction::AskAgent { question } => {
                    let prompt = self.selection_prompt(&question);
                    self.send_to_agent(prompt, true, None);
                }
                EditorAction::SendSelectionToAgent => self.run(ctx, Cmd::SendSelectionToAgent),
                EditorAction::Complete { trigger } => {
                    let root = self.workspace.clone();
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        self.lsp.request_completion(doc, root.as_deref(), trigger);
                    }
                }
                EditorAction::Hover { idx } => {
                    let root = self.workspace.clone();
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        self.lsp.request_hover(doc, root.as_deref(), idx);
                    }
                }
                EditorAction::GotoDefinition { idx } => self.goto_definition(idx),
                EditorAction::Run(cmd) => self.run(ctx, cmd),
                EditorAction::SignatureHelp => {
                    let root = self.workspace.clone();
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        self.lsp.request_signature_help(doc, root.as_deref());
                    }
                }
            }
        }
    }

    fn welcome(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let p = self.colors.clone();
        let mut cmd = None;
        let mut recent = None;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.16).max(20.0));
            ui.label(RichText::new("◆").size(56.0).color(p.accent));
            ui.label(RichText::new("Zenith").size(40.0).strong().family(egui::FontFamily::Name(theme::UI_BOLD.into())));
            ui.label(RichText::new("Fast. Extensible. AI-native.").size(15.0).color(p.text_dim));
            ui.add_space(28.0);
            let w = 340.0;
            for (label, c) in [
                ("🗁   Open Folder", Cmd::OpenFolder),
                ("🗋   New File", Cmd::NewFile),
                ("🔍   Quick Open", Cmd::QuickOpen),
                ("⌘   Command Palette", Cmd::CommandPalette),
            ] {
                let b = egui::Button::new(RichText::new(label).size(14.0))
                    .shortcut_text(c.shortcut())
                    .min_size(egui::vec2(w, 34.0))
                    .fill(p.bg_elevated);
                if ui.add(b).clicked() {
                    cmd = Some(c);
                }
            }
            ui.add_space(14.0);
            let claude_ok = self.installed.get("claude").copied().unwrap_or(false);
            let b = egui::Button::new(RichText::new("✳   Start Claude Code").size(14.0).color(Color32::WHITE))
                .min_size(egui::vec2(w, 36.0))
                .fill(p.accent);
            if ui
                .add(b)
                .on_hover_text(if claude_ok {
                    "Runs `claude` in this workspace"
                } else {
                    "Claude Code CLI not found — see the AI Agents panel"
                })
                .clicked()
            {
                self.launch_agent("claude");
            }
            if !self.session.recent_workspaces.is_empty() {
                ui.add_space(26.0);
                ui.label(RichText::new("RECENT").size(11.0).color(p.text_dim));
                ui.add_space(4.0);
                for r in self.session.recent_workspaces.iter().take(6) {
                    let name = r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let resp = ui.add(
                        egui::Label::new(RichText::new(format!("{name}   {}", r.display())).color(p.accent))
                            .sense(Sense::click()),
                    );
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        recent = Some(r.clone());
                    }
                }
            }
        });
        if let Some(c) = cmd {
            self.run(ctx, c);
        }
        if let Some(r) = recent {
            self.open_workspace(r);
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let p = self.colors.clone();
        let mut cmd = None;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let small = |t: String| RichText::new(t).size(12.0).color(p.text_dim);
            if let Some(b) = self.git.as_ref().and_then(|g| g.branch.as_ref()) {
                let r = ui.add(egui::Label::new(small(format!("⎇ {b}"))).sense(Sense::click()));
                if r.on_hover_text("Switch branch").clicked() {
                    cmd = Some(Cmd::GitSwitchBranch);
                }
            }
            if self.index.indexing {
                ui.label(small("indexing…".into()));
            }
            if let Some(doc) = self.docs.get(self.active) {
                if doc.lsp_active {
                    let (e, w) = doc.diagnostic_counts();
                    let t = RichText::new(format!("⊗ {e}  ⚠ {w}")).size(12.0).color(if e > 0 {
                        p.error
                    } else if w > 0 {
                        p.warning
                    } else {
                        p.text_dim
                    });
                    if ui.add(egui::Label::new(t).sense(Sense::click())).on_hover_text("Next problem (F8)").clicked() {
                        cmd = Some(Cmd::NextProblem);
                    }
                }
            }
            for (server, msg) in &self.lsp.progress {
                let m: String = msg.chars().take(48).collect();
                ui.label(small(format!("⟳ {server}: {m}")));
            }
            let running_agents = self.ai.tabs.iter().filter(|t| t.is_alive()).count();
            if running_agents > 0 {
                ui.label(RichText::new(format!("✳ {running_agents} agent(s)")).size(12.0).color(p.accent));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(doc) = self.docs.get(self.active) {
                    let pos = doc.buffer.cursor_pos();
                    let lang = doc.language().to_string();
                    if ui.add(egui::Label::new(small(lang)).sense(Sense::click())).clicked() {
                        cmd = Some(Cmd::SelectLanguage);
                    }
                    ui.label(small(doc.buffer.line_ending.label().into()));
                    ui.label(small("UTF-8".into()));
                    let indent =
                        if doc.buffer.use_spaces { format!("Spaces: {}", doc.buffer.tab_size) } else { "Tabs".into() };
                    ui.label(small(indent));
                    if doc.buffer.has_multi() {
                        ui.label(
                            RichText::new(format!("{} cursors", doc.buffer.extra.len() + 1)).size(12.0).color(p.accent),
                        );
                    }
                    let sel_len = doc.buffer.sel.range().len();
                    let sel = if sel_len > 0 { format!(" ({sel_len} selected)") } else { String::new() };
                    if ui
                        .add(
                            egui::Label::new(small(format!("Ln {}, Col {}{sel}", pos.line + 1, pos.col + 1)))
                                .sense(Sense::click()),
                        )
                        .clicked()
                    {
                        cmd = Some(Cmd::GotoLine);
                    }
                }
            });
        });
        if let Some(c) = cmd {
            self.run(ctx, c);
        }
    }

    /// Bottom terminal panel or right AI panel.
    fn term_panel_ui(&mut self, ui: &mut egui::Ui, ai: bool) {
        let p = self.colors.clone();
        let font_size = self.settings.terminal_font_size;
        let waker = self.waker.clone();
        let agents = self.agents.clone();
        let mut new_shell = false;
        let mut launch = None;
        let mut open_link = None;
        let mut hide = false;
        let focus_req = self.focus_terminal;
        let panel = if ai { &mut self.ai } else { &mut self.bottom };

        let mut close = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.label(RichText::new(if ai { "AI AGENTS" } else { "TERMINAL" }).size(11.0).strong().color(p.text_dim));
            ui.add_space(8.0);
            for (i, t) in panel.tabs.iter().enumerate() {
                let active = i == panel.active;
                let mut title = t.title();
                if !t.is_alive() {
                    title.push_str(" (exited)");
                }
                let txt = RichText::new(title).size(12.0).color(if active { p.text } else { p.text_dim });
                let b = egui::Button::new(txt)
                    .fill(if active { p.bg_active } else { Color32::TRANSPARENT })
                    .corner_radius(5);
                let r = ui.add(b);
                if r.clicked() {
                    panel.active = i;
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new("✕").frame(false)).on_hover_text("Hide panel").clicked() {
                    hide = true;
                }
                if !panel.tabs.is_empty()
                    && ui.add(egui::Button::new("🗑").frame(false)).on_hover_text("Kill active session").clicked()
                {
                    close = Some(panel.active);
                }
                if ai {
                    ui.menu_button("+ Agent ▾", |ui| {
                        for a in &agents {
                            if ui.button(format!("{}  {}", a.icon, a.name)).clicked() {
                                launch = Some(a.id.clone());
                                ui.close_menu();
                            }
                        }
                    });
                } else if ui.add(egui::Button::new("+").frame(false)).on_hover_text("New terminal").clicked() {
                    new_shell = true;
                }
            });
        });
        ui.add_space(2.0);
        if let Some(i) = close {
            panel.tabs.remove(i);
            if panel.active >= panel.tabs.len() {
                panel.active = panel.tabs.len().saturating_sub(1);
            }
        }
        let mut focused = false;
        if let Some(tab) = panel.active_mut() {
            let want_focus = focus_req == Some(tab.id);
            match terminal_ui(ui, tab, &p, font_size, want_focus) {
                Some(TermEvent::Restart) => tab.restart(waker),
                Some(TermEvent::OpenLink(link)) => open_link = Some((link, tab.spec.cwd.clone())),
                None => {}
            }
            focused = ui.memory(|m| m.has_focus(ui.make_persistent_id(("zenith-term", tab.id))));
            if want_focus {
                self.focus_terminal = None;
            }
        } else {
            ui.centered_and_justified(|ui| {
                if ai {
                    ui.label(
                        RichText::new("Start an AI agent with “+ Agent” — Claude Code, Codex, Gemini, Aider…")
                            .color(p.text_dim),
                    );
                } else {
                    ui.label(RichText::new("No terminal. Click + to open one.").color(p.text_dim));
                }
            });
        }
        self.terminal_focused |= focused;
        if hide {
            if ai {
                self.show_ai = false;
            } else {
                self.show_terminal = false;
            }
        }
        if new_shell {
            self.new_terminal(None);
        }
        if let Some(id) = launch {
            self.launch_agent(&id);
        }
        if let Some((link, cwd)) = open_link {
            self.open_terminal_link(link, cwd);
        }
    }

    /// Open a file path or URL clicked in a terminal.
    fn open_terminal_link(&mut self, link: zenith_terminal::Link, cwd: Option<PathBuf>) {
        match link {
            zenith_terminal::Link::Url(u) => open_url(&u),
            zenith_terminal::Link::File { path, line, col } => {
                let p = PathBuf::from(&path);
                let candidates = if p.is_absolute() {
                    vec![p]
                } else {
                    [cwd, self.workspace.clone()].into_iter().flatten().map(|d| d.join(&p)).collect()
                };
                let Some(found) = candidates.into_iter().find(|c| c.is_file()) else {
                    self.toast(format!("File not found: {path}"), NotifyLevel::Warning);
                    return;
                };
                if self.open_file(&found) {
                    if let (Some(l), Some(doc)) = (line, self.docs.get_mut(self.active)) {
                        let pos = zenith_core::Position::new(l.saturating_sub(1), col.unwrap_or(1).saturating_sub(1));
                        let idx = doc.buffer.pos_to_char(pos);
                        doc.buffer.clear_extra();
                        doc.buffer.set_cursor(idx, false);
                        doc.reveal_line = Some(pos.line);
                    }
                    self.focus_editor = true;
                }
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let mut next: Option<Dialog> = None;
        let window = |title: &str| {
            egui::Window::new(title.to_string())
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_TOP, [0.0, 110.0])
                .min_width(380.0)
        };
        match dialog {
            Dialog::Input { title, mut value, kind, focus } => {
                let mut submit = false;
                window(&title).show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut value).desired_width(f32::INFINITY));
                    if focus {
                        r.request_focus();
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                    });
                });
                if submit {
                    let name = value.trim();
                    if !name.is_empty() {
                        self.apply_input(kind, name.to_string());
                    }
                } else if keep {
                    next = Some(Dialog::Input { title, value, kind, focus: false });
                }
            }
            Dialog::ConfirmDelete(path) => {
                window("Delete").show(ctx, |ui| {
                    ui.label(format!(
                        "Delete “{}”? This cannot be undone.",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Delete").color(Color32::WHITE))
                                    .fill(self.colors.error),
                            )
                            .clicked()
                        {
                            let res = if path.is_dir() {
                                std::fs::remove_dir_all(&path)
                            } else {
                                std::fs::remove_file(&path)
                            };
                            match res {
                                Ok(()) => {
                                    self.docs.retain(|d| d.path().is_none_or(|p| !p.starts_with(&path)));
                                    self.active = self.active.min(self.docs.len().saturating_sub(1));
                                    if let Some(t) = &mut self.tree {
                                        t.refresh();
                                    }
                                }
                                Err(e) => self.toast(format!("Delete failed: {e}"), NotifyLevel::Error),
                            }
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                });
                if keep {
                    next = Some(Dialog::ConfirmDelete(path));
                }
            }
            Dialog::UnsavedClose(id) => {
                let Some(idx) = self.docs.iter().position(|d| d.id == id) else { return };
                let title = self.docs[idx].title();
                window("Unsaved changes").show(ctx, |ui| {
                    ui.label(format!("Do you want to save the changes you made to {title}?"));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(self.colors.accent),
                            )
                            .clicked()
                        {
                            if self.save_doc(idx, false) {
                                self.close_doc(idx, true);
                            }
                            keep = false;
                        }
                        if ui.button("Don't Save").clicked() {
                            self.close_doc(idx, true);
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                });
                if keep {
                    next = Some(Dialog::UnsavedClose(id));
                }
            }
            Dialog::ConfirmReplace { files, count } => {
                let mut go = false;
                window("Replace All").show(ctx, |ui| {
                    ui.label(format!(
                        "Replace {count} occurrence(s) across {files} file(s) with “{}”?",
                        self.search.replacement
                    ));
                    ui.label(
                        RichText::new("Open files are changed in the editor (undoable); others are written to disk.")
                            .small()
                            .color(self.colors.text_dim),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Replace").color(Color32::WHITE))
                                    .fill(self.colors.accent),
                            )
                            .clicked()
                        {
                            go = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                    });
                });
                if go {
                    self.replace_in_files(None);
                } else if keep {
                    next = Some(Dialog::ConfirmReplace { files, count });
                }
            }
            Dialog::UnsavedQuit => {
                let n = self.docs.iter().filter(|d| d.buffer.is_dirty()).count();
                window("Quit Zenith").show(ctx, |ui| {
                    ui.label(format!("{n} file(s) have unsaved changes."));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Save All & Quit").color(Color32::WHITE))
                                    .fill(self.colors.accent),
                            )
                            .clicked()
                        {
                            let mut ok = true;
                            for i in 0..self.docs.len() {
                                if self.docs[i].buffer.is_dirty() {
                                    ok &= self.save_doc(i, false);
                                }
                            }
                            if ok {
                                self.allow_close = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            keep = false;
                        }
                        if ui.button("Discard & Quit").clicked() {
                            self.allow_close = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                });
                if keep {
                    next = Some(Dialog::UnsavedQuit);
                }
            }
            Dialog::AskAgent { mut question, focus } => {
                let mut submit = false;
                let has_sel = self.docs.get(self.active).is_some_and(|d| !d.buffer.sel.is_empty());
                let agent_name = self
                    .agents
                    .iter()
                    .find(|a| a.id == self.settings.default_agent)
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                window(&format!("✳ Ask {agent_name}")).show(ctx, |ui| {
                    ui.label(
                        RichText::new(if has_sel {
                            "The selected code and file path will be included."
                        } else {
                            "The current file path will be included."
                        })
                        .small()
                        .color(self.colors.text_dim),
                    );
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut question)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .hint_text("e.g. Why does this panic on empty input?"),
                    );
                    if focus {
                        r.request_focus();
                    }
                    let enter = r.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
                    if enter {
                        question = question.trim_end_matches('\n').to_string();
                        submit = true;
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new("Ask").color(Color32::WHITE)).fill(self.colors.accent))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                        ui.label(
                            RichText::new("Enter to send · Shift+Enter for newline")
                                .small()
                                .color(self.colors.text_faint),
                        );
                    });
                });
                if submit && !question.trim().is_empty() {
                    let prompt = self.selection_prompt(question.trim());
                    self.send_to_agent(prompt, true, None);
                } else if keep {
                    next = Some(Dialog::AskAgent { question, focus: false });
                }
            }
            Dialog::About => {
                window("About Zenith").show(ctx, |ui| {
                    ui.label(RichText::new("◆ Zenith").size(22.0).color(self.colors.accent));
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label("A fast, extensible, AI-native code editor written in Rust.");
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!("Config: {}", settings::config_dir().display()))
                            .small()
                            .color(self.colors.text_dim),
                    );
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        keep = false;
                    }
                });
                if keep {
                    next = Some(Dialog::About);
                }
            }
        }
        if self.dialog.is_none() {
            self.dialog = next;
        }
    }

    fn apply_input(&mut self, kind: InputKind, name: String) {
        let result = match &kind {
            InputKind::NewFile(dir) => {
                let path = dir.join(&name);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map(|_| {
                    self.open_file(&path);
                })
            }
            InputKind::NewFolder(dir) => std::fs::create_dir_all(dir.join(&name)),
            InputKind::NewBranch => {
                self.git_switch(&name, true);
                return;
            }
            InputKind::RenameSymbol(idx) => {
                let root = self.workspace.clone();
                if let Some(doc) = self.docs.get_mut(self.active) {
                    self.pending_rename = self.lsp.request_rename(doc, root.as_deref(), *idx, &name);
                }
                return;
            }
            InputKind::Rename(path) => {
                let target = path.with_file_name(&name);
                std::fs::rename(path, &target).map(|_| {
                    for d in &mut self.docs {
                        if d.path() == Some(path.as_path()) {
                            d.buffer.set_path(target.clone());
                        }
                    }
                })
            }
        };
        if let Err(e) = result {
            self.toast(format!("{e}"), NotifyLevel::Error);
        }
        if let Some(t) = &mut self.tree {
            t.refresh();
        }
        if let Some(ws) = &self.workspace {
            self.index = FileIndex::build(ws);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let p = self.colors.clone();
        let c = crate::settings_ui::window(
            ctx,
            &mut self.show_settings,
            &mut self.settings_section,
            &mut self.settings,
            &self.agents,
            &p,
        );
        let mut c = c;
        if c.style {
            let was_dark = self.colors.dark;
            self.colors = Palette::by_name(&self.settings.theme);
            self.style_dirty = true;
            // Keep the default syntax themes paired with the UI theme.
            if was_dark != self.colors.dark
                && ["OneHalfDark", "OneHalfLight"].contains(&self.settings.syntax_theme.as_str())
            {
                self.settings.syntax_theme = if self.colors.dark { "OneHalfDark" } else { "OneHalfLight" }.into();
                c.syntax_theme = true;
            }
        }
        if c.syntax_theme {
            let name = self.settings.syntax_theme.clone();
            self.set_syntax_theme(name);
        }
        if c.lsp {
            self.lsp.shutdown();
            self.lsp = LspManager::new(&self.settings.language_servers, self.settings.lsp_enabled, self.waker.clone());
            self.lsp.restart(&mut self.docs);
        }
        if c.any {
            self.write_settings();
        }
        if c.open_file {
            self.show_settings = false;
            self.run(ctx, Cmd::OpenSettingsFile);
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        let p = &self.colors;
        egui::Area::new(egui::Id::new("zenith-toasts"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -36.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                for t in &self.toasts {
                    let color = match t.level {
                        NotifyLevel::Info => p.accent,
                        NotifyLevel::Warning => p.warning,
                        NotifyLevel::Error => p.error,
                    };
                    egui::Frame::popup(ui.style())
                        .fill(p.bg_elevated)
                        .stroke(Stroke::new(1.0_f32, p.border))
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("●").color(color));
                                ui.add(egui::Label::new(&t.text).wrap());
                            });
                        });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn open_in_os(path: &Path) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

impl eframe::App for ZenithApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.style_dirty {
            theme::apply_style(ctx, &self.colors, self.settings.ui_font_size);
            self.style_dirty = false;
        }
        self.index.poll();
        self.handle_plugin_events();
        self.flush_pending_sends();
        self.retry_failed_resumes();
        self.handle_lsp();
        self.handle_git();

        // Keystrokes reset the caret blink so it stays visible while typing.
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)))) {
            self.cursor_epoch = ctx.input(|i| i.time);
        }

        // Global shortcuts (skipped while a modal dialog/palette has focus).
        if self.dialog.is_none() && !self.palette.open {
            for c in self.shortcuts(ctx) {
                self.run(ctx, c);
            }
        } else if self.palette.open {
            ctx.input_mut(|i| {
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::P))
            });
        }

        // Window close: confirm unsaved changes.
        if ctx.input(|i| i.viewport().close_requested()) {
            if !self.allow_close && self.docs.iter().any(|d| d.buffer.is_dirty()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.dialog = Some(Dialog::UnsavedQuit);
            } else {
                self.save_session();
                self.plugins.shutdown();
                self.lsp.shutdown();
            }
        }

        let p = self.colors.clone();
        egui::TopBottomPanel::top("menu")
            .frame(egui::Frame::new().fill(p.bg_sidebar).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ctx, |ui| self.menu_bar(ctx, ui));
        egui::TopBottomPanel::bottom("status")
            .exact_height(24.0)
            .frame(
                egui::Frame::new()
                    .fill(p.bg_sidebar)
                    .inner_margin(egui::Margin::symmetric(10, 0))
                    .stroke(Stroke::new(1.0_f32, p.border)),
            )
            .show(ctx, |ui| self.status_bar(ctx, ui));
        egui::SidePanel::left("activity")
            .exact_width(46.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.bg_sidebar))
            .show(ctx, |ui| self.activity_bar(ui));
        if self.show_sidebar {
            egui::SidePanel::left("sidebar")
                .default_width(self.session.sidebar_width.unwrap_or(260.0))
                .width_range(160.0..=600.0)
                .frame(
                    egui::Frame::new()
                        .fill(p.bg_sidebar)
                        .inner_margin(egui::Margin { left: 10, right: 8, top: 4, bottom: 4 })
                        .stroke(Stroke::new(1.0_f32, p.border)),
                )
                .show(ctx, |ui| self.sidebar_ui(ctx, ui));
        }
        self.terminal_focused = false;
        if self.show_ai {
            egui::SidePanel::right("ai-panel")
                .default_width(460.0)
                .width_range(280.0..=1200.0)
                .frame(
                    egui::Frame::new()
                        .fill(p.bg_panel)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .stroke(Stroke::new(1.0_f32, p.border)),
                )
                .show(ctx, |ui| self.term_panel_ui(ui, true));
        }
        if self.show_terminal {
            egui::TopBottomPanel::bottom("terminal")
                .default_height(260.0)
                .height_range(100.0..=900.0)
                .resizable(true)
                .frame(
                    egui::Frame::new()
                        .fill(p.bg_panel)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .stroke(Stroke::new(1.0_f32, p.border)),
                )
                .show(ctx, |ui| self.term_panel_ui(ui, false));
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg)).show(ctx, |ui| {
            if self.docs.is_empty() {
                self.welcome(ctx, ui);
                return;
            }
            if self.split {
                let w = ui.available_width();
                let h = ui.available_height();
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for right in [false, true] {
                        let pane_w = if right { ui.available_width() } else { (w / 2.0).floor() };
                        ui.allocate_ui_with_layout(egui::vec2(pane_w, h), Layout::top_down(Align::Min), |ui| {
                            ui.set_min_size(egui::vec2(pane_w, h));
                            self.editor_pane(ctx, ui, right);
                        });
                        if !right {
                            let x = ui.cursor().min.x;
                            ui.painter().vline(x, ui.max_rect().y_range(), Stroke::new(1.0_f32, p.border));
                        }
                    }
                });
            } else {
                self.editor_pane(ctx, ui, false);
            }
        });

        if let Some(action) = self.palette.ui(ctx, &p) {
            self.handle_palette(ctx, action);
        }
        self.dialogs(ctx);
        self.settings_window(ctx);
        self.toasts_ui(ctx);

        // Agents may edit files on disk; poll for external changes now and then.
        ctx.request_repaint_after(Duration::from_secs(2));

        if self.last_session_save.elapsed() > Duration::from_secs(5) {
            if self.settings.auto_save {
                for i in 0..self.docs.len() {
                    if self.docs[i].buffer.is_dirty() && self.docs[i].path().is_some() {
                        self.save_doc(i, false);
                    }
                }
            }
            self.save_session();
        }
    }
}
