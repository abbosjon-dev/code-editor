//! A small, non-blocking Language Server Protocol client.
//!
//! Each server runs as a child process speaking JSON-RPC over stdio. Writes
//! are cheap and happen on the caller's thread; a reader thread parses
//! responses and notifications and forwards them as [`LspEvent`]s through a
//! channel, so the UI never waits on a server.

mod protocol;
pub mod text;

use std::collections::HashMap;
use std::io::BufReader;
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
    /// The diagnostic as sent by the server (echoed back in code action requests).
    pub raw: Value,
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
    /// Edits elsewhere in the file (e.g. an `import` for this name).
    pub additional_edits: Vec<TextEdit>,
    /// The server's `data`: the item can be resolved for more details
    /// (servers often compute the import edits only then).
    pub resolve_data: Option<Value>,
    /// The item as the server sent it (sent back to resolve it).
    pub raw: Value,
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

/// The signature of the call around the caret.
#[derive(Debug, Clone, PartialEq)]
pub struct SignatureHelp {
    pub label: String,
    /// Char range of the active parameter inside `label`.
    pub active: Option<(usize, usize)>,
    pub documentation: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeAction {
    pub title: String,
    pub kind: String,
    pub preferred: bool,
    pub edits: Vec<(PathBuf, Vec<TextEdit>)>,
    /// `Command` to run via workspace/executeCommand, if any.
    pub command: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub detail: String,
    pub kind: u32,
    pub line: u32,
    pub character: u32,
    pub depth: usize,
    /// File (workspace symbols only).
    pub path: Option<PathBuf>,
}

/// A semantic token (absolute position, UTF-16 columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticToken {
    pub line: u32,
    pub start: u32,
    pub length: u32,
    /// Token type name from the server's legend (e.g. "function").
    pub kind: String,
    pub modifiers: Vec<String>,
}

/// An inlay hint: virtual text shown inside a line (types, parameter names).
#[derive(Debug, Clone, PartialEq)]
pub struct InlayHint {
    pub line: u32,
    /// UTF-16 column.
    pub character: u32,
    pub label: String,
    /// 1 = type, 2 = parameter.
    pub kind: u32,
    pub padding_left: bool,
    pub padding_right: bool,
}

