//! A small, non-blocking Language Server Protocol client.
//!
//! Each server runs as a child process speaking JSON-RPC over stdio. Writes
//! are cheap and happen on the caller's thread; a reader thread parses
//! responses and notifications and forwards them as [`LspEvent`]s through a
//! channel, so the UI never waits on a server.

mod protocol;
mod text;

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use text::{char_col_to_utf16, parse_snippet, utf16_to_char_col};

/// How to start a language server and which languages it serves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// LSP language ids, e.g. ["rust"] or ["typescript", "javascript"].
    pub languages: Vec<String>,
    /// How to install it, shown when missing.
    #[serde(default)]
    pub install_hint: String,
}

impl ServerConfig {
    fn new(name: &str, command: &str, args: &[&str], languages: &[&str], hint: &str) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            languages: languages.iter().map(|s| s.to_string()).collect(),
            install_hint: hint.into(),
        }
    }

    pub fn is_installed(&self) -> bool {
        which::which(&self.command).is_ok()
    }
}

/// Well-known servers, used when installed on PATH.
pub fn default_servers() -> Vec<ServerConfig> {
    vec![
        ServerConfig::new("rust-analyzer", "rust-analyzer", &[], &["rust"], "rustup component add rust-analyzer"),
        ServerConfig::new("pyright", "pyright-langserver", &["--stdio"], &["python"], "npm install -g pyright"),
        ServerConfig::new(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &["typescript", "typescriptreact", "javascript", "javascriptreact"],
            "npm install -g typescript typescript-language-server",
        ),
        ServerConfig::new("gopls", "gopls", &[], &["go"], "go install golang.org/x/tools/gopls@latest"),
        ServerConfig::new("clangd", "clangd", &[], &["c", "cpp"], "install clangd from your package manager / LLVM"),
        ServerConfig::new("zls", "zls", &[], &["zig"], "see github.com/zigtools/zls"),
        ServerConfig::new("jdtls", "jdtls", &[], &["java"], "see github.com/eclipse-jdtls"),
    ]
}

/// Map a syntect syntax name to an LSP language id.
pub fn language_id(syntax_name: &str) -> Option<&'static str> {
    Some(match syntax_name {
        "Rust" => "rust",
        "Python" => "python",
        "TypeScript" => "typescript",
        "TypeScriptReact" => "typescriptreact",
        "JavaScript" | "JavaScript (Babel)" => "javascript",
        "JavaScriptReact" | "JSX" => "javascriptreact",
        "Go" => "go",
        "C" => "c",
        "C++" | "Objective-C++" => "cpp",
        "Zig" => "zig",
        "Java" => "java",
        _ => return None,
    })
}

/// Position in LSP coordinates (UTF-16 code units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct LspPos {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LspRange {
    pub start: LspPos,
    pub end: LspPos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub range: LspRange,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: u32,
    /// Text to insert (snippet placeholders already stripped).
    pub insert_text: String,
    /// Range to replace, if the server specified one.
    pub edit_range: Option<LspRange>,
    /// Char range inside `insert_text` to select after inserting (first snippet tab stop).
    pub select: Option<(usize, usize)>,
    pub filter_text: String,
    pub sort_text: String,
}

impl CompletionItem {
    /// Short tag for the completion kind (function, struct, ...).
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            2 | 3 => "fn",
            4 => "new",
            5 => "field",
            6 => "var",
            7 | 22 => "struct",
            8 => "trait",
            9 => "mod",
            10 => "prop",
            13 => "enum",
            14 => "kw",
            15 => "snip",
            20 => "variant",
            21 => "const",
            25 => "type",
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    pub range: LspRange,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: LspRange,
}

