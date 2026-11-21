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
}

/// What the app should do with an event that isn't tied to a document.
pub enum LspOutcome {
    GotoLocation { path: PathBuf, line: u32, character: u32 },
    Message(String),
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
        if !self.clients.contains_key(&cfg.name) {
            if !cfg.is_installed() {
                self.failed.insert(cfg.name.clone());
                return None;
            }
            let root = root.map(Path::to_path_buf).or_else(|| doc.path()?.parent().map(Path::to_path_buf))?;
            match LspClient::start(cfg.clone(), &root, self.tx.clone(), self.waker.clone()) {
                Ok(c) => {
                    self.clients.insert(cfg.name.clone(), c);
                }
                Err(e) => {
                    log::warn!("{e:#}");
                    self.failed.insert(cfg.name.clone());
                    return None;
                }
            }
        }
        self.clients.get_mut(&cfg.name)
    }

    /// Send didOpen/didChange when the buffer changed. `force` skips the debounce.
    pub fn sync(&mut self, doc: &mut Document, root: Option<&Path>, force: bool) {
        let Some(path) = doc.path().map(Path::to_path_buf) else { return };
        let version = doc.buffer.version();
        if doc.lsp_version == Some(version) {
            return;
        }
        if !force && doc.lsp_sent_at.is_some_and(|t| t.elapsed() < CHANGE_DEBOUNCE) {
            (self.waker)();
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

    /// Route server events into documents.
    pub fn poll(&mut self, docs: &mut [Document], pending_definition: &mut Option<u64>) -> Vec<LspOutcome> {
        let mut out = vec![];
        for ev in self.rx.try_iter().collect::<Vec<_>>() {
            match ev {
                LspEvent::Diagnostics { path, diagnostics, .. } => {
                    if let Some(doc) = docs.iter_mut().find(|d| d.path() == Some(path.as_path())) {
                        doc.set_diagnostics(diagnostics);
                    }
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
                LspEvent::Progress { server, message } => match message {
                    Some(m) => {
                        self.progress.insert(server, m);
                    }
                    None => {
                        self.progress.remove(&server);
                    }
                },
                LspEvent::Ready { server } => log::info!("language server {server} ready"),
                LspEvent::Exited { server } => {
                    self.clients.remove(&server);
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
        for (_, mut c) in self.clients.drain() {
            c.shutdown();
        }
        self.progress.clear();
    }
}
