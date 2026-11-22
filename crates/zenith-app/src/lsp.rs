//! Connects open documents to language servers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zenith_lsp::{default_servers, language_id, LspClient, LspEvent, ServerConfig};

use crate::document::{Completion, Document, Hover};

pub struct LspManager {
    configs: Vec<ServerConfig>,
    clients: HashMap<String, LspClient>,
    /// Servers that failed to start (don't retry every frame).
    failed: HashSet<String>,
    tx: Sender<LspEvent>,
    rx: Receiver<LspEvent>,
    waker: Arc<dyn Fn() + Send + Sync>,
    pub progress: HashMap<String, String>,
    pub enabled: bool,
    /// A delayed repaint is already scheduled for this moment.
    wake_at: Option<Instant>,
    /// Workspace folder of each running client.
    client_roots: HashMap<String, PathBuf>,
    /// Show inlay hints (types, parameter names).
    pub inlay_hints: bool,
}

/// What the app should do with an event that isn't tied to a document.
pub enum LspOutcome {
    GotoLocation {
        path: PathBuf,
        line: u32,
        character: u32,
    },
    Message(String),
    References {
        request: u64,
        locations: Vec<zenith_lsp::Location>,
    },
    WorkspaceEdit {
        request: u64,
        edits: Vec<(PathBuf, Vec<zenith_lsp::TextEdit>)>,
        error: Option<String>,
    },
    Formatting {
        request: u64,
        edits: Vec<zenith_lsp::TextEdit>,
    },
    CodeActions {
        request: u64,
        actions: Vec<zenith_lsp::CodeAction>,
    },
    Symbols {
        request: u64,
        symbols: Vec<zenith_lsp::Symbol>,
    },
    WorkspaceSymbols {
        request: u64,
        symbols: Vec<zenith_lsp::Symbol>,
    },
    ApplyEdit {
        edits: Vec<(PathBuf, Vec<zenith_lsp::TextEdit>)>,
    },
    /// Diagnostics for any file (open or not), for the Problems panel.
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<zenith_lsp::Diagnostic>,
    },
}

const CHANGE_DEBOUNCE: Duration = Duration::from_millis(120);

impl LspManager {
    pub fn new(user: &[ServerConfig], enabled: bool, waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = channel();
        let mut configs = default_servers();
        for u in user {
            if let Some(c) = configs.iter_mut().find(|c| c.name == u.name) {
                *c = u.clone();
            } else {
                configs.insert(0, u.clone());
            }
        }
        Self {
            configs,
            clients: HashMap::new(),
            failed: HashSet::new(),
            tx,
            rx,
            waker,
            progress: HashMap::new(),
            enabled,
            wake_at: None,
            client_roots: HashMap::new(),
            inlay_hints: true,
        }
    }

    pub fn configs(&self) -> &[ServerConfig] {
        &self.configs
    }