#[derive(Debug, Clone)]
pub enum LspEvent {
    Diagnostics {
        server: String,
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    Completion {
        request: u64,
        items: Vec<CompletionItem>,
    },
    Hover {
        request: u64,
        text: String,
    },
    Definition {
        request: u64,
        locations: Vec<Location>,
    },
    References {
        request: u64,
        locations: Vec<Location>,
    },
    /// Edits grouped per file (rename).
    WorkspaceEdit {
        request: u64,
        edits: Vec<(PathBuf, Vec<TextEdit>)>,
        error: Option<String>,
    },
    /// Edits for the requested document (formatting).
    Formatting {
        request: u64,
        edits: Vec<TextEdit>,
    },
    Progress {
        server: String,
        message: Option<String>,
    },
    Ready {
        server: String,
    },
    Exited {
        server: String,
    },
    Log {
        server: String,
        message: String,
    },
}

#[derive(Clone, Copy)]
enum Pending {
    Initialize,
    Completion,
    Hover,
    Definition,
    References,
    Rename,
    Formatting,
    Other,
}

struct Shared {
    ready: bool,
    queue: Vec<Value>,
    pending: HashMap<u64, Pending>,
    trigger_chars: Vec<String>,
}

pub struct LspClient {
    pub config: ServerConfig,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Child,
    shared: Arc<Mutex<Shared>>,
    next_id: u64,
}

impl LspClient {
    pub fn start(
        config: ServerConfig,
        root: &Path,
        tx: Sender<LspEvent>,
        waker: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let program = which::which(&config.command).unwrap_or_else(|_| PathBuf::from(&config.command));
        let mut cmd = Command::new(program);
        cmd.args(&config.args).current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().with_context(|| format!("failed to start language server `{}`", config.command))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().context("no stdin")?));
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let shared = Arc::new(Mutex::new(Shared {
            ready: false,
            queue: vec![],
            pending: HashMap::from([(1, Pending::Initialize)]),
            trigger_chars: vec![],
        }));

        // Reader thread.
        {
            let name = config.name.clone();
            let stdin = stdin.clone();
            let shared = shared.clone();
            let tx = tx.clone();
            let waker = waker.clone();
            thread::Builder::new().name(format!("lsp-{name}")).spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(msg)) = protocol::read_message(&mut reader) {
                    for ev in handle_message(&name, msg, &stdin, &shared) {
                        let _ = tx.send(ev);
                    }
                    waker();
                }
                let _ = tx.send(LspEvent::Exited { server: name });
                waker();
            })?;
        }
        // Stderr → log.
        {
            let name = config.name.clone();
            thread::spawn(move || {
                use std::io::BufRead;
                for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    log::debug!("[{name}] {line}");
                }
            });
        }

        let root_uri = text::path_to_uri(root);
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "zenith", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() }],
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "window": { "workDoneProgress": true },
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false },
                        "completion": {
                            "completionItem": { "snippetSupport": true, "documentationFormat": ["plaintext", "markdown"], "insertReplaceSupport": false },
                            "contextSupport": true
                        },
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "definition": { "linkSupport": true },
                        "references": {},
                        "rename": { "prepareSupport": false },
                        "formatting": {}
                    }
                }
            }
        });
        protocol::write_message(&mut *stdin.lock().unwrap(), &init)?;
        Ok(Self { config, stdin, child, shared, next_id: 100 })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn trigger_characters(&self) -> Vec<String> {
        self.shared.lock().map(|s| s.trigger_chars.clone()).unwrap_or_default()
    }

    fn send(&self, msg: Value) {
        let mut sh = self.shared.lock().unwrap();
        if !sh.ready {
            sh.queue.push(msg);
            return;
        }
        drop(sh);
        if let Ok(mut w) = self.stdin.lock() {
            let _ = protocol::write_message(&mut *w, &msg);
        }
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn request(&mut self, method: &str, params: Value, kind: Pending) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.shared.lock().unwrap().pending.insert(id, kind);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    pub fn did_open(&self, path: &Path, language_id: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": text::path_to_uri(path), "languageId": language_id, "version": version, "text": text } }),
        );
    }

    pub fn did_change(&self, path: &Path, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": text::path_to_uri(path), "version": version },
                "contentChanges": [{ "text": text }]
            }),
        );
    }

    pub fn did_save(&self, path: &Path) {
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": text::path_to_uri(path) } }));
    }

    pub fn did_close(&self, path: &Path) {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": text::path_to_uri(path) } }));
    }

    fn doc_pos(path: &Path, pos: LspPos) -> Value {
        json!({ "textDocument": { "uri": text::path_to_uri(path) }, "position": pos })
    }

    pub fn completion(&mut self, path: &Path, pos: LspPos, trigger: Option<char>) -> u64 {
        let mut params = Self::doc_pos(path, pos);
        params["context"] = match trigger {
            Some(c) => json!({ "triggerKind": 2, "triggerCharacter": c.to_string() }),
            None => json!({ "triggerKind": 1 }),
        };
        self.request("textDocument/completion", params, Pending::Completion)
    }

    pub fn hover(&mut self, path: &Path, pos: LspPos) -> u64 {
        self.request("textDocument/hover", Self::doc_pos(path, pos), Pending::Hover)
    }

    pub fn definition(&mut self, path: &Path, pos: LspPos) -> u64 {
        self.request("textDocument/definition", Self::doc_pos(path, pos), Pending::Definition)
    }

    pub fn references(&mut self, path: &Path, pos: LspPos) -> u64 {
        let mut params = Self::doc_pos(path, pos);
        params["context"] = json!({ "includeDeclaration": true });
        self.request("textDocument/references", params, Pending::References)
    }

    pub fn rename(&mut self, path: &Path, pos: LspPos, new_name: &str) -> u64 {
        let mut params = Self::doc_pos(path, pos);
        params["newName"] = json!(new_name);
        self.request("textDocument/rename", params, Pending::Rename)
    }

    pub fn formatting(&mut self, path: &Path, tab_size: usize, insert_spaces: bool) -> u64 {
        let params = json!({
            "textDocument": { "uri": text::path_to_uri(path) },
            "options": { "tabSize": tab_size, "insertSpaces": insert_spaces, "trimTrailingWhitespace": true, "insertFinalNewline": true }
        });
        self.request("textDocument/formatting", params, Pending::Formatting)
    }

    pub fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null, Pending::Other);
        self.notify("exit", Value::Null);
        let _ = self.child.kill();
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn write_now(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) {
    if let Ok(mut w) = stdin.lock() {
        let _ = protocol::write_message(&mut *w, msg);
        let _ = w.flush();
    }
}

