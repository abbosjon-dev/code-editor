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
    Debug,
    Tests,
    /// A plugin's panel (index into `PluginHost::panels()`).
    PluginPanel(usize),
}

#[derive(Default)]
struct WsSymbols {
    query: String,
    changed: Option<Instant>,
    pending: Vec<u64>,
    results: Vec<zenith_lsp::Symbol>,
}

/// A plugin's status bar item.
struct StatusItem {
    text: String,
    tooltip: String,
    command: Option<String>,
    right: bool,
    color: String,
}

/// Contents of a plugin panel (from its last `set_panel`).
#[derive(Default)]
struct PanelData {
    items: Vec<zenith_plugin::PanelItem>,
    message: String,
    buttons: Vec<zenith_plugin::PanelButton>,
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
    Watch,
    /// `user@host:/path` of a folder to open over SSH.
    RemoteFolder,
    /// Edit an option of the breakpoint at (document id, line).
    Breakpoint(u64, usize, crate::editor_view::BpField),
}

enum Dialog {
    Input {
        title: String,
        value: String,
        kind: InputKind,
        focus: bool,
    },
    ConfirmDelete(PathBuf),
    UnsavedClose(u64),
    UnsavedQuit,
    ConfirmReplace {
        files: usize,
        count: usize,
    },
    AskAgent {
        question: String,
        focus: bool,
        caret: usize,
        pick: usize,
    },
    About,
    /// A newer release: install it?
    Update(crate::update::Release),
    /// The update is installed: restart now?
    Restart(String),
}

#[derive(Default)]
struct TermPanel {
    tabs: Vec<TermTab>,
    active: usize,
    /// Tab shown on the right half (split view).
    split: Option<u64>,
}

impl TermPanel {
    /// Index of the split tab, if it exists and isn't the active one.
    fn split_index(&self) -> Option<usize> {
        let id = self.split?;
        self.tabs.iter().position(|t| t.id == id).filter(|&i| i != self.active)
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
    /// Additional workspace folders (multi-root), each with its own tree and index.
    extra_trees: Vec<FileTree>,
    extra_index: Vec<FileIndex>,
    index: FileIndex,
    git: Option<Git>,
    /// Repositories of the added workspace folders (None = not a repo / same as another).
    extra_gits: Vec<Option<Git>>,
    /// Last `@` mention query and its matches (Ask AI dialog).
    mention_cache: Option<(String, Vec<crate::mention::Candidate>)>,
    /// Plugin panel contents keyed by `plugin/panel`.
    panel_data: HashMap<String, PanelData>,
    /// keybindings.toml.
    user_keys: Vec<crate::keymap::UserBinding>,
    /// Files changed since the first agent started (multi-file review).
    changes: crate::changeset::ChangeSet,
    show_changes: bool,
    debugger: crate::debug::Debugger,
    /// Launch configurations shown in Run & Debug (cached).
    debug_configs: Option<(Vec<zenith_dap::LaunchConfig>, Option<String>)>,
    /// Update being installed; the executable to start when Zenith exits.
    install_rx: Option<std::sync::mpsc::Receiver<Result<crate::update::Outcome, String>>>,
    restart_exe: Option<PathBuf>,
    pending_update_version: String,
    /// Open remote folder (SSH), and a connection in progress.
    remote: Option<crate::remote::Session>,
    remote_connecting: Option<crate::remote::Connecting>,
    /// Settings Sync operation in flight.
    sync_rx: Option<std::sync::mpsc::Receiver<crate::sync_ui::SyncResult>>,
    /// Test explorer.
    tests: crate::testing::Panel,
    market: crate::marketplace::Market,
    /// settings.toml as written (without the workspace's overrides).
    user_settings: Settings,
    /// Keys the workspace's .zenith/settings.toml overrides.
    ws_overrides: Vec<String>,
    /// MCP servers section (workspace `.mcp.json`).
    mcp: crate::mcp_ui::McpPanel,
    /// Update check in flight (manual = report every result), and a newer release.
    update_rx: Option<(std::sync::mpsc::Receiver<crate::update::CheckResult>, bool)>,
    update_available: Option<crate::update::Release>,
    /// Plugin status bar items keyed by (plugin, item id).
    status_items: std::collections::BTreeMap<(String, String), StatusItem>,
    /// Active document path last announced to plugins.
    announced_editor: Option<PathBuf>,
    /// Live "Go to Symbol in Workspace" state.
    ws_symbols: WsSymbols,
    /// Breakpoints of files that aren't open.
    bp_store: std::collections::BTreeMap<PathBuf, crate::debug::Breakpoints>,
    changes_refreshed: Option<Instant>,
    /// Repository shown in Source Control (0 = main folder).
    scm_repo: usize,
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
        crate::highlight::TREE_SITTER.store(settings.tree_sitter, std::sync::atomic::Ordering::Relaxed);
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
            settings: settings.clone(),
            colors,
            session,
            workspace: None,
            tree: None,
            extra_trees: vec![],
            extra_index: vec![],
            index: FileIndex::empty(),
            git: None,
            extra_gits: vec![],
            mention_cache: None,
            panel_data: HashMap::new(),
            user_keys: vec![],
            changes: Default::default(),
            show_changes: false,
            debugger: crate::debug::Debugger::new(waker.clone()),
            debug_configs: None,
            bp_store: Default::default(),
            ws_symbols: Default::default(),
            status_items: Default::default(),
            update_rx: None,
            update_available: None,
            mcp: Default::default(),
            tests: Default::default(),
            market: Default::default(),
            user_settings: settings.clone(),
            ws_overrides: vec![],
            sync_rx: None,
            remote: None,
            remote_connecting: None,
            install_rx: None,
            restart_exe: None,
            pending_update_version: String::new(),
            announced_editor: None,
            changes_refreshed: None,
            scm_repo: 0,
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
        app.load_keymap();
        if app.settings.sync_on_startup && crate::sync::is_set_up(&settings::config_dir()) {
            app.start_sync(crate::sync_ui::SyncOp::Pull);
        }
        // Daily update check (notify only).
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if app.settings.check_for_updates && now.saturating_sub(app.session.last_update_check) > 24 * 3600 {
            app.start_update_check(false);
        }
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
                    for r in saved.extra_roots.iter().filter(|r| r.is_dir()) {
                        app.add_root(r.clone());
                    }
                    for f in &saved.open_files {
                        if f.is_file() {
                            app.open_file(f);
                        }
                    }
                    app.active = saved.active.min(app.docs.len().saturating_sub(1));
                    // Reattach the remote folder this workspace mirrors (fetches what changed
                    // meanwhile) before terminals are restored, so they open on the host.
                    if let (Some(spec), Some(ws)) = (saved.remote.clone(), app.workspace.clone()) {
                        if crate::remote::cache_dir(&settings::config_dir(), &spec) == ws {
                            if let Some(g) = Git::open_remote(&ws, &spec) {
                                app.git = Some(g);
                            }
                            let mut s = crate::remote::Session::new(spec, ws, saved.remote_since, app.waker.clone());
                            s.pull_now();
                            app.remote = Some(s);
                            app.probe_remote();
                        }
                    }
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
        // A different folder ends the remote session (re-set by the caller when connecting).
        if self.remote.as_ref().is_some_and(|r| r.cache != path) {
            self.remote = None;
        }
        if self.remote.is_none() {
            self.lsp.remote = None;
            self.lsp.remote_pending = None;
            self.debugger.remote = None;
            self.tests.set_exec(crate::testing::Exec::default());
        }
        self.index = FileIndex::build(&path);
        self.git = Git::open(&path);
        self.problems.clear();
        self.tree = Some(FileTree::new(path.clone()));
        self.extra_trees.clear();
        self.extra_index.clear();
        self.extra_gits.clear();
        self.mcp.reset();
        self.scm_repo = 0;
        self.session.recent_workspaces.retain(|p| p != &path);
        // Remote copies are reopened with Remote: Open Folder over SSH, not from here.
        if !path.starts_with(settings::config_dir().join("remote")) {
            self.session.recent_workspaces.insert(0, path.clone());
        }
        self.session.recent_workspaces.truncate(10);
        self.workspace = Some(path);
        self.reload_settings_with(false);
        self.lsp.restart(&mut self.docs);
        self.load_plugins();
        self.save_session();
    }

    fn refresh_trees(&mut self) {
        for t in self.tree.iter_mut().chain(self.extra_trees.iter_mut()) {
            t.refresh();
        }
    }

    fn reveal_in_trees(&mut self, path: &Path) {
        for t in self.tree.iter_mut().chain(self.extra_trees.iter_mut()) {
            if path.starts_with(&t.root) {
                t.reveal(path);
            }
        }
    }

    /// Workspace folder a file belongs to (an added folder, else the main one).
    fn root_of(&self, path: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = path {
            if let Some(t) = self.extra_trees.iter().find(|t| p.starts_with(&t.root)) {
                return Some(t.root.clone());
            }
        }
        self.workspace.clone()
    }

    fn active_root(&self) -> Option<PathBuf> {
        self.root_of(self.docs.get(self.active).and_then(|d| d.path()))
    }

    fn all_gits(&self) -> impl Iterator<Item = &Git> {
        self.git.iter().chain(self.extra_gits.iter().flatten())
    }

    /// Repository slot (0 = main, i+1 = extra folder i) owning `path`: the
    /// deepest repository root containing it.
    fn repo_index(&self, path: Option<&Path>) -> usize {
        let Some(path) = path else { return 0 };
        let slots = std::iter::once(self.git.as_ref()).chain(self.extra_gits.iter().map(Option::as_ref));
        slots
            .enumerate()
            .filter_map(|(i, g)| g.filter(|g| path.starts_with(&g.root)).map(|g| (i, g.root.components().count())))
            .max_by_key(|&(_, depth)| depth)
            .map_or(0, |(i, _)| i)
    }

    /// Stage or unstage the git change at `line` of the active file.
    fn stage_hunk(&mut self, line: usize, stage: bool) {
        self.stage_hunk_in(self.active, line, stage);
    }

    fn stage_hunk_in(&mut self, doc: usize, line: usize, stage: bool) {
        let Some((path, text)) = self.docs.get(doc).and_then(|d| Some((d.path()?.to_path_buf(), d.buffer.text())))
        else {
            return;
        };
        let slot = self.repo_index(Some(&path));
        let r = self.with_repo(slot, |app| {
            let g = app.git.as_mut().filter(|g| path.starts_with(&g.root))?;
            Some(if stage { g.stage_hunk(&path, &text, line) } else { g.unstage_hunk(&path, &text, line) })
        });
        match r {
            Some(Ok(msg)) => self.toast(msg, NotifyLevel::Info),
            Some(Err(e)) => self.toast(e, NotifyLevel::Warning),
            None => self.toast("This file isn't in a Git repository", NotifyLevel::Info),
        }
    }

    fn git_for(&self, path: &Path) -> Option<&Git> {
        self.repo_slot(self.repo_index(Some(path))).as_ref().filter(|g| path.starts_with(&g.root))
    }

    fn repo_slot(&self, i: usize) -> &Option<Git> {
        if i == 0 {
            &self.git
        } else {
            self.extra_gits.get(i - 1).unwrap_or(&self.git)
        }
    }

    /// Run `f` with repository slot `i` temporarily in `self.git`, so the
    /// single-repo code paths work on any folder's repository.
    fn with_repo<R>(&mut self, i: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        if i == 0 || i > self.extra_gits.len() {
            return f(self);
        }
        std::mem::swap(&mut self.git, &mut self.extra_gits[i - 1]);
        let r = f(self);
        std::mem::swap(&mut self.git, &mut self.extra_gits[i - 1]);
        r
    }

    fn active_repo(&self) -> usize {
        self.repo_index(self.docs.get(self.active).and_then(|d| d.path()))
    }

    /// Push the combined status of every repository to the file trees.
    fn update_tree_git_status(&mut self) {
        let map: HashMap<PathBuf, char> =
            self.all_gits().flat_map(|g| g.status.iter().map(|e| (e.path.clone(), e.letter()))).collect();
        for t in self.tree.iter_mut().chain(self.extra_trees.iter_mut()) {
            let mine = map.iter().filter(|(p, _)| p.starts_with(&t.root)).map(|(p, c)| (p.clone(), *c)).collect();
            t.set_git_status(mine);
        }
    }

    /// All workspace folders, primary first.
    fn roots(&self) -> Vec<PathBuf> {
        self.workspace.iter().cloned().chain(self.extra_trees.iter().map(|t| t.root.clone())).collect()
    }

    fn add_root(&mut self, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        if self.workspace.is_none() {
            self.open_workspace(path);
            return;
        }
        if self.roots().contains(&path) {
            return;
        }
        self.extra_index.push(FileIndex::build(&path));
        let git = Git::open(&path).filter(|g| !self.all_gits().any(|o| o.root == g.root));
        self.extra_gits.push(git);
        self.extra_trees.push(FileTree::new(path));
        self.save_session();
    }

    fn remove_root(&mut self, i: usize) {
        if i < self.extra_trees.len() {
            self.extra_trees.remove(i);
            self.extra_index.remove(i);
            self.extra_gits.remove(i);
            self.scm_repo = 0;
            self.update_tree_git_status();
            self.save_session();
        }
    }