impl Symbol {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            2 => "module",
            5 => "class",
            6 => "method",
            8 => "field",
            9 => "constructor",
            10 => "enum",
            11 => "interface",
            12 => "function",
            13 => "variable",
            14 => "constant",
            22 => "enum member",
            23 => "struct",
            26 => "type param",
            _ => "symbol",
        }
    }
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
    /// The extra edits of a resolved completion item (auto-import).
    CompletionResolved {
        request: u64,
        edits: Vec<TextEdit>,
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
    SignatureHelp {
        request: u64,
        help: Option<SignatureHelp>,
    },
    CodeActions {
        request: u64,
        actions: Vec<CodeAction>,
    },
    Symbols {
        request: u64,
        symbols: Vec<Symbol>,
    },
    WorkspaceSymbols {
        request: u64,
        symbols: Vec<Symbol>,
    },
    InlayHints {
        request: u64,
        hints: Vec<InlayHint>,
    },
    SemanticTokens {
        request: u64,
        tokens: Vec<SemanticToken>,
    },
    /// The server asked us to apply an edit (e.g. after a code action command).
    ApplyEdit {
        edits: Vec<(PathBuf, Vec<TextEdit>)>,
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
    CompletionResolve,
    Hover,
    Definition,
    References,
    Rename,
    Formatting,
    SignatureHelp,
    CodeAction,
    Symbols,
    WorkspaceSymbols,
    InlayHints,
    SemanticTokens,
    Other,
}

struct Shared {
    ready: bool,
    queue: Vec<Value>,
    pending: HashMap<u64, Pending>,
    trigger_chars: Vec<String>,
    /// Semantic token legend from the server: (token types, modifiers).
    legend: Option<(Vec<String>, Vec<String>)>,
}

/// The server's stdin; messages are path-mapped for remote servers.
pub struct Out {
    w: ChildStdin,
    map: Option<zenith_core::pathmap::PathMap>,
}

impl Out {
    fn write(&mut self, msg: &Value) -> std::io::Result<()> {
        match &self.map {
            Some(m) => {
                let mut v = msg.clone();
                m.outgoing(&mut v);
                protocol::write_message(&mut self.w, &v)
            }
            None => protocol::write_message(&mut self.w, msg),
        }
    }
}

/// Run a language server on another machine: `argv` starts it there (e.g.
/// through ssh), `map` translates paths between the local mirror and it.
pub struct RemoteLaunch {
    pub argv: Vec<String>,
    pub map: zenith_core::pathmap::PathMap,
}

pub struct LspClient {
    pub config: ServerConfig,
    stdin: Arc<Mutex<Out>>,
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
        Self::start_with(config, root, tx, waker, None)
    }

    pub fn start_with(
        config: ServerConfig,
        root: &Path,
        tx: Sender<LspEvent>,
        waker: Arc<dyn Fn() + Send + Sync>,
        remote: Option<RemoteLaunch>,
    ) -> Result<Self> {
        let (program, args) = match &remote {
            Some(r) if !r.argv.is_empty() => (PathBuf::from(&r.argv[0]), r.argv[1..].to_vec()),
            _ => {
                (which::which(&config.command).unwrap_or_else(|_| PathBuf::from(&config.command)), config.args.clone())
            }
        };
        let map = remote.map(|r| r.map);
        let is_remote = map.is_some();
        let mut cmd = Command::new(program);
        cmd.args(&args).current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().with_context(|| format!("failed to start language server `{}`", config.command))?;
        let stdin = Arc::new(Mutex::new(Out { w: child.stdin.take().context("no stdin")?, map: map.clone() }));
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let shared = Arc::new(Mutex::new(Shared {
            ready: false,
            queue: vec![],
            pending: HashMap::from([(1, Pending::Initialize)]),
            trigger_chars: vec![],
            legend: None,
        }));

        // Reader thread.
        {
            let name = config.name.clone();
            let stdin = stdin.clone();
            let shared = shared.clone();
            let tx = tx.clone();
            let waker = waker.clone();
            let map = map.clone();
            thread::Builder::new().name(format!("lsp-{name}")).spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(mut msg)) = protocol::read_message(&mut reader) {
                    if let Some(m) = &map {
                        m.incoming(&mut msg);
                    }
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
                "processId": if is_remote { Value::Null } else { json!(std::process::id()) },
                "clientInfo": { "name": "zenith", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() }],
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "window": { "workDoneProgress": true },
                    "workspace": { "configuration": true, "workspaceFolders": true, "applyEdit": true, "symbol": {} },
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false },
                        "completion": {
                            "completionItem": {
                                "snippetSupport": true,
                                "documentationFormat": ["plaintext", "markdown"],
                                "insertReplaceSupport": false,
                                "resolveSupport": { "properties": ["additionalTextEdits", "detail", "documentation"] }
                            },
                            "contextSupport": true
                        },
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "definition": { "linkSupport": true },
                        "references": {},
                        "rename": { "prepareSupport": false },
                        "formatting": {},
                        "signatureHelp": {
                            "signatureInformation": {
                                "documentationFormat": ["plaintext", "markdown"],
                                "parameterInformation": { "labelOffsetSupport": true }
                            }
                        },
                        "codeAction": {
                            "codeActionLiteralSupport": {
                                "codeActionKind": { "valueSet": ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports"] }
                            },
                            "isPreferredSupport": true
                        },
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "inlayHint": { "dynamicRegistration": false },
                        "semanticTokens": {
                            "dynamicRegistration": false,
                            "requests": { "full": true, "range": false },
                            "tokenTypes": ["namespace", "type", "class", "enum", "interface", "struct", "typeParameter", "parameter", "variable", "property", "enumMember", "event", "function", "method", "macro", "keyword", "modifier", "comment", "string", "number", "regexp", "operator", "decorator"],
                            "tokenModifiers": ["declaration", "definition", "readonly", "static", "deprecated", "abstract", "async", "modification", "documentation", "defaultLibrary"],
                            "formats": ["relative"],
                            "multilineTokenSupport": false,
                            "overlappingTokenSupport": false
                        }
                    }
                }
            }
        });
        stdin.lock().unwrap().write(&init)?;
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
            let _ = w.write(&msg);
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

    /// Ask for the rest of a completion item (its import edits).
    pub fn resolve_completion(&mut self, item: Value) -> u64 {
        self.request("completionItem/resolve", item, Pending::CompletionResolve)
    }

    /// Code actions of the given kinds for the whole file (e.g.
    /// `source.organizeImports`).
    pub fn source_actions(&mut self, path: &Path, end: LspPos, kinds: &[&str]) -> u64 {
        let params = json!({
            "textDocument": { "uri": text::path_to_uri(path) },
            "range": { "start": { "line": 0, "character": 0 }, "end": end },
            "context": { "diagnostics": [], "only": kinds, "triggerKind": 1 }
        });
        self.request("textDocument/codeAction", params, Pending::CodeAction)
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

    pub fn signature_help(&mut self, path: &Path, pos: LspPos) -> u64 {
        self.request("textDocument/signatureHelp", Self::doc_pos(path, pos), Pending::SignatureHelp)
    }

    /// Code actions for a range; `diagnostics` are the raw diagnostics overlapping it.
    pub fn code_actions(&mut self, path: &Path, range: LspRange, diagnostics: Vec<Value>) -> u64 {
        let params = json!({
            "textDocument": { "uri": text::path_to_uri(path) },
            "range": range,
            "context": { "diagnostics": diagnostics, "triggerKind": 1 }
        });
        self.request("textDocument/codeAction", params, Pending::CodeAction)
    }

    pub fn document_symbols(&mut self, path: &Path) -> u64 {
        self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": text::path_to_uri(path) } }),
            Pending::Symbols,
        )
    }

    /// Symbols matching `query` across the workspace.
    pub fn workspace_symbols(&mut self, query: &str) -> u64 {
        self.request("workspace/symbol", json!({ "query": query }), Pending::WorkspaceSymbols)
    }

    /// Does the server provide semantic tokens?
    pub fn has_semantic_tokens(&self) -> bool {
        self.shared.lock().is_ok_and(|s| s.legend.is_some())
    }

    pub fn semantic_tokens(&mut self, path: &Path) -> u64 {
        self.request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": text::path_to_uri(path) } }),
            Pending::SemanticTokens,
        )
    }

    /// Inlay hints for lines `start..=end` of a document (`end` must exist).
    pub fn inlay_hints(&mut self, path: &Path, start: u32, end: u32) -> u64 {
        self.request(
            "textDocument/inlayHint",
            json!({
                "textDocument": { "uri": text::path_to_uri(path) },
                "range": { "start": { "line": start, "character": 0 }, "end": { "line": end, "character": 10_000 } }
            }),
            Pending::InlayHints,
        )
    }

    /// Run a server-side command (from a code action).
    pub fn execute_command(&mut self, command: &Value) {
        let params = json!({ "command": command["command"], "arguments": command.get("arguments").cloned().unwrap_or(json!([])) });
        self.request("workspace/executeCommand", params, Pending::Other);
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

fn write_now(stdin: &Arc<Mutex<Out>>, msg: &Value) {
    if let Ok(mut w) = stdin.lock() {
        let _ = w.write(msg);
    }
}

fn handle_message(server: &str, msg: Value, stdin: &Arc<Mutex<Out>>, shared: &Arc<Mutex<Shared>>) -> Vec<LspEvent> {
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
                "workspace/applyEdit" => {
                    let edits = parse_workspace_edit(&msg["params"]["edit"]);
                    out.push(LspEvent::ApplyEdit { edits });
                    json!({ "applied": true })
                }
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
                    let legend = &result["capabilities"]["semanticTokensProvider"]["legend"];
                    let strings = |v: &Value| -> Vec<String> {
                        v.as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default()
                    };
                    let legend = legend
                        .is_object()
                        .then(|| (strings(&legend["tokenTypes"]), strings(&legend["tokenModifiers"])));
                    let queued = {
                        let mut sh = shared.lock().unwrap();
                        sh.ready = true;
                        sh.trigger_chars = chars;
                        sh.legend = legend;
                        std::mem::take(&mut sh.queue)
                    };
                    for m in queued {
                        write_now(stdin, &m);
                    }
                    out.push(LspEvent::Ready { server: server.into() });
                }
                Some(Pending::CompletionResolve) => {
                    out.push(LspEvent::CompletionResolved {
                        request: id,
                        edits: parse_text_edits(&result["additionalTextEdits"]),
                    });
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
                Some(Pending::SignatureHelp) => {
                    out.push(LspEvent::SignatureHelp { request: id, help: parse_signature_help(&result) });
                }
                Some(Pending::CodeAction) => {
                    out.push(LspEvent::CodeActions { request: id, actions: parse_code_actions(&result) });
                }
                Some(Pending::Symbols) => {
                    let mut symbols = vec![];
                    parse_symbols(&result, 0, &mut symbols);
                    out.push(LspEvent::Symbols { request: id, symbols });
                }
                Some(Pending::WorkspaceSymbols) => {
                    out.push(LspEvent::WorkspaceSymbols { request: id, symbols: parse_workspace_symbols(&result) });
                }
                Some(Pending::SemanticTokens) => {
                    let legend = shared.lock().ok().and_then(|s| s.legend.clone()).unwrap_or_default();
                    let tokens = decode_semantic_tokens(&result["data"], &legend.0, &legend.1);
                    out.push(LspEvent::SemanticTokens { request: id, tokens });
                }
                Some(Pending::InlayHints) => {
                    out.push(LspEvent::InlayHints { request: id, hints: parse_inlay_hints(&result) });
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
        raw: v.clone(),
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
                additional_edits: parse_text_edits(&it["additionalTextEdits"]),
                resolve_data: it.get("data").filter(|d| !d.is_null()).cloned(),
                raw: it.clone(),
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

pub(crate) fn parse_signature_help(v: &Value) -> Option<SignatureHelp> {
    let sigs = v["signatures"].as_array()?;
    let active_sig = v["activeSignature"].as_u64().unwrap_or(0) as usize;
    let sig = sigs.get(active_sig).or(sigs.first())?;
    let label = sig["label"].as_str()?.to_string();
    let active_param = sig["activeParameter"].as_u64().or(v["activeParameter"].as_u64()).unwrap_or(0) as usize;
    let active = sig["parameters"].as_array().and_then(|ps| ps.get(active_param)).and_then(|p| match &p["label"] {
        // [start, end] in UTF-16 units of the signature label.
        Value::Array(a) if a.len() == 2 => {
            let (s, e) = (a[0].as_u64()? as u32, a[1].as_u64()? as u32);
            Some((text::utf16_to_char_col(&label, s), text::utf16_to_char_col(&label, e)))
        }
        Value::String(name) => {
            let byte = label.find(name.as_str())?;
            let start = label[..byte].chars().count();
            Some((start, start + name.chars().count()))
        }
        _ => None,
    });
    let doc = match &sig["documentation"] {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        _ => String::new(),
    };
    Some(SignatureHelp { label, active, documentation: text::clean_markdown(&doc) })
}

pub(crate) fn parse_code_actions(v: &Value) -> Vec<CodeAction> {
    let Some(list) = v.as_array() else { return vec![] };
    list.iter()
        .filter_map(|a| {
            let title = a["title"].as_str()?.to_string();
            // A bare Command (no "kind"/"edit", but "command" is a string).
            if a["command"].is_string() {
                return Some(CodeAction {
                    title,
                    kind: String::new(),
                    preferred: false,
                    edits: vec![],
                    command: Some(a.clone()),
                });
            }
            if a.get("disabled").is_some() {
                return None;
            }
            Some(CodeAction {
                title,
                kind: a["kind"].as_str().unwrap_or_default().to_string(),
                preferred: a["isPreferred"].as_bool().unwrap_or(false),
                edits: parse_workspace_edit(&a["edit"]),
                command: a.get("command").filter(|c| c.is_object()).cloned(),
            })
        })
        .collect()
}

/// Flatten DocumentSymbol trees (or SymbolInformation lists) in document order.
pub(crate) fn parse_symbols(v: &Value, depth: usize, out: &mut Vec<Symbol>) {
    let Some(list) = v.as_array() else { return };
    for s in list {
        let Some(name) = s["name"].as_str() else { continue };
        let range =
            parse_range(&s["selectionRange"]).or_else(|| parse_range(&s["location"]["range"])).unwrap_or_default();
        out.push(Symbol {
            name: name.to_string(),
            detail: s["detail"].as_str().or(s["containerName"].as_str()).unwrap_or_default().to_string(),
            kind: s["kind"].as_u64().unwrap_or(0) as u32,
            line: range.start.line,
            character: range.start.character,
            depth,
            path: None,
        });
        parse_symbols(&s["children"], depth + 1, out);
    }
    if depth == 0 {
        // SymbolInformation lists are not ordered.
        out.sort_by_key(|s| (s.line, s.character));
    }
}

/// `workspace/symbol` results (SymbolInformation or WorkspaceSymbol).
pub(crate) fn parse_workspace_symbols(v: &Value) -> Vec<Symbol> {
    let Some(list) = v.as_array() else { return vec![] };
    list.iter()
        .filter_map(|s| {
            let name = s["name"].as_str()?;
            let uri = s["location"]["uri"].as_str()?;
            let range = parse_range(&s["location"]["range"]).unwrap_or_default();
            Some(Symbol {
                name: name.to_string(),
                detail: s["containerName"].as_str().unwrap_or_default().to_string(),
                kind: s["kind"].as_u64().unwrap_or(0) as u32,
                line: range.start.line,
                character: range.start.character,
                depth: 0,
                path: text::uri_to_path(uri),
            })
        })
        .take(500)
        .collect()
}

/// Decode the relative `data` array of a semantic tokens result.
pub(crate) fn decode_semantic_tokens(data: &Value, types: &[String], mods: &[String]) -> Vec<SemanticToken> {
    let Some(d) = data.as_array() else { return vec![] };
    let nums: Vec<u32> = d.iter().map(|x| x.as_u64().unwrap_or(0) as u32).collect();
    let (mut line, mut start) = (0u32, 0u32);
    nums.chunks_exact(5)
        .map(|c| {
            if c[0] > 0 {
                line += c[0];
                start = c[1];
            } else {
                start += c[1];
            }
            SemanticToken {
                line,
                start,
                length: c[2],
                kind: types.get(c[3] as usize).cloned().unwrap_or_default(),
                modifiers: mods
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| c[4] & (1 << i) != 0)
                    .map(|(_, m)| m.clone())
                    .collect(),
            }
        })
        .collect()
}

pub(crate) fn parse_inlay_hints(v: &Value) -> Vec<InlayHint> {
    let Some(list) = v.as_array() else { return vec![] };
    let mut out: Vec<InlayHint> = list
        .iter()
        .filter_map(|h| {
            let label = match &h["label"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["value"].as_str()).collect(),
                _ => return None,
            };
            Some(InlayHint {
                line: h["position"]["line"].as_u64()? as u32,
                character: h["position"]["character"].as_u64()? as u32,
                label: label.trim().to_string(),
                kind: h["kind"].as_u64().unwrap_or(0) as u32,
                padding_left: h["paddingLeft"].as_bool().unwrap_or(false),
                padding_right: h["paddingRight"].as_bool().unwrap_or(false),
            })
        })
        .filter(|h| !h.label.is_empty())
        .collect();
    out.sort_by_key(|h| (h.line, h.character));
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
    fn decodes_semantic_tokens() {
        let types: Vec<String> = ["function", "variable", "macro"].map(String::from).to_vec();
        let mods: Vec<String> = ["declaration", "mutable"].map(String::from).to_vec();
        // line 0 col 3 len 4 function+declaration; same line col 10 len 1 variable+mutable; line 2 col 4 macro.
        let t = decode_semantic_tokens(&json!([0, 3, 4, 0, 1, 0, 7, 1, 1, 2, 2, 4, 6, 2, 0]), &types, &mods);
        assert_eq!(t.len(), 3);
        assert_eq!((t[0].line, t[0].start, t[0].length, t[0].kind.as_str()), (0, 3, 4, "function"));
        assert_eq!(t[0].modifiers, vec!["declaration".to_string()]);
        assert_eq!((t[1].line, t[1].start, t[1].kind.as_str()), (0, 10, "variable"));
        assert_eq!(t[1].modifiers, vec!["mutable".to_string()]);
        assert_eq!((t[2].line, t[2].start, t[2].length, t[2].kind.as_str()), (2, 4, 6, "macro"));
    }

    #[test]
    fn inlay_hints_and_workspace_symbols() {
        let h = parse_inlay_hints(&json!([
            {"position": {"line": 3, "character": 9}, "label": ": i32", "kind": 1},
            {"position": {"line": 1, "character": 4}, "label": [{"value": "x"}, {"value": ":"}], "kind": 2, "paddingRight": true},
            {"position": {"line": 1, "character": 5}, "label": "  "}
        ]));
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].line, h[0].label.as_str(), h[0].padding_right), (1, "x:", true));
        assert_eq!((h[1].character, h[1].kind), (9, 1));
        // A real absolute path for this OS (Windows needs a drive letter).
        let uri = text::path_to_uri(&std::env::temp_dir().join("a.rs"));
        let s = parse_workspace_symbols(&json!([
            {"name": "main", "kind": 12, "containerName": "app", "location": {"uri": uri, "range": {"start": {"line": 4, "character": 3}, "end": {"line": 4, "character": 7}}}}
        ]));
        assert_eq!((s[0].name.as_str(), s[0].line), ("main", 4));
        assert!(s[0].path.as_ref().is_some_and(|p| p.ends_with("a.rs")));
    }

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
    fn parses_signature_help() {
        let v = json!({ "signatures": [{ "label": "fn add(a: i32, b: i32) -> i32",
            "parameters": [{ "label": [7, 13] }, { "label": [15, 21] }] }], "activeSignature": 0, "activeParameter": 1 });
        let h = parse_signature_help(&v).unwrap();
        assert_eq!(&h.label[15..21], "b: i32");
        assert_eq!(h.active, Some((15, 21)));
        let v = json!({ "signatures": [{ "label": "f(x, y)", "parameters": [{ "label": "x" }, { "label": "y" }], "activeParameter": 0 }] });
        assert_eq!(parse_signature_help(&v).unwrap().active, Some((2, 3)));
        assert!(parse_signature_help(&json!(null)).is_none());
    }

    #[test]
    fn parses_code_actions_and_symbols() {
        let uri = text::path_to_uri(&std::env::temp_dir().join("c.rs"));
        let v = json!([
            { "title": "Add missing import", "kind": "quickfix", "isPreferred": true,
              "edit": { "changes": { uri: [{ "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":0} }, "newText": "use x;\n" }] } } },
            { "title": "Run cargo fix", "command": "rust-analyzer.runFix", "arguments": [] },
            { "title": "Disabled", "kind": "refactor", "disabled": { "reason": "no" } }
        ]);
        let a = parse_code_actions(&v);
        assert_eq!(a.len(), 2);
        assert!(a[0].preferred && a[0].edits.len() == 1);
        assert!(a[1].command.is_some());
        let s = json!([{ "name": "Foo", "kind": 23, "range": {}, "selectionRange": { "start": {"line":2,"character":7}, "end": {"line":2,"character":10} },
            "children": [{ "name": "bar", "kind": 8, "selectionRange": { "start": {"line":3,"character":4}, "end": {"line":3,"character":7} } }] }]);
        let mut out = vec![];
        parse_symbols(&s, 0, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].depth, 1);
        assert_eq!(out[0].kind_label(), "struct");
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
        assert!(items[0].additional_edits.is_empty() && items[0].resolve_data.is_none());
    }

    #[test]
    fn completion_import_edits_and_resolve_data() {
        let imp = json!({ "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":0} }, "newText": "use std::collections::HashMap;\n" });
        let v = json!([
            { "label": "HashMap", "additionalTextEdits": [imp] },
            { "label": "BTreeMap", "data": { "imports": [{ "full_import_path": "std::collections::BTreeMap" }] } }
        ]);
        let items = parse_completions(&v);
        assert_eq!(items[0].additional_edits[0].new_text, "use std::collections::HashMap;\n");
        assert!(items[1].resolve_data.is_some());
        assert_eq!(items[1].raw["label"], "BTreeMap");
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