fn handle_message(
    server: &str,
    msg: Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    shared: &Arc<Mutex<Shared>>,
) -> Vec<LspEvent> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).map(str::to_string);
    let mut out = vec![];
    match (id, method) {
        // Request from the server: answer so it never blocks on us.
        (Some(id), Some(method)) => {
            let result = match method.as_str() {
                "workspace/configuration" => {
                    let n = msg["params"]["items"].as_array().map(|a| a.len()).unwrap_or(0);
                    Value::Array(vec![Value::Null; n])
                }
                "workspace/workspaceFolders" => Value::Null,
                _ => Value::Null,
            };
            write_now(stdin, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
        // Notification.
        (None, Some(method)) => match method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Some(path) = msg["params"]["uri"].as_str().and_then(text::uri_to_path) {
                    let diagnostics = msg["params"]["diagnostics"]
                        .as_array()
                        .map(|a| a.iter().filter_map(parse_diagnostic).collect())
                        .unwrap_or_default();
                    out.push(LspEvent::Diagnostics { server: server.into(), path, diagnostics });
                }
            }
            "$/progress" => {
                let v = &msg["params"]["value"];
                let message = match v["kind"].as_str() {
                    Some("end") => None,
                    _ => {
                        let title = v["title"].as_str().or(v["message"].as_str()).unwrap_or("working");
                        let pct = v["percentage"].as_u64().map(|p| format!(" {p}%")).unwrap_or_default();
                        Some(format!("{title}{pct}"))
                    }
                };
                out.push(LspEvent::Progress { server: server.into(), message });
            }
            "window/logMessage" | "window/showMessage" => {
                out.push(LspEvent::Log {
                    server: server.into(),
                    message: msg["params"]["message"].as_str().unwrap_or_default().into(),
                });
            }
            _ => {}
        },
        // Response.
        (Some(id), None) => {
            let Some(id) = id.as_u64() else { return out };
            let kind = shared.lock().unwrap().pending.remove(&id);
            let result = msg.get("result").cloned().unwrap_or(Value::Null);
            match kind {
                Some(Pending::Initialize) => {
                    let chars = result["capabilities"]["completionProvider"]["triggerCharacters"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    write_now(stdin, &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
                    let queued = {
                        let mut sh = shared.lock().unwrap();
                        sh.ready = true;
                        sh.trigger_chars = chars;
                        std::mem::take(&mut sh.queue)
                    };
                    for m in queued {
                        write_now(stdin, &m);
                    }
                    out.push(LspEvent::Ready { server: server.into() });
                }
                Some(Pending::Completion) => {
                    out.push(LspEvent::Completion { request: id, items: parse_completions(&result) });
                }
                Some(Pending::Hover) => {
                    out.push(LspEvent::Hover { request: id, text: parse_hover(&result) });
                }
                Some(Pending::Definition) => {
                    out.push(LspEvent::Definition { request: id, locations: parse_locations(&result) });
                }
                Some(Pending::References) => {
                    out.push(LspEvent::References { request: id, locations: parse_locations(&result) });
                }
                Some(Pending::Rename) => {
                    let error = msg.get("error").and_then(|e| e["message"].as_str()).map(str::to_string);
                    out.push(LspEvent::WorkspaceEdit { request: id, edits: parse_workspace_edit(&result), error });
                }
                Some(Pending::Formatting) => {
                    out.push(LspEvent::Formatting { request: id, edits: parse_text_edits(&result) });
                }
                Some(Pending::Other) | None => {}
            }
        }
        (None, None) => {}
    }
    out
}

fn parse_range(v: &Value) -> Option<LspRange> {
    serde_json::from_value(v.clone()).ok()
}

fn parse_diagnostic(v: &Value) -> Option<Diagnostic> {
    Some(Diagnostic {
        range: parse_range(&v["range"])?,
        severity: match v["severity"].as_u64().unwrap_or(1) {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Info,
            _ => Severity::Hint,
        },
        message: v["message"].as_str()?.to_string(),
        source: v["source"].as_str().map(str::to_string),
    })
}

pub(crate) fn parse_completions(result: &Value) -> Vec<CompletionItem> {
    let items = match result {
        Value::Array(a) => a.clone(),
        Value::Object(_) => result["items"].as_array().cloned().unwrap_or_default(),
        _ => vec![],
    };
    items
        .iter()
        .filter_map(|it| {
            let label = it["label"].as_str()?.to_string();
            let is_snippet = it["insertTextFormat"].as_u64() == Some(2);
            let (raw, edit_range) = if let Some(te) = it.get("textEdit").filter(|t| t.is_object()) {
                let range = parse_range(&te["range"]).or_else(|| parse_range(&te["replace"]));
                (te["newText"].as_str().unwrap_or(&label).to_string(), range)
            } else {
                (it["insertText"].as_str().unwrap_or(&label).to_string(), None)
            };
            let (insert_text, select) = if is_snippet { text::parse_snippet(&raw) } else { (raw, None) };
            Some(CompletionItem {
                detail: it["detail"].as_str().map(str::to_string),
                kind: it["kind"].as_u64().unwrap_or(0) as u32,
                insert_text,
                edit_range,
                select,
                filter_text: it["filterText"].as_str().unwrap_or(&label).to_string(),
                sort_text: it["sortText"].as_str().unwrap_or(&label).to_string(),
                label,
            })
        })
        .collect()
}

pub(crate) fn parse_hover(result: &Value) -> String {
    fn piece(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o.get("value").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            _ => String::new(),
        }
    }
    let contents = &result["contents"];
    let text = match contents {
        Value::Array(a) => a.iter().map(piece).collect::<Vec<_>>().join("\n\n"),
        other => piece(other),
    };
    text::clean_markdown(&text)
}