    fn load_plugins(&mut self) {
        let mut dirs = vec![settings::plugins_dir()];
        if let Some(ws) = &self.workspace {
            dirs.push(ws.join(".zenith").join("plugins"));
        }
        let mut host = PluginHost::default();
        host.set_waker(self.waker.clone());
        host.set_workspace(self.workspace.clone());
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
                if let Some(bps) = self.bp_store.remove(&path) {
                    doc.breakpoints = bps;
                    doc.breakpoints_gen += 1;
                }
                if let Some(g) = self.git_for(&path) {
                    doc.set_git_base(g.head_text(&path));
                    doc.blame_rx = g.blame_async(&path);
                }
                self.docs.push(doc);
                self.active = self.docs.len() - 1;
                self.reveal_in_trees(&path);
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
                if let (Some(r), Some(p)) = (&self.remote, &path) {
                    r.upload(p);
                }
                if let Some(p) = &path {
                    if let Some(parent) = p.parent() {
                        for t in self.tree.iter_mut().chain(self.extra_trees.iter_mut()) {
                            t.refresh_dir(parent);
                        }
                    }
                    if Some(p.as_path()) == settings::settings_path().canonicalize().ok().as_deref()
                        || p == &settings::settings_path()
                        || self.workspace.as_ref().is_some_and(|w| p == &settings::workspace_settings_path(w))
                    {
                        self.reload_settings();
                    }
                    if p == &crate::keymap::path() || p.canonicalize().ok() == crate::keymap::path().canonicalize().ok()
                    {
                        self.load_keymap();
                        self.toast("Keyboard shortcuts reloaded", NotifyLevel::Info);
                    }
                    self.plugins.emit("file_saved", serde_json::json!({ "path": p }));
                    let rx = self.git_for(p).and_then(|g| g.blame_async(p));
                    if let Some(doc) = self.docs.get_mut(idx) {
                        doc.blame_rx = rx;
                    }
                    let root = self.root_of(self.docs.get(idx).and_then(|d| d.path()));
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
        self.reload_settings_with(true);
    }

    /// Reload settings.toml and the workspace's overrides.
    fn reload_settings_with(&mut self, announce: bool) {
        let (user, err) = Settings::load();
        if let Some(e) = err {
            self.toast(e, NotifyLevel::Error);
            return;
        }
        let layered = user.for_workspace(self.workspace.as_deref());
        if let Some(e) = &layered.error {
            self.toast(e.clone(), NotifyLevel::Error);
        }
        if !layered.ignored.is_empty() {
            self.toast(
                format!(
                    ".zenith/settings.toml: ignored {} (only allowed in your own settings)",
                    layered.ignored.join(", ")
                ),
                NotifyLevel::Warning,
            );
        }
        let s = layered.settings;
        let unchanged = toml::Table::try_from(&s).ok() == toml::Table::try_from(&self.settings).ok();
        self.user_settings = user;
        self.ws_overrides = layered.overridden;
        if unchanged {
            if announce {
                self.toast("Settings reloaded", NotifyLevel::Info);
            }
            return;
        }
        let lsp_changed = s.lsp_enabled != self.settings.lsp_enabled
            || toml::Value::try_from(&s.language_servers).ok()
                != toml::Value::try_from(&self.settings.language_servers).ok();
        let theme_changed = s.syntax_theme != self.settings.syntax_theme || s.tree_sitter != self.settings.tree_sitter;
        crate::highlight::TREE_SITTER.store(s.tree_sitter, std::sync::atomic::Ordering::Relaxed);
        self.settings = s;
        self.colors = Palette::by_name(&self.settings.theme);
        self.agents = merge_agents(&self.settings.agents);
        self.installed = self.agents.iter().map(|a| (a.id.clone(), a.is_installed())).collect();
        if theme_changed {
            self.set_syntax_theme(self.settings.syntax_theme.clone());
        }
        if lsp_changed {
            self.lsp.shutdown();
            self.lsp = LspManager::new(&self.settings.language_servers, self.settings.lsp_enabled, self.waker.clone());
            self.lsp.restart(&mut self.docs);
        }
        self.style_dirty = true;
        if announce {
            self.toast("Settings reloaded", NotifyLevel::Info);
        }
    }

    fn write_settings(&mut self) {
        let path = settings::settings_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        // Rewrite simple top-level keys while keeping comments/agents intact.
        let mut doc: toml::Table = toml::from_str(&text).unwrap_or_default();
        let mut fresh = toml::Table::try_from(&self.settings).unwrap_or_default();
        // Keys the workspace overrides keep the user's own value.
        let user = toml::Table::try_from(&self.user_settings).unwrap_or_default();
        for k in &self.ws_overrides {
            if let Some(v) = user.get(k) {
                fresh.insert(k.clone(), v.clone());
            }
        }
        if let Ok(u) = toml::Value::Table(fresh.clone()).try_into::<Settings>() {
            self.user_settings = u;
        }
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
            // Semantic colors come from the theme too: fetch them again.
            d.semantic.clear();
            d.semantic_version = None;
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
        if let (Some(p), false) = (doc.path(), doc.breakpoints.is_empty()) {
            self.bp_store.insert(p.to_path_buf(), doc.breakpoints.clone());
        }
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
        if let Some(ws) = &self.workspace {
            if let Ok(r) = p.strip_prefix(ws) {
                return r.to_string_lossy().replace('\\', "/");
            }
        }
        for t in &self.extra_trees {
            if let Ok(r) = p.strip_prefix(&t.root) {
                let name = t.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return format!("{name}/{}", r.to_string_lossy().replace('\\', "/"));
            }
        }
        p.to_string_lossy().into_owned()
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
        let all = self.all_breakpoints();
        self.session.breakpoints = all.iter().map(|(p, l)| (p.clone(), l.keys().copied().collect())).collect();
        self.session.breakpoint_details = all
            .iter()
            .flat_map(|(p, l)| l.iter().filter(|(_, b)| !b.is_plain()).map(|(&n, b)| (p.clone(), n, b.clone())))
            .collect();
        self.session.watches = self.debugger.watches.clone();
        self.session.remote = self.remote.as_ref().map(|r| r.spec.clone());
        self.session.remote_since = self.remote.as_ref().map_or(0, |r| r.since);
        self.session.show_sidebar = Some(self.show_sidebar);
        self.session.sidebar = match self.sidebar {
            SidebarView::Explorer => "explorer".into(),
            SidebarView::Search => "search".into(),
            SidebarView::Git => "git".into(),
            SidebarView::Problems => "problems".into(),
            SidebarView::Agents => "agents".into(),
            SidebarView::Plugins => "plugins".into(),
            SidebarView::Debug => "debug".into(),
            SidebarView::Tests => "tests".into(),
            SidebarView::PluginPanel(i) => match self.plugins.panels().get(i) {
                Some((plugin, panel)) => format!("plugin:{plugin}/{}", panel.id),
                None => "explorer".into(),
            },
        };
        self.session.show_terminal = self.show_terminal;
        self.session.show_ai = self.show_ai;
        self.session.split = self.split;
        self.session.extra_roots = self.extra_trees.iter().map(|t| t.root.clone()).collect();
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
    /// Breakpoints of every file: open documents and closed ones.
    fn all_breakpoints(&self) -> std::collections::BTreeMap<PathBuf, crate::debug::Breakpoints> {
        let mut all = self.bp_store.clone();
        for d in &self.docs {
            if let (Some(p), false) = (d.path(), d.breakpoints.is_empty()) {
                all.insert(p.to_path_buf(), d.breakpoints.clone());
            }
        }
        all
    }

    fn restore_session_state(&mut self, sess: &Session) {
        self.debugger.watches = sess.watches.clone();
        for (path, lines) in &sess.breakpoints {
            let set: crate::debug::Breakpoints = lines
                .iter()
                .map(|&l| {
                    let detail = sess.breakpoint_details.iter().find(|(p, n, _)| p == path && *n == l);
                    (l, detail.map(|(_, _, b)| b.clone()).unwrap_or_default())
                })
                .collect();
            match self.docs.iter_mut().find(|d| d.path() == Some(path.as_path())) {
                Some(d) => {
                    d.breakpoints = set;
                    d.breakpoints_gen += 1;
                }
                None => {
                    self.bp_store.insert(path.clone(), set);
                }
            }
        }
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
            "debug" => SidebarView::Debug,
            "tests" => SidebarView::Tests,
            other => match other.strip_prefix("plugin:").and_then(|k| k.split_once('/')) {
                Some((plugin, panel)) => self
                    .plugins
                    .panels()
                    .iter()
                    .position(|(pl, pa)| pl == plugin && pa.id == panel)
                    .map_or(SidebarView::Explorer, SidebarView::PluginPanel),
                None => SidebarView::Explorer,
            },
        };
        if let (SidebarView::PluginPanel(i), true) = (self.sidebar, self.show_sidebar) {
            self.open_plugin_panel(i);
        }
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
        if let Some(r) = &self.remote {
            // The shell runs on the host, in the remote folder.
            spec = CommandSpec::program("ssh".to_string(), crate::remote::shell_args(&r.spec))
                .with_cwd(Some(r.cache.clone()));
        }
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
        // Checkpoint the workspace so everything the agent changes can be reviewed.
        if self.changes.base.is_none() && !self.changes.busy() {
            if let Some(root) = self.git.as_ref().map(|g| g.root.clone()) {
                self.changes.start(root);
            }
        }
        let id = self.id();
        let mut args = agent.args.clone();
        if resume {
            args.extend(agent.resume_args.iter().cloned());
        }
        let mut spec = CommandSpec::program(agent.command.clone(), args.clone()).with_cwd(self.workspace.clone());
        spec.env = agent.env.clone();
        if let Some(r) = &self.remote {
            // Agents work on the host's files; their edits are fetched back.
            spec = CommandSpec::program("ssh".to_string(), crate::remote::program_args(&r.spec, &agent.command, &args))
                .with_cwd(Some(r.cache.clone()));
        }
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

    /// Files offered for an `@` mention: open files first, then the index.
    fn mention_candidates(&mut self, query: &str) -> Vec<crate::mention::Candidate> {
        use crate::mention::Candidate;
        if let Some((q, found)) = &self.mention_cache {
            if q == query {
                return found.clone();
            }
        }
        let ws = self.workspace.clone();
        let insert_for = |p: &Path| match ws.as_ref().and_then(|w| p.strip_prefix(w).ok()) {
            Some(r) => r.to_string_lossy().replace('\\', "/"),
            None => p.to_string_lossy().into_owned(),
        };
        let mut seen = std::collections::HashSet::new();
        let mut all = vec![];
        for d in &self.docs {
            if let Some(p) = d.path() {
                let label = self.rel_path(p);
                if seen.insert(label.clone()) {
                    all.push(Candidate { label, insert: insert_for(p) });
                }
            }
        }
        for f in &self.index.files {
            if !seen.contains(f) {
                all.push(Candidate { label: f.clone(), insert: f.clone() });
            }
        }
        for (t, idx) in self.extra_trees.iter().zip(&self.extra_index) {
            let name = t.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for f in &idx.files {
                let label = format!("{name}/{f}");
                if !seen.contains(&label) {
                    all.push(Candidate { insert: insert_for(&t.root.join(f)), label });
                }
            }
        }
        let found = crate::mention::rank(&all, query, 8);
        self.mention_cache = Some((query.to_string(), found.clone()));
        found
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
            Cmd::AddFolderToWorkspace => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.add_root(dir);
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
                    .collect::<Vec<_>>();
                let mut items = items;
                for (t, idx) in self.extra_trees.iter().zip(self.extra_index.iter_mut()) {
                    idx.poll();
                    let name = t.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    items.extend(idx.files.iter().map(|f| Item {
                        label: format!("{name}/{f}"),
                        detail: String::new(),
                        action: PaletteAction::OpenFile(t.root.join(f).to_string_lossy().into_owned()),
                    }));
                }
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
            Cmd::GotoWorkspaceSymbol => {
                self.ws_symbols = Default::default();
                self.palette.show(Mode::WorkspaceSymbols, vec![], "Type to search symbols in the workspace");
                if self.lsp.running().is_empty() {
                    self.toast("No language server is running yet — open a source file first", NotifyLevel::Info);
                }
            }
            Cmd::ToggleInlayHints => {
                self.settings.inlay_hints = !self.settings.inlay_hints;
                self.write_settings();
                for d in &mut self.docs {
                    d.inlay_hints.clear();
                    d.inlay_version = None;
                }
            }
            Cmd::ShowDebug => {
                self.sidebar = SidebarView::Debug;
                self.show_sidebar = true;
                self.debug_configs = None;
            }
            Cmd::ToggleBreakpoint => {
                if let Some(doc) = self.active_doc() {
                    let line = doc.buffer.cursor_pos().line;
                    doc.toggle_breakpoint(line);
                }
            }
            Cmd::OpenLaunchConfig => {
                let Some(ws) = self.workspace.clone() else {
                    self.toast("Open a folder first", NotifyLevel::Info);
                    return;
                };
                let p = crate::debug::launch_file(&ws);
                if !p.exists() {
                    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&ws));
                    let _ = std::fs::write(&p, crate::debug::LAUNCH_TEMPLATE);
                }
                self.debug_configs = None;
                self.open_file(&p);
            }
            Cmd::DebugStart => {
                if self.debugger.stopped_thread().is_some() {
                    self.debugger.continue_();
                } else if !self.debugger.active() {
                    self.start_debugging();
                }
            }
            Cmd::DebugStop => {
                self.debugger.stop();
                self.clear_debug_lines();
            }
            Cmd::DebugRestart => {
                self.debugger.stop();
                self.clear_debug_lines();
                self.start_debugging();
            }
            Cmd::DebugPause => self.debugger.pause(),
            Cmd::DebugStepOver => self.debugger.step("next"),
            Cmd::DebugStepInto => self.debugger.step("stepIn"),
            Cmd::DebugStepOut => self.debugger.step("stepOut"),
            Cmd::ReviewAgentChanges => {
                if self.changes.base.is_none() && !self.changes.busy() {
                    match self.git.as_ref().map(|g| g.root.clone()) {
                        Some(root) => {
                            self.changes.start(root);
                            self.toast(
                                "Checkpoint taken: changes made from now on will be listed here",
                                NotifyLevel::Info,
                            );
                        }
                        None => {
                            self.toast("Reviewing agent changes needs a Git repository", NotifyLevel::Warning);
                            return;
                        }
                    }
                }
                self.show_changes = true;
                self.changes_refreshed = None;
            }
            Cmd::SplitTerminal => {
                if self.bottom.split_index().is_some() {
                    self.bottom.split = None;
                } else if self.bottom.tabs.is_empty() {
                    self.new_terminal(None);
                    let keep = self.bottom.active;
                    let id = self.new_terminal(None);
                    self.bottom.split = Some(id);
                    self.bottom.active = keep;
                } else {
                    let keep = self.bottom.active;
                    let id = self.new_terminal(None);
                    self.bottom.split = Some(id);
                    self.bottom.active = keep;
                }
                self.show_terminal = true;
            }
            Cmd::OpenKeybindings => {
                let p = crate::keymap::path();
                if !p.exists() {
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&p, crate::keymap::template());
                }
                self.open_file(&p);
            }
            Cmd::ToggleVimMode => {
                self.settings.vim_mode = !self.settings.vim_mode;
                self.write_settings();
                for d in &mut self.docs {
                    d.vim = Default::default();
                }
                let on = if self.settings.vim_mode { "on" } else { "off" };
                self.toast(format!("Vim mode {on}"), NotifyLevel::Info);
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
            Cmd::StageChangeAtCursor | Cmd::UnstageChangeAtCursor => {
                if let Some(line) = self.docs.get(self.active).map(|d| d.buffer.cursor_pos().line) {
                    self.stage_hunk(line, cmd == Cmd::StageChangeAtCursor);
                }
            }
            Cmd::OpenWorkspaceSettings => {
                let Some(ws) = self.workspace.clone() else {
                    self.toast("Open a folder first", NotifyLevel::Info);
                    return;
                };
                let p = settings::workspace_settings_path(&ws);
                if !p.exists() {
                    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&ws));
                    let _ = std::fs::write(
                        &p,
                        "# Settings for this project only: they override your own settings.toml\n\
# while this folder is open. Commit the file to share them with your team.\n\
# Programs, agents, language servers and network settings can't be set here.\n\
#\n\
# tab_size = 2\n\
# insert_spaces = true\n\
# format_on_save = true\n\
# word_wrap = false\n",
                    );
                }
                self.open_file(&p);
            }
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
            Cmd::AskAgent => {
                self.mention_cache = None;
                self.dialog = Some(Dialog::AskAgent { question: String::new(), focus: true, caret: 0, pick: 0 });
            }
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
                let Some(g) = self.repo_slot(self.active_repo()) else {
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
                self.scm_repo = self.active_repo();
                self.show_sidebar = true;
                for g in self.git.iter_mut().chain(self.extra_gits.iter_mut().flatten()) {
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
                let root = self.active_root();
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
                let root = self.active_root();
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
            Cmd::CheckForUpdates => self.start_update_check(true),
            Cmd::OpenRemoteFolder => {
                let value = self.session.recent_remotes.first().cloned().unwrap_or_default();
                self.dialog = Some(Dialog::Input {
                    title: "Open folder over SSH (user@host:/path)".into(),
                    value,
                    kind: InputKind::RemoteFolder,
                    focus: true,
                });
            }
            Cmd::RemoteSyncNow => match &mut self.remote {
                Some(r) => r.pull_now(),
                None => self.toast("No remote folder is open", NotifyLevel::Info),
            },
            Cmd::SyncPush => self.start_sync(crate::sync_ui::SyncOp::Push),
            Cmd::SyncPull => self.start_sync(crate::sync_ui::SyncOp::Pull),
            Cmd::ShowTests => {
                self.sidebar = SidebarView::Tests;
                self.show_sidebar = true;
            }
            Cmd::RunAllTests => {
                self.sidebar = SidebarView::Tests;
                self.show_sidebar = true;
                let root = self.workspace.clone();
                self.tests.ensure_discovered(root.as_deref(), self.waker.clone());
                if !self.tests.items.is_empty() {
                    self.tests.run(None, self.waker.clone());
                }
            }
            Cmd::DebugTestAtCursor => {
                let name = self.test_name_at_cursor();
                self.sidebar = SidebarView::Tests;
                self.show_sidebar = true;
                let ids = name.as_deref().map(|n| self.tests.ids_named(n)).unwrap_or_default();
                match ids.first().and_then(|id| self.tests.item(id)).cloned() {
                    Some(t) => self.debug_test(&t),
                    None => {
                        let msg = match name {
                            Some(n) => format!("No test named `{n}` (discover tests with ⟳ first)"),
                            None => "Put the caret inside a test function".into(),
                        };
                        self.toast(msg, NotifyLevel::Info);
                    }
                }
            }
            Cmd::RunTestAtCursor => {
                let name = self.test_name_at_cursor();
                self.sidebar = SidebarView::Tests;
                self.show_sidebar = true;
                let ids = name.as_deref().map(|n| self.tests.ids_named(n)).unwrap_or_default();
                if ids.is_empty() {
                    let msg = match name {
                        Some(n) => format!("No test named `{n}` (discover tests with ⟳ first)"),
                        None => "Put the caret inside a test function".into(),
                    };
                    self.toast(msg, NotifyLevel::Info);
                } else {
                    self.tests.run(Some(ids), self.waker.clone());
                }
            }
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
            PaletteAction::OpenFile(rel) if Path::new(&rel).is_absolute() => {
                self.open_file(Path::new(&rel));
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
            PaletteAction::OpenAt(path, line, character) => {
                if self.open_file(&path) {
                    if let Some(doc) = self.active_doc() {
                        let idx =
                            doc.lsp_to_char(zenith_lsp::LspPos { line: line as u32, character: character as u32 });
                        doc.buffer.clear_extra();
                        doc.buffer.set_cursor(idx, false);
                        doc.reveal_line = Some(line);
                    }
                    self.focus_editor = true;
                }
            }
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
                    let root = self.active_root();
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
                PluginEventKind::Exited => {
                    log::info!("plugin {} exited", ev.plugin_id);
                    self.status_items.retain(|(p, _), _| *p != ev.plugin_id);
                }
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
            Action::SetStatus { id, text, tooltip, command, align, color } => {
                let key = (plugin.to_string(), id);
                if text.is_empty() {
                    self.status_items.remove(&key);
                } else {
                    let right = align == "right";
                    self.status_items.insert(key, StatusItem { text, tooltip, command, right, color });
                }
            }
            Action::SetPanel { panel, items, message, buttons } => {
                self.panel_data.insert(format!("{plugin}/{panel}"), PanelData { items, message, buttons });
            }
        }
    }

    /// Ask the plugin to fill (refresh) panel `i`.
    fn open_plugin_panel(&mut self, i: usize) {
        let Some((plugin, panel)) = self.plugins.panels().into_iter().nth(i) else { return };
        let ectx = self.editor_context();
        if let Err(e) = self.plugins.open_panel(&plugin, &panel.id, &ectx) {
            self.toast(format!("[{plugin}] {e:#}"), NotifyLevel::Error);
        }
    }

    fn plugin_panel_ui(&mut self, ui: &mut egui::Ui, p: &Palette, i: usize) {
        let Some((plugin, panel)) = self.plugins.panels().into_iter().nth(i) else {
            self.sidebar = SidebarView::Explorer;
            return;
        };
        let key = format!("{plugin}/{}", panel.id);
        let mut run: Option<(String, Option<String>)> = None;
        let mut open: Option<(String, Option<usize>)> = None;
        let mut refresh = false;
        let data = self.panel_data.get(&key);
        ui.horizontal(|ui| {
            Self::section_header(ui, p, &panel.title.to_uppercase());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    refresh = true;
                }
                for b in data.map(|d| d.buttons.as_slice()).unwrap_or_default().iter().rev() {
                    if ui.small_button(&b.title).clicked() {
                        run = Some((b.command.clone(), None));
                    }
                }
            });
        });
        match data {
            None => {
                ui.label(RichText::new("Loading…").color(p.text_dim));
            }
            Some(d) => {
                if !d.message.is_empty() {
                    ui.label(RichText::new(&d.message).color(p.text_dim));
                    ui.add_space(4.0);
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (k, it) in d.items.iter().enumerate() {
                        let indent = it.indent as f32 * 14.0;
                        if it.header {
                            ui.add_space(if k == 0 { 0.0 } else { 6.0 });
                            ui.horizontal(|ui| {
                                ui.add_space(indent);
                                let text = if it.icon.is_empty() {
                                    it.label.clone()
                                } else {
                                    format!("{} {}", it.icon, it.label)
                                };
                                ui.label(RichText::new(text).strong().color(p.text));
                                if !it.detail.is_empty() {
                                    ui.label(RichText::new(&it.detail).size(11.0).color(p.text_dim));
                                }
                            });
                            continue;
                        }
                        let clickable = it.path.is_some() || it.command.is_some();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 22.0),
                            if clickable { Sense::click() } else { Sense::hover() },
                        );
                        if resp.hovered() && clickable {
                            ui.painter().rect_filled(rect, 4.0, p.bg_hover);
                        }
                        let painter = ui.painter().with_clip_rect(rect);
                        let mut x = rect.min.x + 8.0 + indent;
                        if !it.icon.is_empty() {
                            let r = painter.text(
                                egui::pos2(x, rect.center().y),
                                Align2::LEFT_CENTER,
                                &it.icon,
                                FontId::proportional(12.0),
                                p.accent,
                            );
                            x = r.max.x + 6.0;
                        }
                        let r = painter.text(
                            egui::pos2(x, rect.center().y),
                            Align2::LEFT_CENTER,
                            &it.label,
                            FontId::proportional(13.0),
                            p.text,
                        );
                        if !it.detail.is_empty() {
                            painter.text(
                                egui::pos2(r.max.x + 8.0, rect.center().y),
                                Align2::LEFT_CENTER,
                                &it.detail,
                                FontId::proportional(11.5),
                                p.text_dim,
                            );
                        }
                        let resp = if it.tooltip.is_empty() { resp } else { resp.on_hover_text(&it.tooltip) };
                        if resp.clicked() {
                            if let Some(path) = &it.path {
                                open = Some((path.clone(), it.line));
                            } else if let Some(c) = &it.command {
                                run = Some((c.clone(), it.id.clone()));
                            }
                        }
                    }
                });
            }
        }
        if refresh {
            self.open_plugin_panel(i);
        }
        if let Some((path, line)) = open {
            self.apply_plugin_action(&plugin, Action::OpenFile { path, line });
        }
        if let Some((cmd, item)) = run {
            let ectx = self.editor_context();
            if let Err(e) = self.plugins.execute_item(&cmd, &ectx, item.as_deref()) {
                self.toast(format!("[{plugin}] {e:#}"), NotifyLevel::Error);
            }
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

    /// Re-query workspace symbols when the palette's query changed (debounced).
    fn update_workspace_symbols(&mut self) {
        if !(self.palette.open && self.palette.mode == Mode::WorkspaceSymbols) {
            return;
        }
        let q = self.palette.query.trim().to_string();
        if q == self.ws_symbols.query {
            self.ws_symbols.changed = None;
            return;
        }
        let since = *self.ws_symbols.changed.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_millis(150) {
            self.waker.as_ref()();
            return;
        }
        self.ws_symbols.query = q.clone();
        self.ws_symbols.changed = None;
        self.ws_symbols.results.clear();
        self.ws_symbols.pending = if q.is_empty() { vec![] } else { self.lsp.request_workspace_symbols(&q) };
        if q.is_empty() {
            self.palette.set_items(vec![]);
        }
    }

    fn handle_lsp(&mut self) {
        let roots: Vec<Option<PathBuf>> = self.docs.iter().map(|d| self.root_of(d.path())).collect();
        self.lsp.inlay_hints = self.settings.inlay_hints;
        if self.lsp.semantic != self.settings.semantic_highlighting {
            self.lsp.semantic = self.settings.semantic_highlighting;
            for d in &mut self.docs {
                d.semantic.clear();
                d.semantic_version = None;
            }
        }
        for (i, (doc, root)) in self.docs.iter_mut().zip(roots).enumerate() {
            self.lsp.sync(doc, root.as_deref(), false);
            // Hints only for what's on screen.
            if i == self.active || (self.split && i == self.other) {
                self.lsp.maybe_request_inlay_hints(doc, root.as_deref());
                self.lsp.maybe_request_semantic(doc, root.as_deref());
            }
        }
        self.update_workspace_symbols();
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
                LspOutcome::WorkspaceSymbols { request, symbols } => {
                    if let Some(pos) = self.ws_symbols.pending.iter().position(|r| *r == request) {
                        self.ws_symbols.pending.remove(pos);
                        self.ws_symbols.results.extend(symbols);
                        let items = self
                            .ws_symbols
                            .results
                            .iter()
                            .filter_map(|s| {
                                let path = s.path.clone()?;
                                Some(Item {
                                    label: s.name.clone(),
                                    detail: format!("{}  {}:{}", s.kind_label(), self.rel_path(&path), s.line + 1),
                                    action: PaletteAction::OpenAt(path, s.line as usize, s.character as usize),
                                })
                            })
                            .collect();
                        if self.palette.open && self.palette.mode == Mode::WorkspaceSymbols {
                            self.palette.set_items(items);
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
        let mut changed = false;
        for g in self.git.iter_mut().chain(self.extra_gits.iter_mut().flatten()) {
            changed |= g.poll();
        }
        if changed {
            self.update_tree_git_status();
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
        self.with_repo(self.active_repo(), |app| app.git_switch_current(branch, create));
    }

    fn git_switch_current(&mut self, branch: &str, create: bool) {
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
                self.refresh_trees();
                self.toast(format!("Switched to branch {branch}"), NotifyLevel::Info);
            }
            Err(e) => self.toast(format!("git: {e}"), NotifyLevel::Error),
        }
    }

    /// Re-read HEAD contents for open files (after commit / checkout).
    fn reload_git_bases(&mut self) {
        let bases: Vec<_> = self
            .docs
            .iter()
            .map(|d| {
                let p = d.path()?;
                let g = self.git_for(p)?;
                Some((g.head_text(p), g.blame_async(p)))
            })
            .collect();
        for (d, b) in self.docs.iter_mut().zip(bases) {
            if let Some((base, blame)) = b {
                d.set_git_base(base);
                d.blame_rx = blame;
            }
        }
    }

    fn git_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, p: &Palette) {
        let roots = self.roots();
        if self.scm_repo >= roots.len() {
            self.scm_repo = 0;
        }
        if roots.len() > 1 {
            let name = |r: &PathBuf| r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut sel = self.scm_repo;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Repository").size(11.5).color(p.text_dim));
                egui::ComboBox::from_id_salt("scm_repo").selected_text(name(&roots[sel])).show_ui(ui, |ui| {
                    for (i, r) in roots.iter().enumerate() {
                        let changes = self.repo_slot(i).as_ref().map_or(0, |g| g.status.len());
                        let label = if changes > 0 { format!("{}  ({changes})", name(r)) } else { name(r) };
                        ui.selectable_value(&mut sel, i, label);
                    }
                });
            });
            ui.add_space(2.0);
            self.scm_repo = sel;
        }
        let root = roots.get(self.scm_repo).cloned();
        self.with_repo(self.scm_repo, |app| app.git_repo_ui(ctx, ui, p, root));
    }

    fn git_repo_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, p: &Palette, root: Option<PathBuf>) {
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
            if root.is_some() && ui.button("Initialize Repository").clicked() {
                if let Some(ws) = root {
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
        let mut result: Option<Result<String, String>> = None;
        let mut op_done = false;
        if let Some(op) = g.operation {
            let conflicts = g.status.iter().filter(|e| e.is_conflicted()).count();
            egui::Frame::new()
                .fill(p.warning.gamma_multiply(0.12))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("⚠ {} in progress", op.label())).color(p.warning).strong());
                    ui.label(
                        RichText::new(if conflicts > 0 {
                            format!(
                                "{conflicts} file(s) with conflicts. Resolve them in the editor, then stage with +."
                            )
                        } else {
                            "All conflicts resolved. Continue to finish.".to_string()
                        })
                        .size(11.5)
                        .color(p.text_dim),
                    );
                    ui.horizontal(|ui| {
                        if ui.add_enabled(conflicts == 0, egui::Button::new("✓ Continue")).clicked() {
                            result = Some(g.continue_operation());
                            op_done = true;
                        }
                        if ui.button("✕ Abort").clicked() {
                            result = Some(g.abort_operation());
                            op_done = true;
                        }
                    });
                });
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
            if ui
                .add_enabled(!g.status.is_empty(), egui::Button::new("⊟ Stash").small())
                .on_hover_text("Stash all changes (message box is used as the stash name)")
                .clicked()
            {
                result = Some(g.stash(&self.commit_message));
                op_done = true;
            }
            if ui
                .add_enabled(g.stashes > 0, egui::Button::new(format!("⊞ Pop ({})", g.stashes)).small())
                .on_hover_text("Apply and drop the latest stash")
                .clicked()
            {
                result = Some(g.stash_pop());
                op_done = true;
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
            for (title, staged, merge) in
                [("MERGE CHANGES", false, true), ("STAGED CHANGES", true, false), ("CHANGES", false, false)]
            {
                let list: Vec<_> = g
                    .status
                    .iter()
                    .filter(|e| {
                        if merge {
                            e.is_conflicted()
                        } else if staged {
                            e.is_staged()
                        } else {
                            e.is_unstaged()
                        }
                    })
                    .collect();
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
                        '!' => p.error,
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
                    if b.on_hover_text(if merge {
                        "Mark as resolved (stage)"
                    } else if staged {
                        "Unstage"
                    } else {
                        "Stage"
                    })
                    .clicked()
                    {
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
        if committed || op_done {
            // Abort / continue / stash change files on disk: clean buffers follow.
            if op_done {
                for d in &mut self.docs {
                    if !d.buffer.is_dirty() && d.changed_on_disk() {
                        let _ = d.reload_from_disk();
                    }
                }
                self.refresh_trees();
            }
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
        let root = self.active_root();
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

    /// Built-in shortcuts: (mods, key, command, allowed while a terminal has focus).
    fn default_shortcuts() -> Vec<(Modifiers, Key, Cmd, bool)> {
        let cmd = Modifiers::COMMAND;
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let ca = Modifiers::COMMAND | Modifiers::ALT;
        let ctrl = Modifiers::CTRL;
        let mac = cfg!(target_os = "macos");
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
            (cs, Key::F5, Cmd::DebugRestart, true),
            (Modifiers::SHIFT, Key::F5, Cmd::DebugStop, true),
            (Modifiers::NONE, Key::F5, Cmd::DebugStart, true),
            (Modifiers::NONE, Key::F6, Cmd::DebugPause, true),
            (Modifiers::NONE, Key::F9, Cmd::ToggleBreakpoint, false),
            (Modifiers::NONE, Key::F10, Cmd::DebugStepOver, true),
            (Modifiers::SHIFT, Key::F11, Cmd::DebugStepOut, true),
            (Modifiers::NONE, Key::F11, Cmd::DebugStepInto, true),
            (cs, Key::D, Cmd::ShowDebug, true),
            (cs, Key::Space, Cmd::TriggerParameterHints, false),
            (cmd, Key::Period, Cmd::CodeActions, false),
            (cs, Key::T, Cmd::GotoWorkspaceSymbol, true),
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
        table
    }

    /// Load keybindings.toml and update the shortcut labels in menus.
    fn load_keymap(&mut self) {
        let (bindings, errors) = crate::keymap::load();
        for e in errors {
            self.toast(e, NotifyLevel::Error);
        }
        let mut labels = HashMap::new();
        for b in &bindings {
            if let Some(crate::keymap::Target::Cmd(c)) = &b.target {
                labels.entry(*c).or_insert_with(|| crate::keymap::format_chord(b.mods, b.key));
            }
        }
        for b in bindings.iter().filter(|b| b.target.is_none()) {
            for (m, k, c, _) in Self::default_shortcuts() {
                if same_chord(m, b.mods) && k == b.key {
                    labels.entry(c).or_insert_with(String::new);
                }
            }
        }
        Cmd::set_labels(labels);
        self.user_keys = bindings;
    }

    fn shortcuts(&mut self, ctx: &egui::Context) -> Vec<PaletteAction> {
        use crate::keymap::Target;
        let mac = cfg!(target_os = "macos");
        // When a terminal has focus on Linux/Windows, plain Ctrl+letter must
        // reach the shell, so only Ctrl+Shift / special combos are global.
        let term = self.terminal_focused && !mac;
        let mut table: Vec<(Modifiers, Key, Target, bool)> =
            Self::default_shortcuts().into_iter().map(|(m, k, c, t)| (m, k, Target::Cmd(c), t)).collect();
        // Plugin commands with a `keybinding` in plugin.toml, then the user's
        // own bindings, each replacing whatever used the same keys.
        let plugin_keys: Vec<(Modifiers, Key, Option<Target>)> = self
            .plugins
            .commands()
            .into_iter()
            .filter_map(|(_, c)| {
                let (m, k) = crate::keymap::parse_chord(c.keybinding.as_deref()?)?;
                Some((m, k, Some(Target::Plugin(c.id.clone()))))
            })
            .collect();
        let user = self.user_keys.iter().map(|b| (b.mods, b.key, b.target.clone()));
        for (m, k, target) in plugin_keys.into_iter().chain(user) {
            table.retain(|(m2, k2, _, _)| !(same_chord(*m2, m) && *k2 == k));
            if let Some(t) = target {
                let ctrl_only = (m.ctrl || m.command) && !m.shift && !m.alt;
                table.push((m, k, t, !ctrl_only));
            }
        }
        // egui matches shortcuts "logically" (extra Shift is ignored), so
        // combinations with more modifiers must be tried first.
        let weight = |m: &Modifiers| u8::from(m.shift) + u8::from(m.alt) + u8::from(m.ctrl || m.command);
        table.sort_by_key(|(m, ..)| std::cmp::Reverse(weight(m)));
        let mut out = vec![];
        ctx.input_mut(|i| {
            for (m, k, t, in_term) in table {
                if term && !in_term {
                    continue;
                }
                if i.consume_shortcut(&KeyboardShortcut::new(m, k)) {
                    out.push(match t {
                        Target::Cmd(c) => PaletteAction::Command(c),
                        Target::Plugin(id) => PaletteAction::PluginCommand(id),
                    });
                }
            }
            if i.consume_key(Modifiers::NONE, Key::F1) {
                out.push(PaletteAction::Command(Cmd::CommandPalette));
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
                item(ui, Cmd::AddFolderToWorkspace, "Add Folder to Workspace…");
                item(ui, Cmd::OpenRemoteFolder, "Open Remote Folder (SSH)…");
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
                item(ui, Cmd::CheckForUpdates, "Check for Updates");
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
                (SidebarView::Debug, "▷", "Run and Debug (Ctrl+Shift+D)"),
                (SidebarView::Tests, "✓", "Testing"),
                (SidebarView::Agents, "✳", "AI Agents"),
                (SidebarView::Plugins, "⊞", "Plugins"),
            ]
            .into_iter()
            .map(|(v, g, t)| (v, g.to_string(), t.to_string()))
            .chain(self.plugins.panels().into_iter().enumerate().map(|(i, (_, panel))| {
                let glyph = if panel.icon.is_empty() {
                    panel.title.chars().next().unwrap_or('◇').to_uppercase().to_string()
                } else {
                    panel.icon.clone()
                };
                (SidebarView::PluginPanel(i), glyph, panel.title.clone())
            })) {
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
                        if let SidebarView::PluginPanel(i) = view {
                            self.open_plugin_panel(i);
                        }
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
                                self.refresh_trees();
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
                let mut remove = None;
                let action = match &mut self.tree {
                    Some(tree) if !self.extra_trees.is_empty() => {
                        // Multi-root: every folder under its own header, one scroll area.
                        let mut action = None;
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            if let Some(a) = tree.contents_ui(ui, &p, active_path.as_deref()) {
                                action = Some(a);
                            }
                            for (i, t) in self.extra_trees.iter_mut().enumerate() {
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    let name = t
                                        .root
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_uppercase())
                                        .unwrap_or_default();
                                    ui.label(RichText::new(name).strong().size(12.0));
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.small_button("✕").on_hover_text("Remove folder from workspace").clicked()
                                        {
                                            remove = Some(i);
                                        }
                                    });
                                });
                                ui.add_space(2.0);
                                if let Some(a) = t.contents_ui(ui, &p, active_path.as_deref()) {
                                    action = Some(a);
                                }
                            }
                        });
                        action
                    }
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
                if let Some(i) = remove {
                    self.remove_root(i);
                }
            }
            SidebarView::Search => self.search_ui(ui, &p),
            SidebarView::Agents => self.agents_ui(ctx, ui, &p),
            SidebarView::Plugins => self.plugins_ui(ctx, ui, &p),
            SidebarView::Debug => self.debug_ui(ui, &p),
            SidebarView::Tests => {
                let root = self.workspace.clone();
                let waker = self.waker.clone();
                match self.tests.ui(ui, &p, root.as_deref(), waker) {
                    Some(crate::testing::Action::Open(path, line)) => {
                        if self.open_file(&path) {
                            if let Some(doc) = self.docs.get_mut(self.active) {
                                doc.goto_line(line);
                            }
                        }
                    }
                    Some(crate::testing::Action::Debug(t)) => self.debug_test(&t),
                    None => {}
                }
            }
            SidebarView::PluginPanel(i) => self.plugin_panel_ui(ui, &p, i),
            SidebarView::Git => self.git_ui(ctx, ui, &p),
            SidebarView::Problems => self.problems_ui(ui, &p),
        }
    }

    fn search_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        Self::section_header(ui, p, "SEARCH");
        if self.workspace.is_none() {
            ui.label(RichText::new("Open a folder to search in files.").color(p.text_dim));
            return;
        }
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
                match SearchJob::start(&self.roots(), &self.search.query, self.search.opts) {
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
        let mut mcp_settings_changed = false;
        let mut mcp_message: Option<String> = None;
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
            match self.mcp.ui(ui, p, self.workspace.as_deref(), &mut self.settings.mcp_write_gemini) {
                Some(m) if m.is_empty() => mcp_settings_changed = true,
                Some(m) => mcp_message = Some(m),
                None => {}
            }
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
        if mcp_settings_changed {
            self.write_settings();
        }
        if let Some(m) = mcp_message {
            self.toast(m, NotifyLevel::Info);
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
            if ui
                .button("+ Create sample")
                .on_hover_text("Scaffold example plugins you can edit (commands + a sidebar panel)")
                .clicked()
            {
                match crate::sample_plugin::scaffold(&settings::plugins_dir()) {
                    Ok(dir) => {
                        self.load_plugins();
                        self.toast(format!("Sample plugins created in {}", dir.display()), NotifyLevel::Info);
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
        let mut market_act: Option<MarketAct> = None;
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
                            if pl.may_read_workspace() {
                                ui.label(RichText::new("WASM · reads workspace").small().color(p.warning)).on_hover_text(
                                    "Runs inside Zenith as WebAssembly. It may read the open folder (not change it); no network or environment",
                                );
                            } else if pl.is_wasm() {
                                ui.label(RichText::new("WASM · sandboxed").small().color(p.accent)).on_hover_text(
                                    "Runs inside Zenith as WebAssembly: no access to files, network or environment",
                                );
                            }
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
            market_ui(&mut self.market, ui, p, &self.settings.plugin_registry, &mut market_act, self.waker.clone());
            ui.add_space(8.0);
            Self::section_header(ui, p, "LANGUAGE SERVERS");
            let running = self.lsp.running();
            for cfg in self.lsp.configs() {
                let is_running = running.iter().any(|k| k.starts_with(&format!("{} · ", cfg.name)));
                let (dot, tip) = if is_running {
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
        match market_act {
            Some(MarketAct::Install(e)) => {
                // Updating: stop the running copy first (Windows can't replace a folder in use).
                if crate::marketplace::installed_version(&settings::plugins_dir(), &e.id).is_some() {
                    self.plugins.shutdown();
                }
                self.market.install(e, settings::plugins_dir(), self.waker.clone());
            }
            Some(MarketAct::Uninstall(id)) => {
                // Stop it before its folder goes away.
                self.plugins.shutdown();
                match crate::marketplace::uninstall(&id, &settings::plugins_dir()) {
                    Ok(msg) => self.toast(msg, NotifyLevel::Info),
                    Err(e) => self.toast(e, NotifyLevel::Error),
                }
                self.load_plugins();
            }
            None => {}
        }
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
                if let Some(p) = path {
                    self.reveal_in_trees(&p);
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
                match crate::review::apply(doc, a) {
                    // Reverts are written to disk: send them to the host too.
                    Ok(()) => {
                        if let (Some(r), Some(p)) = (&self.remote, doc.path()) {
                            r.upload(p);
                        }
                    }
                    Err(e) => self.toast(format!("{e}"), NotifyLevel::Error),
                }
            }
        }
        if self.docs.get(shown).is_some_and(|d| d.show_git_diff) {
            let fs = self.settings.editor_font_size;
            let mut hunk_act = None;
            if let Some(doc) = self.docs.get_mut(shown) {
                match crate::diff_view::ui(ui, doc, &p, fs) {
                    Some(crate::diff_view::DiffAction::Close) => {
                        doc.show_git_diff = false;
                        self.focus_editor = true;
                    }
                    Some(crate::diff_view::DiffAction::StageHunk(line)) => hunk_act = Some((line, true)),
                    Some(crate::diff_view::DiffAction::RevertHunk(line)) => {
                        doc.revert_git_hunk(line);
                    }
                    None => {}
                }
            }
            if let Some((line, stage)) = hunk_act {
                self.stage_hunk_in(shown, line, stage);
            }
            return;
        }
        let showing_diff = self.docs.get(shown).is_some_and(|d| d.review.as_ref().is_some_and(|r| r.show));
        if showing_diff {
            let fs = self.settings.editor_font_size;
            if let Some(doc) = self.docs.get_mut(shown) {
                if let Some(a) = crate::review::diff_view(ui, doc, &p, fs) {
                    match crate::review::apply(doc, a) {
                        Ok(()) => {
                            if let (Some(r), Some(p)) = (&self.remote, doc.path()) {
                                r.upload(p);
                            }
                        }
                        Err(e) => self.toast(format!("{e}"), NotifyLevel::Error),
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
                EditorAction::Notify(m) => self.toast(m, NotifyLevel::Warning),
                EditorAction::EditBreakpoint { line, field } => {
                    use crate::editor_view::BpField;
                    let Some(doc) = self.docs.get(self.active) else { continue };
                    let b = doc.breakpoints.get(&line).cloned().unwrap_or_default();
                    let (title, value) = match field {
                        BpField::Condition => ("Breakpoint condition (stop when true)", b.condition),
                        BpField::Log => ("Log message — {expression} is replaced", b.log),
                        BpField::Hit => ("Hit count (e.g. 5)", b.hit),
                    };
                    self.dialog = Some(Dialog::Input {
                        title: title.into(),
                        value,
                        kind: InputKind::Breakpoint(doc.id, line, field),
                        focus: true,
                    });
                }
                EditorAction::DebugHover { idx } => {
                    if let Some(doc) = self.docs.get(self.active) {
                        if let Some(expr) = crate::debug::expression_at(&doc.buffer, idx) {
                            let id = doc.id;
                            self.debugger.evaluate_hover(id, idx, expr);
                        }
                    }
                }
                EditorAction::AskAgent { question } => {
                    let prompt = self.selection_prompt(&question);
                    self.send_to_agent(prompt, true, None);
                }
                EditorAction::SendSelectionToAgent => self.run(ctx, Cmd::SendSelectionToAgent),
                EditorAction::Complete { trigger } => {
                    let root = self.active_root();
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        self.lsp.request_completion(doc, root.as_deref(), trigger);
                    }
                }
                EditorAction::Hover { idx } => {
                    let root = self.active_root();
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        self.lsp.request_hover(doc, root.as_deref(), idx);
                    }
                }
                EditorAction::GotoDefinition { idx } => self.goto_definition(idx),
                EditorAction::Run(cmd) => self.run(ctx, cmd),
                EditorAction::StageHunk { line, stage } => self.stage_hunk(line, stage),
                EditorAction::SignatureHelp => {
                    let root = self.active_root();
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

    fn connect_remote(&mut self, text: &str) {
        let Some(spec) = crate::remote::RemoteSpec::parse(text) else {
            self.toast("Use user@host:/path (or ssh://user@host:port/path)", NotifyLevel::Warning);
            return;
        };
        if self.remote_connecting.is_some() {
            return;
        }
        let cache = crate::remote::cache_dir(&settings::config_dir(), &spec);
        self.session.recent_remotes.retain(|r| r != text.trim());
        self.session.recent_remotes.insert(0, text.trim().to_string());
        self.session.recent_remotes.truncate(10);
        self.toast(format!("Connecting to {}…", spec.host), NotifyLevel::Info);
        let w = self.waker.clone();
        let rx = crate::remote::connect(spec.clone(), cache.clone(), move || w());
        self.remote_connecting = Some((spec, cache, rx));
    }

    /// Find out what the host has, so language servers and debug adapters
    /// can run there.
    fn probe_remote(&mut self) {
        let mut programs: Vec<String> = self.lsp.configs().iter().map(|c| c.command.clone()).collect();
        for a in zenith_dap::ADAPTERS {
            programs.extend(a.commands.iter().map(|(p, _)| p.to_string()));
        }
        // Test runners and builds.
        programs.extend(["cargo", "python3", "go", "npm"].map(String::from));
        programs.sort();
        programs.dedup();
        if let Some(r) = &mut self.remote {
            r.start_probe(programs);
            self.lsp.remote_pending = Some(r.cache.clone());
        }
    }

    fn poll_remote(&mut self, ctx: &egui::Context) {
        if let Some((_, _, rx)) = &self.remote_connecting {
            if let Ok(r) = rx.try_recv() {
                let (spec, cache, _) = self.remote_connecting.take().unwrap();
                match r {
                    Ok(since) => {
                        self.remote = None;
                        self.open_workspace(cache.clone());
                        // Source Control runs git on the host.
                        if let Some(g) = Git::open_remote(&cache, &spec) {
                            self.git = Some(g);
                        }
                        self.remote = Some(crate::remote::Session::new(spec.clone(), cache, since, self.waker.clone()));
                        self.probe_remote();
                        self.save_session();
                        self.toast(format!("Opened {} over SSH", spec.label()), NotifyLevel::Info);
                    }
                    Err(e) => self.toast(format!("Couldn't open {}: {e}", spec.label()), NotifyLevel::Error),
                }
            }
        }
        let Some(r) = &mut self.remote else { return };
        match r.take_probed() {
            Some(Ok(())) => {
                let host = r.host.clone();
                self.lsp.remote = host.clone();
                self.lsp.remote_pending = None;
                self.debugger.remote = host.clone();
                self.tests.set_exec(crate::testing::Exec { host });
                self.lsp.restart(&mut self.docs);
                return;
            }
            Some(Err(e)) => {
                self.lsp.remote_pending = None;
                self.toast(format!("Language servers on the host are unavailable: {e}"), NotifyLevel::Warning);
                return;
            }
            None => {}
        }
        // Git on the host may have changed files (pull, checkout, stash): fetch them.
        if self.git.as_mut().is_some_and(|g| std::mem::take(&mut g.host_files_changed)) {
            r.pull_all();
        }
        let (changed, errors) = r.poll();
        for e in errors {
            self.toast(e, NotifyLevel::Error);
        }
        if !changed.is_empty() {
            self.refresh_trees();
            if let Some(ws) = &self.workspace {
                self.index = FileIndex::build(ws);
            }
            self.session.remote_since = self.remote.as_ref().map_or(0, |r| r.since);
        }
        ctx.request_repaint_after(crate::remote::PULL_EVERY);
    }

    fn start_sync(&mut self, op: crate::sync_ui::SyncOp) {
        use crate::sync_ui::SyncOp;
        if self.sync_rx.is_some() {
            self.toast("Settings Sync is already running", NotifyLevel::Info);
            return;
        }
        let config = settings::config_dir();
        if op != SyncOp::Setup && !crate::sync::is_set_up(&config) {
            self.toast("Set up Settings Sync first: Settings → Advanced → Settings Sync", NotifyLevel::Info);
            self.show_settings = true;
            return;
        }
        if op == SyncOp::Push {
            // Push what's in the editor, not an older file.
            self.write_settings();
        }
        let w = self.waker.clone();
        self.sync_rx = Some(crate::sync_ui::start(op, config, self.settings.sync_repo.clone(), move || w()));
        let what = match op {
            SyncOp::Setup => "Connecting to the settings repository…",
            SyncOp::Push => "Pushing settings…",
            SyncOp::Pull => "Pulling settings…",
        };
        self.toast(what, NotifyLevel::Info);
    }

    fn poll_sync(&mut self) {
        use crate::sync_ui::SyncOp;
        let Some(rx) = &self.sync_rx else { return };
        let Ok((op, result)) = rx.try_recv() else { return };
        self.sync_rx = None;
        match result {
            Ok(msg) => {
                self.toast(msg, NotifyLevel::Info);
                match op {
                    // Freshly connected: take the repository's settings if it has any.
                    SyncOp::Setup => self.start_sync(SyncOp::Pull),
                    SyncOp::Pull => {
                        // Keep the repository URL from this machine.
                        let url = self.settings.sync_repo.clone();
                        self.reload_settings();
                        if self.settings.sync_repo.is_empty() && !url.is_empty() {
                            self.settings.sync_repo = url;
                            self.write_settings();
                        }
                        self.load_keymap();
                        self.load_plugins();
                    }
                    SyncOp::Push => {}
                }
            }
            Err(e) => self.toast(format!("Settings Sync: {e}"), NotifyLevel::Error),
        }
    }

    /// Ask GitHub for the latest release (`manual`: also report "up to date").
    fn start_update_check(&mut self, manual: bool) {
        if self.update_rx.is_some() {
            return;
        }
        let w = self.waker.clone();
        self.update_rx = Some((crate::update::check_async(move || w()), manual));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.session.last_update_check = now;
        if manual {
            self.toast("Checking for updates…", NotifyLevel::Info);
        }
    }

    fn start_update_install(&mut self, release: crate::update::Release) {
        if self.install_rx.is_some() {
            return;
        }
        let w = self.waker.clone();
        self.toast(format!("Downloading Zenith {}…", release.version), NotifyLevel::Info);
        self.pending_update_version = release.version.clone();
        self.install_rx = Some(crate::update::install_async(release, move || w()));
    }

    fn poll_update_install(&mut self, ctx: &egui::Context) {
        use crate::update::Outcome;
        let Some(rx) = &self.install_rx else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.install_rx = None;
        match result {
            Ok(Outcome::Restart(exe)) => {
                self.restart_exe = Some(exe);
                self.update_available = None;
                self.dialog = Some(Dialog::Restart(self.pending_update_version.clone()));
            }
            Ok(Outcome::InstallerStarted) => {
                self.toast("The installer is running; Zenith will close.", NotifyLevel::Info);
                self.run(ctx, Cmd::Quit);
            }
            Ok(Outcome::Manual(msg)) => {
                self.toast(msg, NotifyLevel::Info);
            }
            Err(e) => self.toast(format!("Update failed: {e}"), NotifyLevel::Error),
        }
    }

    fn poll_update_check(&mut self) {
        let Some((rx, manual)) = &self.update_rx else { return };
        let manual = *manual;
        let Ok(result) = rx.try_recv() else { return };
        self.update_rx = None;
        let current = env!("CARGO_PKG_VERSION");
        match result {
            Ok(Some(r)) if crate::update::is_newer(&r.version, current) => {
                self.toast(
                    format!("Zenith {} is available (you have {current}). Click ⬆ in the status bar.", r.version),
                    NotifyLevel::Info,
                );
                self.update_available = Some(r);
            }
            Ok(Some(_)) if manual => self.toast(format!("Zenith {current} is the latest version."), NotifyLevel::Info),
            Ok(None) if manual => self.toast("No releases have been published yet.", NotifyLevel::Info),
            Err(e) if manual => self.toast(format!("Couldn't check for updates: {e}"), NotifyLevel::Warning),
            _ => {}
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let p = self.colors.clone();
        let mut cmd = None;
        let mut plugin_click: Option<String> = None;
        let mut update_click = false;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let small = |t: String| RichText::new(t).size(12.0).color(p.text_dim);
            if let Some(b) = self.repo_slot(self.active_repo()).as_ref().and_then(|g| g.branch.as_ref()) {
                let r = ui.add(egui::Label::new(small(format!("⎇ {b}"))).sense(Sense::click()));
                if r.on_hover_text("Switch branch").clicked() {
                    cmd = Some(Cmd::GitSwitchBranch);
                }
            }
            if self.settings.vim_mode {
                if let Some(doc) = self.docs.get(self.active) {
                    let v = &doc.vim;
                    let text = match &v.cmdline {
                        Some(c) => format!(":{c}▏"),
                        None => {
                            let rec =
                                v.macro_rec.as_ref().map(|(r, _)| format!("  recording @{r}")).unwrap_or_default();
                            format!("-- {} --{rec}  {}", v.mode.label(), v.pending_text())
                        }
                    };
                    let color = match v.mode {
                        crate::vim::Mode::Insert => p.success,
                        crate::vim::Mode::Normal => p.accent,
                        _ => p.warning,
                    };
                    ui.label(RichText::new(text.trim_end()).size(12.0).monospace().color(color));
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
            if let Some(r) = &self.remote {
                let (text, color) = match &r.last_error {
                    Some(_) => (format!("SSH {} (offline)", r.spec.host), p.error),
                    None => (format!("SSH {}", r.spec.host), p.accent),
                };
                if ui
                    .add(egui::Label::new(RichText::new(text).size(12.0).color(color)).sense(Sense::click()))
                    .on_hover_text(format!(
                        "Remote folder {}\nSaves go to the host; terminals and agents run there.\nClick to fetch changes now.",
                        r.spec.label()
                    ))
                    .clicked()
                {
                    cmd = Some(Cmd::RemoteSyncNow);
                }
            }
            if let Some(r) = &self.update_available {
                let label = RichText::new(format!("⬆ Update {}", r.version)).size(12.0).color(p.success);
                if ui
                    .add(egui::Label::new(label).sense(Sense::click()))
                    .on_hover_text("A newer Zenith is available — open the release page")
                    .clicked()
                {
                    update_click = true;
                }
            }
            let running_agents = self.ai.tabs.iter().filter(|t| t.is_alive()).count();
            if running_agents > 0 {
                ui.label(RichText::new(format!("✳ {running_agents} agent(s)")).size(12.0).color(p.accent));
            }
            // Plugin items: left ones here, right ones before the file info.
            let mut item_ui = |ui: &mut egui::Ui, (plugin, _): &(String, String), it: &StatusItem| {
                let color = match it.color.as_str() {
                    "accent" => p.accent,
                    "success" => p.success,
                    "warning" => p.warning,
                    "error" => p.error,
                    _ => p.text_dim,
                };
                let mut r =
                    ui.add(
                        egui::Label::new(RichText::new(&it.text).size(12.0).color(color))
                            .sense(if it.command.is_some() { Sense::click() } else { Sense::hover() }),
                    );
                if !it.tooltip.is_empty() {
                    r = r.on_hover_text(format!("{}\n— {plugin}", it.tooltip));
                }
                if r.clicked() {
                    if let Some(c) = &it.command {
                        plugin_click = Some(c.clone());
                    }
                }
            };
            for (k, it) in self.status_items.iter().filter(|(_, i)| !i.right) {
                item_ui(ui, k, it);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(doc) = self.docs.get(self.active) {
                    let pos = doc.buffer.cursor_pos();
                    let lang = doc.language().to_string();
                    let engine = match doc.highlight.tree_sitter_name() {
                        Some(g) => format!("Highlighting: tree-sitter ({g}) — click to change language"),
                        None => "Highlighting: TextMate grammar — click to change language".into(),
                    };
                    if ui.add(egui::Label::new(small(lang)).sense(Sense::click())).on_hover_text(engine).clicked() {
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
                for (k, it) in self.status_items.iter().filter(|(_, i)| i.right) {
                    item_ui(ui, k, it);
                }
            });
        });
        if let Some(c) = cmd {
            self.run(ctx, c);
        }
        if update_click {
            if let Some(r) = self.update_available.clone() {
                self.dialog = Some(Dialog::Update(r));
            }
        }
        if let Some(c) = plugin_click {
            let ectx = self.editor_context();
            if let Err(e) = self.plugins.execute(&c, &ectx) {
                self.toast(format!("{e:#}"), NotifyLevel::Error);
            }
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
        let mut split_new = false;
        let changes_ready = self.changes.base.is_some();
        let changes_n = self.changes.changes.len();
        let mut open_changes = false;
        let split_idx = panel.split_index();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.label(RichText::new(if ai { "AI AGENTS" } else { "TERMINAL" }).size(11.0).strong().color(p.text_dim));
            ui.add_space(8.0);
            let mut clicked = None;
            for (i, t) in panel.tabs.iter().enumerate() {
                let active = i == panel.active || Some(i) == split_idx;
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
                    clicked = Some(i);
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
            }
            if let Some(i) = clicked {
                if Some(i) == split_idx {
                    // Swap the two sides.
                    panel.split = panel.tabs.get(panel.active).map(|t| t.id);
                }
                panel.active = i;
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
                if ai && changes_ready {
                    let n = changes_n;
                    let label = if n > 0 { format!("⧉ Changes ({n})") } else { "⧉ Changes".to_string() };
                    if ui
                        .add(egui::Button::new(label).frame(false))
                        .on_hover_text("Review everything the agents changed since they started")
                        .clicked()
                    {
                        open_changes = true;
                    }
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
                let tip = if split_idx.is_some() { "Unsplit" } else { "Split: show two side by side" };
                if !panel.tabs.is_empty() && ui.add(egui::Button::new("‖").frame(false)).on_hover_text(tip).clicked()
                {
                    if split_idx.is_some() {
                        panel.split = None;
                    } else if ai {
                        // Pair the active agent with the next one.
                        let n = panel.tabs.len();
                        if n > 1 {
                            panel.split = Some(panel.tabs[(panel.active + 1) % n].id);
                        }
                    } else {
                        split_new = true;
                    }
                }
            });
        });
        ui.add_space(2.0);
        if let Some(i) = close {
            if panel.split == Some(panel.tabs[i].id) {
                panel.split = None;
            }
            panel.tabs.remove(i);
            if panel.active >= panel.tabs.len() {
                panel.active = panel.tabs.len().saturating_sub(1);
            }
        }
        let mut focused = false;
        if !panel.tabs.is_empty() {
            let active = panel.active.min(panel.tabs.len() - 1);
            let split = panel.split_index();
            let full = ui.available_rect_before_wrap();
            let mut panes = vec![(active, full)];
            if let Some(si) = split {
                let mid = (full.center().x).round();
                panes = vec![
                    (active, egui::Rect::from_min_max(full.min, egui::pos2(mid - 1.0, full.max.y))),
                    (si, egui::Rect::from_min_max(egui::pos2(mid + 1.0, full.min.y), full.max)),
                ];
                ui.painter().vline(mid, full.y_range(), Stroke::new(1.0_f32, p.border));
            }
            for (i, rect) in panes {
                let tab = &mut panel.tabs[i];
                let want_focus = focus_req == Some(tab.id);
                let ev = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                        terminal_ui(ui, tab, &p, font_size, want_focus)
                    })
                    .inner;
                match ev {
                    Some(TermEvent::Restart) => tab.restart(waker.clone()),
                    Some(TermEvent::OpenLink(link)) => open_link = Some((link, tab.spec.cwd.clone())),
                    None => {}
                }
                focused |= ui.memory(|m| m.has_focus(ui.make_persistent_id(("zenith-term", tab.id))));
                if want_focus {
                    self.focus_terminal = None;
                }
            }
            ui.allocate_rect(full, Sense::hover());
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
        if open_changes {
            self.run(ui.ctx(), Cmd::ReviewAgentChanges);
        }
        if split_new {
            let keep = self.bottom.active;
            let id = self.new_terminal(None);
            self.bottom.split = Some(id);
            self.bottom.active = keep;
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
                    if !name.is_empty() || matches!(kind, InputKind::Breakpoint(..)) {
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
                                    if let Some(r) = &self.remote {
                                        r.remove(&path);
                                    }
                                    self.docs.retain(|d| d.path().is_none_or(|p| !p.starts_with(&path)));
                                    self.active = self.active.min(self.docs.len().saturating_sub(1));
                                    self.refresh_trees();
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
            Dialog::AskAgent { mut question, focus, mut caret, mut pick } => {
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
                    // `@file` suggestions for the token under the caret.
                    let mention = crate::mention::mention_at(&question, caret);
                    let found = match &mention {
                        Some((_, q)) => self.mention_candidates(q),
                        None => vec![],
                    };
                    pick = pick.min(found.len().saturating_sub(1));
                    let mut accept: Option<String> = None;
                    if !found.is_empty() {
                        // Keys go to the list, not the text box.
                        ui.input_mut(|i| {
                            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                                pick = (pick + 1) % found.len();
                            }
                            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                                pick = (pick + found.len() - 1) % found.len();
                            }
                            if i.consume_key(Modifiers::NONE, Key::Tab) || i.consume_key(Modifiers::NONE, Key::Enter) {
                                accept = Some(found[pick].insert.clone());
                            }
                        });
                    }
                    let te_id = egui::Id::new("ask-agent-question");
                    let out = egui::TextEdit::multiline(&mut question)
                        .id(te_id)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("e.g. Why does this panic on empty input?  (@ to mention a file)")
                        .show(ui);
                    let r = out.response;
                    if let Some(cr) = out.cursor_range {
                        caret = cr.primary.ccursor.index;
                    }
                    if focus {
                        r.request_focus();
                    }
                    if !found.is_empty() {
                        egui::Frame::new()
                            .fill(self.colors.bg_elevated)
                            .stroke(Stroke::new(1.0_f32, self.colors.border))
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::same(4))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                for (i, c) in found.iter().enumerate() {
                                    let (dir, name) = c.label.rsplit_once('/').unwrap_or(("", &c.label));
                                    let (icon, icolor) = theme::file_icon(name);
                                    let mut job = egui::text::LayoutJob::default();
                                    let fmt = |size: f32, color: Color32| egui::TextFormat {
                                        font_id: FontId::proportional(size),
                                        color,
                                        ..Default::default()
                                    };
                                    job.append(icon, 0.0, fmt(11.0, icolor));
                                    job.append(name, 6.0, fmt(13.0, self.colors.text));
                                    job.append(dir, 8.0, fmt(11.5, self.colors.text_dim));
                                    if ui.selectable_label(i == pick, job).clicked() {
                                        accept = Some(c.insert.clone());
                                    }
                                }
                                ui.label(
                                    RichText::new("↑↓ choose · Tab/Enter insert").small().color(self.colors.text_faint),
                                );
                            });
                    }
                    if let (Some(insert), Some((at, _))) = (accept, &mention) {
                        let (text, new_caret) = crate::mention::apply(&question, *at, caret, &insert);
                        question = text;
                        caret = new_caret;
                        pick = 0;
                        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), te_id) {
                            let cc = egui::text::CCursor::new(new_caret);
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cc)));
                            st.store(ui.ctx(), te_id);
                        }
                        r.request_focus();
                    }
                    let enter = found.is_empty()
                        && r.has_focus()
                        && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
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
                    next = Some(Dialog::AskAgent { question, focus: false, caret, pick });
                }
            }
            Dialog::Update(r) => {
                let mut install = false;
                window("Update available").show(ctx, |ui| {
                    ui.label(
                        RichText::new(format!("Zenith {} is available", r.version))
                            .size(16.0)
                            .color(self.colors.accent),
                    );
                    ui.label(
                        RichText::new(format!("You have {}.", env!("CARGO_PKG_VERSION"))).color(self.colors.text_dim),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("⬇ Install and restart").color(Color32::WHITE))
                                    .fill(self.colors.accent),
                            )
                            .clicked()
                        {
                            install = true;
                        }
                        if ui.button("Release notes").clicked() {
                            open_url(&r.url);
                        }
                        if ui.button("Later").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                    });
                    ui.label(
                        RichText::new("The download is checked against GitHub's checksum before installing.")
                            .small()
                            .color(self.colors.text_faint),
                    );
                });
                if install {
                    self.start_update_install(r);
                } else if keep {
                    next = Some(Dialog::Update(r));
                }
            }
            Dialog::Restart(version) => {
                let mut restart = false;
                window("Update installed").show(ctx, |ui| {
                    ui.label(format!("Zenith {version} is installed. Restart to use it."));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Restart now").color(Color32::WHITE))
                                    .fill(self.colors.accent),
                            )
                            .clicked()
                        {
                            restart = true;
                        }
                        if ui.button("Later").clicked() {
                            keep = false;
                        }
                    });
                });
                if restart {
                    // The new process starts when this one has really closed (after any unsaved-changes prompt).
                    self.run(ctx, Cmd::Quit);
                } else if keep {
                    next = Some(Dialog::Restart(version));
                } else {
                    self.restart_exe = None;
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
                    if let Some(r) = &self.remote {
                        r.upload(&path);
                    }
                    self.open_file(&path);
                })
            }
            InputKind::NewFolder(dir) => std::fs::create_dir_all(dir.join(&name)).map(|_| {
                if let Some(r) = &self.remote {
                    r.mkdir(&dir.join(&name));
                }
            }),
            InputKind::RemoteFolder => {
                self.connect_remote(&name);
                return;
            }
            InputKind::NewBranch => {
                self.git_switch(&name, true);
                return;
            }
            InputKind::Watch => {
                self.debugger.watches.push(name);
                self.debugger.evaluate_watches();
                self.save_session();
                return;
            }
            InputKind::Breakpoint(id, line, field) => {
                use crate::editor_view::BpField;
                if let Some(doc) = self.docs.iter_mut().find(|d| d.id == *id) {
                    let b = doc.breakpoints.entry(*line).or_default();
                    match field {
                        BpField::Condition => b.condition = name,
                        BpField::Log => b.log = name,
                        BpField::Hit => b.hit = name,
                    }
                    doc.breakpoints_gen += 1;
                }
                return;
            }
            InputKind::RenameSymbol(idx) => {
                let root = self.active_root();
                if let Some(doc) = self.docs.get_mut(self.active) {
                    self.pending_rename = self.lsp.request_rename(doc, root.as_deref(), *idx, &name);
                }
                return;
            }
            InputKind::Rename(path) => {
                let target = path.with_file_name(&name);
                std::fs::rename(path, &target).map(|_| {
                    if let Some(r) = &self.remote {
                        r.rename(path, &target);
                    }
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
        self.refresh_trees();
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
            &self.ws_overrides,
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
            crate::highlight::TREE_SITTER.store(self.settings.tree_sitter, std::sync::atomic::Ordering::Relaxed);
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
        if let Some(op) = c.sync {
            self.start_sync(op);
        }
        if c.open_keybindings {
            self.run(ctx, Cmd::OpenKeybindings);
        }
        if c.open_workspace_file {
            self.show_settings = false;
            self.run(ctx, Cmd::OpenWorkspaceSettings);
        }
        if c.open_file {
            self.show_settings = false;
            self.run(ctx, Cmd::OpenSettingsFile);
        }
    }

    /// One window with every file the agents changed: list, side-by-side
    /// diff of the selected file, keep / revert per file or all at once.
    fn changes_window(&mut self, ctx: &egui::Context) {
        if !self.show_changes {
            return;
        }
        let p = self.colors.clone();
        let font_size = self.settings.editor_font_size * 0.92;
        let mut open = true;
        let mut keep: Option<usize> = None;
        let mut revert: Option<usize> = None;
        let mut keep_all = false;
        let mut revert_all = false;
        let mut open_path: Option<PathBuf> = None;
        let screen = ctx.screen_rect();
        let cs = &mut self.changes;
        let root = cs.root.clone().unwrap_or_default();
        egui::Window::new("✳ Agent changes")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([screen.width() * 0.85, screen.height() * 0.8])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let n = cs.changes.len();
                    ui.label(
                        RichText::new(if cs.base.is_none() {
                            "Taking checkpoint…".to_string()
                        } else if n == 0 {
                            "No changes since the agent started.".to_string()
                        } else {
                            format!("{n} file(s) changed since the agent started")
                        })
                        .color(p.text_dim),
                    );
                    if let Some(e) = &cs.error {
                        ui.label(RichText::new(e).color(p.error));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(n > 0, egui::Button::new(RichText::new("↺ Revert all").color(p.error)))
                            .clicked()
                        {
                            revert_all = true;
                        }
                        if ui
                            .add_enabled(n > 0, egui::Button::new("✓ Keep all"))
                            .on_hover_text("Accept everything and start a new checkpoint")
                            .clicked()
                        {
                            keep_all = true;
                        }
                    });
                });
                ui.separator();
                let avail = ui.available_size();
                ui.horizontal_top(|ui| {
                    // File list.
                    ui.vertical(|ui| {
                        ui.set_width(260.0);
                        ui.set_min_height(avail.y);
                        egui::ScrollArea::vertical().id_salt("changes-list").show(ui, |ui| {
                            for (i, c) in cs.changes.iter().enumerate() {
                                let (rect, resp) = ui.allocate_exact_size(egui::vec2(256.0, 24.0), Sense::click());
                                if i == cs.selected {
                                    ui.painter().rect_filled(rect, 4.0, p.bg_active);
                                } else if resp.hovered() {
                                    ui.painter().rect_filled(rect, 4.0, p.bg_hover);
                                }
                                let (dir, name) = c.rel.rsplit_once('/').unwrap_or(("", &c.rel));
                                let color = match c.status {
                                    'A' => p.success,
                                    'D' => p.error,
                                    _ => p.warning,
                                };
                                let painter = ui.painter().with_clip_rect(rect);
                                let r = painter.text(
                                    egui::pos2(rect.min.x + 8.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    name,
                                    FontId::proportional(13.0),
                                    p.text,
                                );
                                painter.text(
                                    egui::pos2(r.max.x + 6.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    dir,
                                    FontId::proportional(11.0),
                                    p.text_dim,
                                );
                                ui.painter().text(
                                    rect.right_center() - egui::vec2(8.0, 0.0),
                                    Align2::RIGHT_CENTER,
                                    c.status.to_string(),
                                    FontId::monospace(12.0),
                                    color,
                                );
                                if resp.clicked() {
                                    cs.selected = i;
                                }
                            }
                        });
                    });
                    ui.separator();
                    // Diff of the selected file.
                    ui.vertical(|ui| {
                        let Some(c) = cs.changes.get(cs.selected).cloned() else { return };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&c.rel).strong());
                            ui.label(
                                RichText::new(match c.status {
                                    'A' => "new file",
                                    'D' => "deleted",
                                    _ => "modified",
                                })
                                .small()
                                .color(p.text_dim),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button(RichText::new("↺ Revert").color(p.error)).clicked() {
                                    revert = Some(cs.selected);
                                }
                                if ui.button("✓ Keep").clicked() {
                                    keep = Some(cs.selected);
                                }
                                if c.status != 'D' && ui.button("Open file").clicked() {
                                    open_path = Some(root.join(&c.rel));
                                }
                            });
                        });
                        let stale = cs.diff.as_ref().is_none_or(|d| d.0 != c.rel || d.1 != c.new_blob);
                        if stale {
                            let base = cs
                                .base
                                .as_deref()
                                .and_then(|b| crate::changeset::base_bytes(&root, b, &c.rel))
                                .map(|b| String::from_utf8_lossy(&b).replace("\r\n", "\n"))
                                .unwrap_or_default();
                            let cur = std::fs::read(root.join(&c.rel))
                                .map(|b| String::from_utf8_lossy(&b).replace("\r\n", "\n"))
                                .unwrap_or_default();
                            let rows = std::rc::Rc::new(crate::git::side_by_side(&base, &cur));
                            cs.diff = Some((c.rel.clone(), c.new_blob.clone(), rows, base, cur));
                        }
                        if let Some((rel, _, rows, base, cur)) = &cs.diff {
                            let changes: Vec<usize> = rows
                                .iter()
                                .enumerate()
                                .filter(|(i, r)| r.changed && (*i == 0 || !rows[i - 1].changed))
                                .map(|(i, _)| i)
                                .collect();
                            let old: Vec<&str> = base.lines().collect();
                            let new: Vec<&str> = cur.lines().collect();
                            let id = ui.make_persistent_id(("agent-diff", rel));
                            crate::diff_view::rows_ui(ui, id, rows, &changes, &old, &new, &p, font_size, None, false);
                        }
                    });
                });
            });
        if !open {
            self.show_changes = false;
        }
        let mut touched: Vec<PathBuf> = vec![];
        if let Some(i) = keep {
            self.changes.keep(i);
        }
        if let Some(i) = revert {
            match self.changes.revert(i) {
                Ok(p) => touched.push(p),
                Err(e) => self.toast(e, NotifyLevel::Error),
            }
        }
        if revert_all {
            while !self.changes.changes.is_empty() {
                match self.changes.revert(0) {
                    Ok(p) => touched.push(p),
                    Err(e) => {
                        self.toast(e, NotifyLevel::Error);
                        break;
                    }
                }
            }
        }
        if keep_all {
            if let Some(root) = self.changes.root.clone() {
                self.changes.start(root);
            }
        }
        if !touched.is_empty() {
            // Reverted files: open clean buffers follow the disk silently.
            for d in &mut self.docs {
                if d.path().is_some_and(|p| touched.iter().any(|t| t == p)) && !d.buffer.is_dirty() {
                    let _ = d.reload_from_disk();
                    d.review = None;
                }
            }
            self.refresh_trees();
            self.toast(format!("Reverted {} file(s)", touched.len()), NotifyLevel::Info);
        }
        if let Some(path) = open_path {
            self.show_changes = false;
            self.open_file(&path);
        }
    }

    // ------------------------------------------------------------------
    // Debugging
    // ------------------------------------------------------------------

    fn debug_configurations(&mut self) -> (Vec<zenith_dap::LaunchConfig>, Option<String>) {
        if self.debug_configs.is_none() {
            let active = self.docs.get(self.active).and_then(|d| d.path()).map(Path::to_path_buf);
            self.debug_configs = Some(crate::debug::configurations(self.workspace.as_deref(), active.as_deref()));
        }
        self.debug_configs.clone().unwrap_or_default()
    }

    fn start_debugging(&mut self) {
        let Some(root) = self.workspace.clone() else {
            self.toast("Open a folder to debug", NotifyLevel::Info);
            return;
        };
        self.debug_configs = None;
        let (configs, err) = self.debug_configurations();
        if let Some(e) = err {
            self.toast(e, NotifyLevel::Error);
        }
        let Some(cfg) = configs.get(self.debugger.selected.min(configs.len().saturating_sub(1))).cloned() else {
            self.toast("No debug configuration found: add one to .zenith/launch.toml", NotifyLevel::Warning);
            self.sidebar = SidebarView::Debug;
            self.show_sidebar = true;
            return;
        };
        // Unsaved changes would not match what runs.
        let dirty: Vec<usize> = (0..self.docs.len()).filter(|&i| self.docs[i].buffer.is_dirty()).collect();
        for i in dirty {
            if self.docs[i].path().is_some() {
                self.save_doc(i, false);
            }
        }
        self.sidebar = SidebarView::Debug;
        self.show_sidebar = true;
        self.debugger.start(cfg, root);
    }

    /// The function the caret is in: the nearest `fn name(` / `def name(` above.
    fn test_name_at_cursor(&self) -> Option<String> {
        let d = self.docs.get(self.active)?;
        let line = d.buffer.cursor_pos().line;
        (0..=line).rev().find_map(|l| {
            let t = d.buffer.line_text(l);
            let t = t.trim_start();
            let rest = t
                .strip_prefix("fn ")
                .or_else(|| t.split_once(" fn ").map(|(_, r)| r))
                .or_else(|| t.strip_prefix("def "))
                .or_else(|| t.strip_prefix("async def "))?;
            let n: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            (!n.is_empty()).then_some(n)
        })
    }

    /// Run one test under the debugger (breakpoints in the test are hit).
    fn debug_test(&mut self, t: &crate::testing::TestItem) {
        let Some(root) = self.workspace.clone() else { return };
        if self.remote.is_some() && self.debugger.remote.is_none() {
            self.toast("Still connecting to the host…", NotifyLevel::Info);
            return;
        }
        let dirty: Vec<usize> = (0..self.docs.len()).filter(|&i| self.docs[i].buffer.is_dirty()).collect();
        for i in dirty {
            if self.docs[i].path().is_some() {
                self.save_doc(i, false);
            }
        }
        self.sidebar = SidebarView::Debug;
        self.show_sidebar = true;
        self.debugger.start(crate::testing::debug_config(t), root);
    }

    fn clear_debug_lines(&mut self) {
        for d in &mut self.docs {
            d.debug_line = None;
        }
    }

    fn handle_debugger(&mut self) {
        let bps = self.all_breakpoints();
        for ev in self.debugger.poll(&bps) {
            match ev {
                crate::debug::DebugEvent::Show(path, line) => {
                    self.clear_debug_lines();
                    if self.open_file(&path) {
                        if let Some(doc) = self.docs.get_mut(self.active) {
                            doc.debug_line = Some(line);
                            doc.reveal_line = Some(line);
                            let idx = doc.buffer.pos_to_char(zenith_core::Position::new(line, 0));
                            doc.buffer.clear_extra();
                            doc.buffer.set_cursor(idx, false);
                        }
                    }
                }
                crate::debug::DebugEvent::Ended => self.clear_debug_lines(),
                crate::debug::DebugEvent::Message(m) => self.toast(m, NotifyLevel::Warning),
                crate::debug::DebugEvent::HoverValue(id, idx, text) => {
                    if let Some(d) = self.docs.iter_mut().find(|d| d.id == id) {
                        if d.debug_value.as_ref().is_some_and(|(i, _)| *i == idx) {
                            d.debug_value = Some((idx, text));
                        }
                    }
                }
            }
        }
        let paused = self.debugger.stopped_thread().is_some();
        for d in &mut self.docs {
            if d.debug_paused != paused {
                d.debug_paused = paused;
                d.debug_value = None;
            }
        }
        if self.debugger.active() && self.debugger.stopped_thread().is_none() {
            self.clear_debug_lines();
        }
        self.debugger.sync_breakpoints(&bps);
    }

    fn debug_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        use crate::debug::State;
        let (configs, err) = self.debug_configurations();
        let mut cmd: Option<Cmd> = None;
        ui.horizontal(|ui| {
            Self::section_header(ui, p, "RUN AND DEBUG");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("⚙").on_hover_text("Open .zenith/launch.toml").clicked() {
                    cmd = Some(Cmd::OpenLaunchConfig);
                }
                if ui.small_button("⟳").on_hover_text("Reload configurations").clicked() {
                    self.debug_configs = None;
                }
            });
        });
        if let Some(e) = &err {
            ui.label(RichText::new(e).color(p.error).small());
        }
        let state = self.debugger.session.as_ref().map(|s| s.state.clone());
        match &state {
            None => {
                if configs.is_empty() {
                    ui.label(
                        RichText::new(
                            "No configurations found. Zenith detects Cargo binaries and the open Python file; \
                             add others in .zenith/launch.toml.",
                        )
                        .color(p.text_dim),
                    );
                    if ui.button("Create launch.toml").clicked() {
                        cmd = Some(Cmd::OpenLaunchConfig);
                    }
                } else {
                    let sel = self.debugger.selected.min(configs.len() - 1);
                    let mut new_sel = sel;
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new("▶ Start").color(Color32::WHITE)).fill(p.success))
                            .on_hover_text("F5")
                            .clicked()
                        {
                            cmd = Some(Cmd::DebugStart);
                        }
                        egui::ComboBox::from_id_salt("debug-config")
                            .width(ui.available_width() - 8.0)
                            .selected_text(&configs[sel].name)
                            .show_ui(ui, |ui| {
                                for (i, c) in configs.iter().enumerate() {
                                    ui.selectable_value(&mut new_sel, i, &c.name);
                                }
                            });
                    });
                    self.debugger.selected = new_sel;
                    let c = &configs[new_sel];
                    let adapter = zenith_dap::adapter(&c.adapter);
                    let missing = c.command.is_empty() && adapter.is_some_and(|a| a.resolve().is_none());
                    let detail = format!("{} · {}", adapter.map_or(c.adapter.as_str(), |a| a.name), c.program);
                    ui.label(RichText::new(detail).small().color(p.text_dim));
                    if missing {
                        ui.label(
                            RichText::new(format!(
                                "⚠ Adapter not installed. {}",
                                adapter.map_or("", |a| a.install_hint)
                            ))
                            .small()
                            .color(p.warning),
                        );
                    }
                }
            }
            Some(st) => {
                let stopped = matches!(st, State::Stopped { .. });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let b = |ui: &mut egui::Ui, label: &str, tip: &str, enabled: bool| {
                        ui.add_enabled(enabled, egui::Button::new(label).min_size(egui::vec2(28.0, 24.0)))
                            .on_hover_text(tip)
                            .clicked()
                    };
                    if stopped {
                        if b(ui, "▶", "Continue (F5)", true) {
                            cmd = Some(Cmd::DebugStart);
                        }
                    } else if b(ui, "‖", "Pause (F6)", matches!(st, State::Running)) {
                        cmd = Some(Cmd::DebugPause);
                    }
                    if b(ui, "→", "Step Over (F10)", stopped) {
                        cmd = Some(Cmd::DebugStepOver);
                    }
                    if b(ui, "↓", "Step Into (F11)", stopped) {
                        cmd = Some(Cmd::DebugStepInto);
                    }
                    if b(ui, "↑", "Step Out (Shift+F11)", stopped) {
                        cmd = Some(Cmd::DebugStepOut);
                    }
                    if b(ui, "⟲", "Restart (Ctrl+Shift+F5)", true) {
                        cmd = Some(Cmd::DebugRestart);
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("■").color(p.error)).min_size(egui::vec2(28.0, 24.0)))
                        .on_hover_text("Stop (Shift+F5)")
                        .clicked()
                    {
                        cmd = Some(Cmd::DebugStop);
                    }
                });
                let status = match st {
                    State::Building => "Building…".to_string(),
                    State::Starting => "Starting…".to_string(),
                    State::Running => "Running".to_string(),
                    State::Stopped { reason, .. } => format!("Paused on {reason}"),
                };
                let name = self.debugger.session.as_ref().map(|s| s.config.name.clone()).unwrap_or_default();
                ui.label(RichText::new(format!("{name} — {status}")).small().color(if stopped {
                    p.warning
                } else {
                    p.success
                }));
            }
        }
        ui.add_space(4.0);

        let mut select_frame = None;
        let mut toggle = None;
        let mut remove_bp: Option<(PathBuf, usize)> = None;
        let mut open_bp: Option<(PathBuf, usize)> = None;
        let mut eval: Option<String> = None;
        let mut add_watch = false;
        let mut remove_watch: Option<usize> = None;
        let ws = self.workspace.clone();
        let bps = self.all_breakpoints();
        let avail = ui.available_height();
        egui::ScrollArea::vertical().id_salt("debug-scroll").max_height(avail * 0.62).auto_shrink([false, true]).show(
            ui,
            |ui| {
                // Watch expressions (evaluated whenever the debugger stops).
                ui.horizontal(|ui| {
                    ui.label(RichText::new("WATCH").size(11.0).strong().color(p.text_dim));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("+").on_hover_text("Add expression").clicked() {
                            add_watch = true;
                        }
                    });
                });
                for (i, w) in self.debugger.watches.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let mut job = egui::text::LayoutJob::default();
                        let f = |c: Color32, mono: bool| egui::TextFormat {
                            font_id: if mono { FontId::monospace(12.0) } else { FontId::proportional(12.5) },
                            color: c,
                            ..Default::default()
                        };
                        job.append(w, 0.0, f(p.accent, false));
                        match self.debugger.watch_values.get(i).cloned().flatten() {
                            Some(Ok(v)) => {
                                job.append(": ", 0.0, f(p.text_dim, false));
                                job.append(&v.chars().take(120).collect::<String>(), 0.0, f(p.text, true));
                            }
                            Some(Err(e)) => {
                                job.append(": ", 0.0, f(p.text_dim, false));
                                job.append(&e.chars().take(80).collect::<String>(), 0.0, f(p.text_faint, false));
                            }
                            None => {}
                        }
                        ui.add(egui::Label::new(job).truncate());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("✕").clicked() {
                                remove_watch = Some(i);
                            }
                        });
                    });
                }
                if self.debugger.watches.is_empty() {
                    ui.label(RichText::new("Add expressions to watch with +").small().color(p.text_faint));
                }
                ui.add_space(6.0);
                if let Some(s) = &self.debugger.session {
                    if !s.frames.is_empty() {
                        ui.label(RichText::new("VARIABLES").size(11.0).strong().color(p.text_dim));
                        let mut stack: Vec<(usize, &zenith_dap::Variable)> =
                            s.scopes.iter().rev().map(|v| (0usize, v)).collect();
                        let mut shown = 0;
                        while let Some((depth, v)) = stack.pop() {
                            shown += 1;
                            if shown > 400 {
                                break;
                            }
                            let open = s.expanded.contains(&v.reference);
                            ui.horizontal(|ui| {
                                ui.add_space(depth as f32 * 12.0);
                                let arrow = if v.reference > 0 {
                                    if open {
                                        "▾ "
                                    } else {
                                        "▸ "
                                    }
                                } else {
                                    "   "
                                };
                                let mut job = egui::text::LayoutJob::default();
                                let f = |c: Color32, mono: bool| egui::TextFormat {
                                    font_id: if mono { FontId::monospace(12.0) } else { FontId::proportional(12.5) },
                                    color: c,
                                    ..Default::default()
                                };
                                job.append(arrow, 0.0, f(p.text_dim, true));
                                let is_scope = depth == 0 && v.value.is_empty();
                                job.append(&v.name, 0.0, f(if is_scope { p.text } else { p.accent }, false));
                                if !v.value.is_empty() {
                                    job.append(": ", 0.0, f(p.text_dim, false));
                                    let val: String = v.value.chars().take(120).collect();
                                    job.append(&val, 0.0, f(p.text, true));
                                }
                                let r = ui.add(egui::Label::new(job).sense(Sense::click()).truncate());
                                let r = if v.type_name.is_empty() { r } else { r.on_hover_text(&v.type_name) };
                                if r.clicked() && v.reference > 0 {
                                    toggle = Some(v.reference);
                                }
                            });
                            if open {
                                if let Some(kids) = s.children.get(&v.reference) {
                                    for k in kids.iter().rev() {
                                        stack.push((depth + 1, k));
                                    }
                                }
                            }
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("CALL STACK").size(11.0).strong().color(p.text_dim));
                        for (i, f) in s.frames.iter().enumerate() {
                            let file = f
                                .path
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| format!("{}:{}", n.to_string_lossy(), f.line + 1))
                                .unwrap_or_default();
                            // Long generic paths: keep the end, which names the function.
                            let n = f.name.chars().count();
                            let short: String = if n > 34 {
                                format!("…{}", f.name.chars().skip(n - 33).collect::<String>())
                            } else {
                                f.name.clone()
                            };
                            let text = RichText::new(format!("{short}  {file}"))
                                .size(12.5)
                                .color(if f.path.is_some() { p.text } else { p.text_faint });
                            if ui.selectable_label(i == s.frame, text).on_hover_text(&f.name).clicked() {
                                select_frame = Some(i);
                            }
                        }
                        ui.add_space(6.0);
                    }
                }
                ui.label(RichText::new("BREAKPOINTS").size(11.0).strong().color(p.text_dim));
                if bps.is_empty() {
                    ui.label(
                        RichText::new("Click left of a line number (or F9) to add one.").small().color(p.text_faint),
                    );
                }
                for (path, lines) in &bps {
                    let rel = match ws.as_ref().and_then(|w| path.strip_prefix(w).ok()) {
                        Some(r) => r.to_string_lossy().into_owned(),
                        None => path.to_string_lossy().into_owned(),
                    };
                    for (&l, b) in lines {
                        ui.horizontal(|ui| {
                            let (sym, color) = if b.disabled {
                                ("○", p.text_faint)
                            } else if !b.log.trim().is_empty() {
                                ("◆", Color32::from_rgb(0xe5, 0x48, 0x4d))
                            } else {
                                ("●", Color32::from_rgb(0xe5, 0x48, 0x4d))
                            };
                            ui.label(RichText::new(sym).color(color));
                            let mut detail = vec![];
                            if !b.condition.trim().is_empty() {
                                detail.push(format!("if {}", b.condition.trim()));
                            }
                            if !b.hit.trim().is_empty() {
                                detail.push(format!("hits {}", b.hit.trim()));
                            }
                            if !b.log.trim().is_empty() {
                                detail.push(format!("log \"{}\"", b.log.trim()));
                            }
                            let r = ui.add(
                                egui::Label::new(RichText::new(format!("{rel}:{}", l + 1)).size(12.5))
                                    .sense(Sense::click()),
                            );
                            if !detail.is_empty() {
                                ui.label(RichText::new(detail.join(" · ")).size(11.0).color(p.text_dim));
                            }
                            if r.clicked() {
                                open_bp = Some((path.clone(), l));
                            }
                            if ui.small_button("✕").clicked() {
                                remove_bp = Some((path.clone(), l));
                            }
                        });
                    }
                }
            },
        );
        ui.separator();
        ui.label(RichText::new("DEBUG CONSOLE").size(11.0).strong().color(p.text_dim));
        let input_h = 26.0;
        egui::ScrollArea::vertical()
            .id_salt("debug-console")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .max_height((ui.available_height() - input_h - 6.0).max(40.0))
            .show(ui, |ui| {
                for (cat, line) in &self.debugger.output {
                    let color = match cat.as_str() {
                        "stderr" => p.error,
                        "input" => p.accent,
                        "result" => p.success,
                        "console" | "important" => p.text_dim,
                        _ => p.text,
                    };
                    ui.label(RichText::new(line).monospace().size(12.0).color(color));
                }
            });
        let paused = self.debugger.stopped_thread().is_some();
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.debugger.console_input)
                .hint_text(if paused { "Evaluate expression…" } else { "Evaluate (when paused)…" })
                .desired_width(f32::INFINITY),
        );
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let e = std::mem::take(&mut self.debugger.console_input);
            if !e.trim().is_empty() {
                eval = Some(e);
            }
            r.request_focus();
        }

        if let Some(i) = select_frame {
            if let Some(crate::debug::DebugEvent::Show(path, line)) = self.debugger.select_frame(i) {
                self.clear_debug_lines();
                if self.open_file(&path) {
                    if let Some(doc) = self.docs.get_mut(self.active) {
                        doc.debug_line = Some(line);
                        doc.reveal_line = Some(line);
                    }
                }
            }
        }
        if let Some(r) = toggle {
            self.debugger.toggle_var(r);
        }
        if let Some(e) = eval {
            self.debugger.evaluate(e);
        }
        if add_watch {
            self.dialog = Some(Dialog::Input {
                title: "Add watch expression".into(),
                value: String::new(),
                kind: InputKind::Watch,
                focus: true,
            });
        }
        if let Some(i) = remove_watch {
            self.debugger.watches.remove(i);
            if i < self.debugger.watch_values.len() {
                self.debugger.watch_values.remove(i);
            }
            self.save_session();
        }
        if let Some((path, l)) = remove_bp {
            match self.docs.iter_mut().find(|d| d.path() == Some(path.as_path())) {
                Some(d) => d.toggle_breakpoint(l),
                None => {
                    if let Some(set) = self.bp_store.get_mut(&path) {
                        set.remove(&l);
                        if set.is_empty() {
                            self.bp_store.remove(&path);
                        }
                    }
                }
            }
        }
        if let Some((path, l)) = open_bp {
            if self.open_file(&path) {
                if let Some(doc) = self.docs.get_mut(self.active) {
                    doc.goto_line(l);
                }
            }
        }
        if let Some(c) = cmd {
            self.run(ui.ctx(), c);
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
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Restart into the freshly installed version.
        if let Some(exe) = self.restart_exe.take() {
            let args: Vec<String> = std::env::args().skip(1).collect();
            if let Err(e) = std::process::Command::new(&exe).args(args).spawn() {
                log::error!("restart failed: {e}");
            }
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.style_dirty {
            theme::apply_style(ctx, &self.colors, self.settings.ui_font_size);
            self.style_dirty = false;
        }
        self.index.poll();
        for i in &mut self.extra_index {
            i.poll();
        }
        self.handle_plugin_events();
        self.flush_pending_sends();
        self.retry_failed_resumes();
        self.handle_lsp();
        self.handle_git();
        if self.changes.poll() {
            ctx.request_repaint();
        }
        self.handle_debugger();
        self.poll_update_check();
        self.poll_remote(ctx);
        for o in self.market.poll() {
            match o {
                crate::marketplace::Outcome::Reload(msg) => {
                    self.load_plugins();
                    self.toast(msg, NotifyLevel::Info);
                }
                crate::marketplace::Outcome::Error(e) => {
                    self.load_plugins();
                    self.toast(e, NotifyLevel::Error);
                }
            }
        }
        self.poll_update_install(ctx);
        self.poll_sync();
        if self.tests.poll() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let active_path = self.docs.get(self.active).and_then(|d| d.path()).map(Path::to_path_buf);
        if active_path != self.announced_editor {
            self.announced_editor = active_path.clone();
            self.plugins.emit("active_editor_changed", serde_json::json!({ "path": active_path }));
        }
        if self.show_changes
            && self.changes.base.is_some()
            && self.changes_refreshed.is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            self.changes.refresh();
            self.changes_refreshed = Some(Instant::now());
        }

        // Keystrokes reset the caret blink so it stays visible while typing.
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)))) {
            self.cursor_epoch = ctx.input(|i| i.time);
        }

        // Global shortcuts (skipped while a modal dialog/palette has focus).
        if self.dialog.is_none() && !self.palette.open {
            for a in self.shortcuts(ctx) {
                self.handle_palette(ctx, a);
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
        self.changes_window(ctx);
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

/// Two modifier sets name the same chord (Ctrl and Cmd are one on Linux/Windows).
fn same_chord(a: Modifiers, b: Modifiers) -> bool {
    let norm = |m: Modifiers| {
        if cfg!(target_os = "macos") {
            (m.shift, m.alt, m.ctrl, m.command || m.mac_cmd)
        } else {
            (m.shift, m.alt, m.ctrl || m.command, false)
        }
    };
    norm(a) == norm(b)
}

enum MarketAct {
    Install(crate::marketplace::Entry),
    Uninstall(String),
}

/// The MARKETPLACE section of the Plugins view.
fn market_ui(
    m: &mut crate::marketplace::Market,
    ui: &mut egui::Ui,
    p: &Palette,
    registry: &str,
    act: &mut Option<MarketAct>,
    waker: Arc<dyn Fn() + Send + Sync>,
) {
    let url = if registry.trim().is_empty() { crate::marketplace::DEFAULT_REGISTRY } else { registry.trim() };
    m.load(url, false, waker.clone());
    ui.horizontal(|ui| {
        ui.label(RichText::new("MARKETPLACE").size(11.0).color(p.text_dim).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if m.loading() {
                ui.spinner();
            } else if ui.small_button("⟳").on_hover_text("Reload the plugin index").clicked() {
                m.load(url, true, waker.clone());
            }
        });
    });
    ui.add(egui::TextEdit::singleline(&mut m.search).hint_text("Search plugins").desired_width(f32::INFINITY));
    if let Some(e) = &m.error {
        ui.label(RichText::new(e).small().color(p.error));
    }
    let dir = settings::plugins_dir();
    let visible: Vec<crate::marketplace::Entry> = m.visible().into_iter().cloned().collect();
    if visible.is_empty() && !m.loading() && m.error.is_none() {
        ui.label(RichText::new("No plugins found.").small().color(p.text_dim));
    }
    for e in visible {
        let installed = crate::marketplace::installed_version(&dir, &e.id);
        let busy = m.busy_with(&e.id);
        egui::Frame::new()
            .fill(p.bg_elevated)
            .corner_radius(8)
            .inner_margin(10)
            .stroke(Stroke::new(1.0_f32, p.border))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&e.name).strong());
                    ui.label(RichText::new(&e.version).small().color(p.text_dim));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if busy {
                            ui.spinner();
                            return;
                        }
                        match &installed {
                            None => {
                                if ui.button(RichText::new("Install").color(p.accent)).clicked() {
                                    *act = Some(MarketAct::Install(e.clone()));
                                }
                            }
                            Some(v) => {
                                if ui.small_button("Uninstall").clicked() {
                                    *act = Some(MarketAct::Uninstall(e.id.clone()));
                                }
                                if crate::marketplace::is_newer(&e.version, v) {
                                    if ui
                                        .button(RichText::new(format!("Update ({v} → {})", e.version)).color(p.accent))
                                        .clicked()
                                    {
                                        *act = Some(MarketAct::Install(e.clone()));
                                    }
                                } else {
                                    ui.label(RichText::new("✓ Installed").small().color(p.success));
                                }
                            }
                        }
                    });
                });
                if !e.description.is_empty() {
                    ui.label(RichText::new(&e.description).small().color(p.text_dim));
                }
                let mut meta = vec![];
                if !e.author.is_empty() {
                    meta.push(e.author.clone());
                }
                meta.extend(e.tags.iter().map(|t| format!("#{t}")));
                if !meta.is_empty() {
                    ui.label(RichText::new(meta.join("  ")).small().color(p.text_faint));
                }
            });
        ui.add_space(6.0);
    }
}