    pub fn running(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    fn config_for(&self, lang: &str) -> Option<ServerConfig> {
        self.configs.iter().find(|c| c.languages.iter().any(|l| l == lang)).cloned()
    }

    /// Server for this document's language, started on demand.
    fn client_for(&mut self, doc: &Document, root: Option<&Path>) -> Option<&mut LspClient> {
        if !self.enabled {
            return None;
        }
        let lang = language_id(doc.language())?;
        let cfg = self.config_for(lang)?;
        if self.failed.contains(&cfg.name) {
            return None;
        }
        // One server per (language server, workspace folder): `name · folder`.
        let root = root.map(Path::to_path_buf).or_else(|| doc.path()?.parent().map(Path::to_path_buf))?;
        let folder = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut key = format!("{} · {folder}", cfg.name);
        let mut n = 2;
        while self.client_roots.get(&key).is_some_and(|r| *r != root) {
            key = format!("{} · {folder} ({n})", cfg.name);
            n += 1;
        }
        if self.failed.contains(&key) {
            return None;
        }
        if !self.clients.contains_key(&key) {
            if !cfg.is_installed() {
                self.failed.insert(cfg.name.clone());
                return None;
            }
            let mut cfg = cfg.clone();
            cfg.name = key.clone(); // events are tagged with this name
            match LspClient::start(cfg, &root, self.tx.clone(), self.waker.clone()) {
                Ok(c) => {
                    self.clients.insert(key.clone(), c);
                    self.client_roots.insert(key.clone(), root);
                }
                Err(e) => {
                    log::warn!("{e:#}");
                    self.failed.insert(key);
                    return None;
                }
            }
        }
        self.clients.get_mut(&key)
    }

    /// Send didOpen/didChange when the buffer changed. `force` skips the debounce.
    pub fn sync(&mut self, doc: &mut Document, root: Option<&Path>, force: bool) {
        let Some(path) = doc.path().map(Path::to_path_buf) else { return };
        let version = doc.buffer.version();
        if doc.lsp_version == Some(version) {
            return;
        }
        // Full-text sync costs O(file size): wait longer between syncs for big files.
        let debounce = CHANGE_DEBOUNCE + Duration::from_millis((doc.buffer.len_chars() / 10_000).min(2_000) as u64);
        if !force && doc.lsp_sent_at.is_some_and(|t| t.elapsed() < debounce) {
            // Wake the UI once when the debounce window ends (not every frame).
            let due = doc.lsp_sent_at.unwrap() + debounce;
            if self.wake_at.is_none_or(|w| w < Instant::now() || w > due) {
                self.wake_at = Some(due);
                let waker = self.waker.clone();
                let wait = due.saturating_duration_since(Instant::now());
                std::thread::spawn(move || {
                    std::thread::sleep(wait + Duration::from_millis(5));
                    waker();
                });
            }
            return;
        }
        let Some(lang) = language_id(doc.language()) else { return };
        let first = doc.lsp_version.is_none();
        let Some(client) = self.client_for(doc, root) else { return };
        let text = doc.buffer.text();
        if first {
            client.did_open(&path, lang, version as i64, &text);
        } else {
            client.did_change(&path, version as i64, &text);
        }
        doc.lsp_triggers = client.trigger_characters().iter().filter_map(|s| s.chars().next()).collect();
        if doc.lsp_triggers.is_empty() {
            doc.lsp_triggers = vec!['.'];
        }
        doc.lsp_active = true;
        doc.lsp_version = Some(version);
        doc.lsp_sent_at = Some(Instant::now());
    }

    pub fn did_save(&mut self, doc: &Document, root: Option<&Path>) {
        if let (Some(path), Some(c)) = (doc.path().map(Path::to_path_buf), self.client_for(doc, root)) {
            c.did_save(&path);
        }
    }

    pub fn did_close(&mut self, doc: &Document) {
        if doc.lsp_version.is_none() {
            return;
        }
        if let (Some(path), Some(c)) = (doc.path().map(Path::to_path_buf), self.client_for(doc, None)) {
            c.did_close(&path);
        }
    }

    pub fn request_completion(&mut self, doc: &mut Document, root: Option<&Path>, trigger: Option<char>) {
        self.sync(doc, root, true);
        let Some(path) = doc.path().map(Path::to_path_buf) else { return };
        let pos = doc.to_lsp(doc.buffer.sel.head);
        let anchor = doc.word_start();
        let Some(c) = self.client_for(doc, root) else { return };
        let request = c.completion(&path, pos, trigger);
        doc.completion =
            Some(Completion { request, anchor, items: vec![], filtered: vec![], selected: 0, received: false });
    }

    pub fn request_hover(&mut self, doc: &mut Document, root: Option<&Path>, idx: usize) {
        self.sync(doc, root, true);
        let Some(path) = doc.path().map(Path::to_path_buf) else { return };
        let pos = doc.to_lsp(idx);
        let Some(c) = self.client_for(doc, root) else { return };
        let request = c.hover(&path, pos);
        doc.hover = Some(Hover { request, idx, text: None });
    }

    /// Returns the request id to match against the reply.
    pub fn request_definition(&mut self, doc: &mut Document, root: Option<&Path>, idx: usize) -> Option<u64> {
        self.sync(doc, root, true);
        let path = doc.path()?.to_path_buf();
        let pos = doc.to_lsp(idx);
        let c = self.client_for(doc, root)?;
        Some(c.definition(&path, pos))
    }

    pub fn request_references(&mut self, doc: &mut Document, root: Option<&Path>, idx: usize) -> Option<u64> {
        self.sync(doc, root, true);
        let path = doc.path()?.to_path_buf();
        let pos = doc.to_lsp(idx);
        Some(self.client_for(doc, root)?.references(&path, pos))
    }

    pub fn request_rename(&mut self, doc: &mut Document, root: Option<&Path>, idx: usize, name: &str) -> Option<u64> {
        self.sync(doc, root, true);
        let path = doc.path()?.to_path_buf();
        let pos = doc.to_lsp(idx);
        Some(self.client_for(doc, root)?.rename(&path, pos, name))
    }

    pub fn request_formatting(&mut self, doc: &mut Document, root: Option<&Path>) -> Option<u64> {
        self.sync(doc, root, true);
        let path = doc.path()?.to_path_buf();
        let (tab, spaces) = (doc.buffer.tab_size, doc.buffer.use_spaces);
        Some(self.client_for(doc, root)?.formatting(&path, tab, spaces))
    }

    pub fn request_signature_help(&mut self, doc: &mut Document, root: Option<&Path>) {
        self.sync(doc, root, true);
        let Some(path) = doc.path().map(Path::to_path_buf) else { return };
        let pos = doc.to_lsp(doc.buffer.sel.head);
        let line = doc.buffer.cursor_pos().line;
        let Some(c) = self.client_for(doc, root) else { return };
        let request = c.signature_help(&path, pos);
        let help = doc.signature.take().and_then(|s| s.help);
        doc.signature = Some(crate::document::Signature { request, line, help });
    }

    /// Code actions at the selection (or caret line), with overlapping diagnostics.
    pub fn request_code_actions(&mut self, doc: &mut Document, root: Option<&Path>) -> Option<u64> {
        self.sync(doc, root, true);
        let path = doc.path()?.to_path_buf();
        let (mut a, mut b) = (doc.buffer.sel.start(), doc.buffer.sel.end());
        let line = doc.buffer.char_to_pos(a).line;
        // With no selection, ask about the diagnostic on this line (servers
        // only offer quick fixes for ranges that touch the problem).
        if a == b {
            let on_line: Vec<_> = doc.diagnostics.iter().filter(|d| d.line == line).collect();
            if let Some(d) = on_line.iter().find(|d| d.start <= a && a <= d.end).or(on_line.first()) {
                (a, b) = (d.start, d.end);
            }
        }
        let diags: Vec<serde_json::Value> = doc
            .diagnostics
            .iter()
            .filter(|d| (d.start <= b && d.end >= a) || d.line == line)
            .map(|d| d.raw.clone())
            .collect();
        let range = zenith_lsp::LspRange { start: doc.to_lsp(a), end: doc.to_lsp(b) };
        Some(self.client_for(doc, root)?.code_actions(&path, range, diags))
    }

    /// Ask every running server for workspace symbols matching `query`.
    pub fn request_workspace_symbols(&mut self, query: &str) -> Vec<u64> {
        self.clients.values_mut().map(|c| c.workspace_symbols(query)).collect()
    }

    /// Request inlay hints for a document once its current text reached the
    /// server (and again after new diagnostics, when the server may know more).
    pub fn maybe_request_inlay_hints(&mut self, doc: &mut Document, root: Option<&Path>) {
        if !self.inlay_hints || !doc.lsp_active || doc.inlay_request.is_some() {
            return;
        }
        let v = doc.buffer.version();
        if doc.lsp_version != Some(v) || doc.inlay_version == Some(v) {
            return;
        }
        if doc.inlay_retry.1.is_some_and(|t| t > Instant::now()) {
            return;
        }
        let Some(path) = doc.path().map(Path::to_path_buf) else { return };
        let last = doc.buffer.len_lines().saturating_sub(1) as u32;
        if let Some(c) = self.client_for(doc, root) {
            doc.inlay_request = Some((c.inlay_hints(&path, 0, last), v));
        }
    }

    pub fn request_symbols(&mut self, doc: &mut Document, root: Option<&Path>) -> Option<u64> {
        self.sync(doc, root, true);
        let path = doc.path()?.to_path_buf();
        Some(self.client_for(doc, root)?.document_symbols(&path))
    }

    pub fn execute_command(&mut self, doc: &mut Document, root: Option<&Path>, command: &serde_json::Value) {
        if let Some(c) = self.client_for(doc, root) {
            c.execute_command(command);
        }
    }

    /// Route server events into documents.
    pub fn poll(&mut self, docs: &mut [Document], pending_definition: &mut Option<u64>) -> Vec<LspOutcome> {
        let mut out = vec![];
        for ev in self.rx.try_iter().collect::<Vec<_>>() {
            match ev {
                LspEvent::Diagnostics { path, diagnostics, .. } => {
                    let path = path.canonicalize().unwrap_or(path);
                    if let Some(doc) = docs.iter_mut().find(|d| d.path() == Some(path.as_path())) {
                        doc.set_diagnostics(diagnostics.clone());
                        // The server finished analysing: hints may be complete now.
                        doc.inlay_version = None;
                    }
                    out.push(LspOutcome::Diagnostics { path, diagnostics });
                }
                LspEvent::Completion { request, items } => {
                    for doc in docs.iter_mut() {
                        if let Some(c) = doc.completion.as_mut().filter(|c| c.request == request) {
                            c.items = items;
                            c.received = true;
                            doc.refilter_completion();
                            break;
                        }
                    }
                }
                LspEvent::Hover { request, text } => {
                    for doc in docs.iter_mut() {
                        if let Some(h) = doc.hover.as_mut().filter(|h| h.request == request) {
                            h.text = Some(text);
                            break;
                        }
                    }
                }
                LspEvent::Definition { request, locations } => {
                    if *pending_definition == Some(request) {
                        *pending_definition = None;
                        match locations.first() {
                            Some(l) => out.push(LspOutcome::GotoLocation {
                                path: l.path.clone(),
                                line: l.range.start.line,
                                character: l.range.start.character,
                            }),
                            None => out.push(LspOutcome::Message("No definition found".into())),
                        }
                    }
                }
                LspEvent::References { request, locations } => out.push(LspOutcome::References { request, locations }),
                LspEvent::WorkspaceEdit { request, edits, error } => {
                    out.push(LspOutcome::WorkspaceEdit { request, edits, error })
                }
                LspEvent::Formatting { request, edits } => out.push(LspOutcome::Formatting { request, edits }),
                LspEvent::CodeActions { request, actions } => out.push(LspOutcome::CodeActions { request, actions }),
                LspEvent::Symbols { request, symbols } => out.push(LspOutcome::Symbols { request, symbols }),
                LspEvent::WorkspaceSymbols { request, symbols } => {
                    out.push(LspOutcome::WorkspaceSymbols { request, symbols })
                }
                LspEvent::InlayHints { request, hints } => {
                    if let Some(doc) = docs.iter_mut().find(|d| d.inlay_request.is_some_and(|(r, _)| r == request)) {
                        let (_, v) = doc.inlay_request.take().unwrap_or_default();
                        if v == doc.buffer.version() {
                            let views = hints
                                .iter()
                                .map(|h| {
                                    let line = h.line as usize;
                                    let text = doc.buffer.line_text(line);
                                    let col = zenith_lsp::text::utf16_to_char_col(&text, h.character);
                                    let label = format!(
                                        "{}{}{}",
                                        if h.padding_left { " " } else { "" },
                                        h.label,
                                        if h.padding_right { " " } else { "" }
                                    );
                                    crate::document::InlayView { line, col, label }
                                })
                                .collect();
                            let empty = hints.is_empty();
                            doc.inlay_hints = views;
                            doc.inlay_version = Some(v);
                            if empty && doc.inlay_retry.0 < 8 {
                                // Probably still indexing: ask again a bit later.
                                doc.inlay_retry.0 += 1;
                                let wait = Duration::from_millis(800 * doc.inlay_retry.0 as u64);
                                doc.inlay_retry.1 = Some(Instant::now() + wait);
                                doc.inlay_version = None;
                                let waker = self.waker.clone();
                                std::thread::spawn(move || {
                                    std::thread::sleep(wait + Duration::from_millis(10));
                                    waker();
                                });
                            } else if !empty {
                                doc.inlay_retry = (0, None);
                            }
                        }
                    }
                }
                LspEvent::ApplyEdit { edits } => out.push(LspOutcome::ApplyEdit { edits }),
                LspEvent::SignatureHelp { request, help } => {
                    for doc in docs.iter_mut() {
                        if doc.signature.as_ref().is_some_and(|s| s.request == request) {
                            match help {
                                Some(h) => doc.signature.as_mut().unwrap().help = Some(h),
                                None => doc.signature = None,
                            }
                            break;
                        }
                    }
                }
                LspEvent::Progress { server, message } => match message {
                    Some(m) => {
                        self.progress.insert(server, m);
                    }
                    None => {
                        self.progress.remove(&server);
                        // Indexing finished: hints may be available now.
                        for d in docs.iter_mut() {
                            d.inlay_version = None;
                            d.inlay_retry = (0, None);
                        }
                    }
                },
                LspEvent::Ready { server } => log::info!("language server {server} ready"),
                LspEvent::Exited { server } => {
                    self.clients.remove(&server);
                    self.client_roots.remove(&server);
                    self.progress.remove(&server);
                    self.failed.insert(server.clone());
                    for d in docs.iter_mut() {
                        d.lsp_version = None;
                        d.lsp_active = false;
                    }
                    out.push(LspOutcome::Message(format!("Language server `{server}` stopped")));
                }
                LspEvent::Log { server, message } => log::debug!("[{server}] {message}"),
            }
        }
        out
    }

    /// Restart everything (e.g. after changing settings or opening a new folder).
    pub fn restart(&mut self, docs: &mut [Document]) {
        self.shutdown();
        self.failed.clear();
        for d in docs {
            d.lsp_version = None;
            d.lsp_active = false;
            d.diagnostics.clear();
        }
    }

    pub fn shutdown(&mut self) {
        self.client_roots.clear();
        for (_, mut c) in self.clients.drain() {
            c.shutdown();
        }
        self.progress.clear();
    }
}