pub(crate) fn parse_locations(result: &Value) -> Vec<Location> {
    let list = match result {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![result.clone()],
        _ => vec![],
    };
    list.iter()
        .filter_map(|l| {
            // Location or LocationLink.
            let uri = l["uri"].as_str().or(l["targetUri"].as_str())?;
            let range = parse_range(&l["range"]).or_else(|| parse_range(&l["targetSelectionRange"]))?;
            Some(Location { path: text::uri_to_path(uri)?, range })
        })
        .collect()
}

pub(crate) fn parse_text_edits(v: &Value) -> Vec<TextEdit> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(TextEdit { range: parse_range(&e["range"])?, new_text: e["newText"].as_str()?.to_string() })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a WorkspaceEdit (`changes` map or `documentChanges` list).
pub(crate) fn parse_workspace_edit(v: &Value) -> Vec<(PathBuf, Vec<TextEdit>)> {
    let mut out = vec![];
    if let Some(changes) = v["changes"].as_object() {
        for (uri, edits) in changes {
            if let Some(p) = text::uri_to_path(uri) {
                out.push((p, parse_text_edits(edits)));
            }
        }
    }
    if let Some(dc) = v["documentChanges"].as_array() {
        for d in dc {
            if let Some(p) = d["textDocument"]["uri"].as_str().and_then(text::uri_to_path) {
                out.push((p, parse_text_edits(&d["edits"])));
            }
        }
    }
    out
}

/// Apply LSP text edits (UTF-16 positions) to a string.
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    // Byte offset of each line start.
    let mut line_starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let offset = |p: LspPos| -> usize {
        let Some(&start) = line_starts.get(p.line as usize) else { return text.len() };
        let end = line_starts.get(p.line as usize + 1).copied().unwrap_or(text.len());
        let line = &text[start..end];
        let mut units = 0u32;
        for (i, c) in line.char_indices() {
            if units >= p.character || c == '\n' {
                return start + i;
            }
            units += c.len_utf16() as u32;
        }
        end
    };
    let mut resolved: Vec<(usize, usize, &str)> =
        edits.iter().map(|e| (offset(e.range.start), offset(e.range.end), e.new_text.as_str())).collect();
    resolved.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = text.to_string();
    for (s, e, t) in resolved {
        if s <= e && e <= out.len() {
            out.replace_range(s..e, t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_edits_with_utf16() {
        let text = "let 😀x = 1;\nx + x\n";
        let pos = |l, c| LspPos { line: l, character: c };
        let edits = vec![
            TextEdit { range: LspRange { start: pos(0, 6), end: pos(0, 7) }, new_text: "y".into() },
            TextEdit { range: LspRange { start: pos(1, 0), end: pos(1, 1) }, new_text: "y".into() },
            TextEdit { range: LspRange { start: pos(1, 4), end: pos(1, 5) }, new_text: "y".into() },
        ];
        assert_eq!(apply_text_edits(text, &edits), "let 😀y = 1;\ny + y\n");
    }

    #[test]
    fn parses_workspace_edit_forms() {
        let uri = text::path_to_uri(&std::env::temp_dir().join("w.rs"));
        let e =
            json!({ "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":1} }, "newText": "z" });
        let a = parse_workspace_edit(&json!({ "changes": { uri.clone(): [e.clone()] } }));
        let b = parse_workspace_edit(
            &json!({ "documentChanges": [{ "textDocument": { "uri": uri, "version": 1 }, "edits": [e] }] }),
        );
        assert_eq!(a, b);
        assert_eq!(a[0].1[0].new_text, "z");
    }

    #[test]
    fn parses_completion_list_with_snippets() {
        let v = json!({ "isIncomplete": false, "items": [
            { "label": "push", "kind": 2, "insertTextFormat": 2, "insertText": "push(${1:value})$0", "detail": "fn(&mut self, T)" },
            { "label": "len", "kind": 2, "textEdit": { "range": { "start": {"line":0,"character":2}, "end": {"line":0,"character":4} }, "newText": "len()" } }
        ]});
        let items = parse_completions(&v);
        assert_eq!(items[0].insert_text, "push(value)");
        assert_eq!(items[0].kind_label(), "fn");
        assert_eq!(items[1].edit_range.unwrap().start.character, 2);
    }

    #[test]
    fn parses_hover_and_locations() {
        let h = json!({ "contents": { "kind": "markdown", "value": "```rust\nfn main()\n```\n---\nEntry point" } });
        assert_eq!(parse_hover(&h), "fn main()\n\nEntry point");
        let uri = text::path_to_uri(&std::env::temp_dir().join("a.rs"));
        let l = json!([{ "targetUri": uri, "targetSelectionRange": { "start": {"line":3,"character":1}, "end": {"line":3,"character":5} }, "targetRange": {} }]);
        let locs = parse_locations(&l);
        assert_eq!(locs[0].range.start.line, 3);
        assert!(locs[0].path.ends_with("a.rs"));
    }

    #[test]
    fn language_ids() {
        assert_eq!(language_id("Rust"), Some("rust"));
        assert_eq!(language_id("Markdown"), None);
    }
}
