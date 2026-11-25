//! Zenith plugin host.
//!
//! Plugins are ordinary programs in any language. Each lives in its own
//! folder with a `plugin.toml` manifest and talks to the editor with
//! newline-delimited JSON over stdin/stdout (see `docs/PLUGINS.md`).
//!
//! Running plugins out-of-process means a slow or crashing plugin can never
//! freeze or crash the editor: all IO happens on background threads and
//! results are polled by the UI once per frame.

use std::collections::HashMap;
#[cfg(feature = "wasm")]
mod wasm;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Command line to start the plugin, e.g. `["python3", "main.py"]`.
    /// Relative paths are resolved inside the plugin folder.
    #[serde(default)]
    pub run: Vec<String>,
    /// Or a WebAssembly module (WASI command, `wasm32-wasip1`) in the plugin
    /// folder, run sandboxed inside the editor: no files, network or
    /// environment — only the protocol messages.
    #[serde(default)]
    pub wasm: String,
    /// What a WASM plugin may do beyond the protocol. `"read_workspace"`:
    /// read (not change) the open folder, visible to it at `/workspace`.
    /// Native plugins (`run`) are ordinary programs and aren't limited.
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub commands: Vec<CommandContribution>,
    /// Editor events the plugin wants: "file_opened", "file_saved".
    #[serde(default)]
    pub events: Vec<String>,
    /// Sidebar panels (an icon in the activity bar each).
    #[serde(default)]
    pub panels: Vec<PanelContribution>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PanelContribution {
    pub id: String,
    pub title: String,
    /// One symbol shown in the activity bar, e.g. "☑".
    #[serde(default)]
    pub icon: String,
}

/// One row of a plugin panel.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct PanelItem {
    pub label: String,
    /// Dimmed text after the label.
    #[serde(default)]
    pub detail: String,
    /// Short symbol before the label.
    #[serde(default)]
    pub icon: String,
    /// Section header (bold, not clickable).
    #[serde(default)]
    pub header: bool,
    #[serde(default)]
    pub indent: usize,
    /// Click opens this file (relative to the workspace or absolute)...
    #[serde(default)]
    pub path: Option<String>,
    /// ...at this 1-based line.
    #[serde(default)]
    pub line: Option<usize>,
    /// ...or runs this plugin command (receives `item` = `id`).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    /// Hover text.
    #[serde(default)]
    pub tooltip: String,
}

/// A button in a panel's header.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PanelButton {
    pub title: String,
    pub command: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandContribution {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub keybinding: Option<String>,
}

/// Snapshot of editor state handed to a plugin command.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditorContext {
    pub workspace: Option<String>,
    pub file: Option<String>,
    pub language: Option<String>,
    pub text: Option<String>,
    pub selection: String,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// Things a plugin can ask the editor to do.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Notify {
        message: String,
        #[serde(default)]
        level: NotifyLevel,
    },
    ReplaceSelection {
        text: String,
    },
    Insert {
        text: String,
    },
    SetText {
        text: String,
    },
    OpenFile {
        path: String,
        #[serde(default)]
        line: Option<usize>,
    },
    RunInTerminal {
        command: String,
    },
    SendToAgent {
        text: String,
        #[serde(default)]
        agent: Option<String>,
    },
    /// Replace the contents of one of the plugin's panels.
    SetPanel {
        panel: String,
        #[serde(default)]
        items: Vec<PanelItem>,
        /// Text shown above the items (e.g. "No TODOs found").
        #[serde(default)]
        message: String,
        #[serde(default)]
        buttons: Vec<PanelButton>,
    },
    /// Add or update a status bar item (empty `text` removes it).
    SetStatus {
        id: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        tooltip: String,
        /// Plugin command run on click.
        #[serde(default)]
        command: Option<String>,
        /// "left" (default) or "right".
        #[serde(default)]
        align: String,
        /// "accent", "success", "warning", "error" or "" (normal).
        #[serde(default)]
        color: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyLevel {
    #[default]
    Info,
    Warning,
    Error,
}

/// A message from a plugin, delivered to the UI thread.
#[derive(Debug, Clone)]
pub struct PluginEvent {
    pub plugin_id: String,
    pub kind: PluginEventKind,
}

#[derive(Debug, Clone)]
pub enum PluginEventKind {
    Action(Action),
    Error(String),
    Log(String),
    Exited,
}

#[derive(Debug, Deserialize)]
struct Incoming {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    process: Option<Running>,
    pub enabled: bool,
}

/// A started plugin: the process and its stdin, stdout, stderr.
type Spawned = (Proc, Box<dyn Write + Send>, Box<dyn Read + Send>, Box<dyn Read + Send>);

enum Proc {
    Native(Child),
    #[cfg(feature = "wasm")]
    Wasm(wasm::WasmProcess),
}

struct Running {
    proc: Proc,
    stdin: Box<dyn Write + Send>,
    next_id: u64,
    /// Host paths ↔ the plugin's view (`/workspace`), for sandboxed plugins.
    map: Option<zenith_core::pathmap::PathMap>,
}

impl Running {
    fn alive(&mut self) -> bool {
        match &mut self.proc {
            Proc::Native(c) => matches!(c.try_wait(), Ok(None)),
            #[cfg(feature = "wasm")]
            Proc::Wasm(w) => w.is_running(),
        }
    }

    fn kill(&mut self) {
        match &mut self.proc {
            Proc::Native(c) => {
                let _ = c.kill();
            }
            #[cfg(feature = "wasm")]
            Proc::Wasm(w) => w.kill(),
        }
    }
}

impl Plugin {
    pub fn is_running(&mut self) -> bool {
        self.process.as_mut().is_some_and(Running::alive)
    }

    /// A WASM plugin that asked to read the workspace.
    pub fn may_read_workspace(&self) -> bool {
        self.is_wasm() && self.manifest.permissions.iter().any(|p| p == "read_workspace")
    }

    /// A WebAssembly (sandboxed) plugin.
    pub fn is_wasm(&self) -> bool {
        !self.manifest.wasm.is_empty()
    }
}

pub struct PluginHost {
    pub plugins: Vec<Plugin>,
    tx: Sender<PluginEvent>,
    rx: Receiver<PluginEvent>,
    /// Map command id -> plugin index.
    commands: HashMap<String, usize>,
    /// Called after a plugin sends something so the UI can wake up.
    waker: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// The open folder (shown to WASM plugins that may read it).
    workspace: Option<PathBuf>,
}

impl Default for PluginHost {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { plugins: vec![], tx, rx, commands: HashMap::new(), waker: None, workspace: None }
    }
}

impl PluginHost {
    /// The open folder; WASM plugins with `read_workspace` see it read-only.
    pub fn set_workspace(&mut self, ws: Option<PathBuf>) {
        self.workspace = ws;
    }

    pub fn set_waker(&mut self, waker: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.waker = Some(waker);
    }

    /// Scan directories for `*/plugin.toml`. Returns load errors (non-fatal).
    pub fn discover(&mut self, dirs: &[PathBuf]) -> Vec<String> {
        let mut errors = vec![];
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let manifest_path = entry.path().join("plugin.toml");
                if !manifest_path.is_file() {
                    continue;
                }
                match load_manifest(&manifest_path) {
                    Ok(m) => {
                        if self.plugins.iter().any(|p| p.manifest.id == m.id) {
                            continue;
                        }
                        self.add(m, entry.path());
                    }
                    Err(e) => errors.push(format!("{}: {e:#}", manifest_path.display())),
                }
            }
        }
        errors
    }

    pub fn add(&mut self, manifest: Manifest, dir: PathBuf) {
        let idx = self.plugins.len();
        for c in &manifest.commands {
            self.commands.insert(c.id.clone(), idx);
        }
        self.plugins.push(Plugin { manifest, dir, process: None, enabled: true });
    }

    /// All commands contributed by enabled plugins.
    pub fn commands(&self) -> Vec<(&Plugin, &CommandContribution)> {
        self.plugins
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| p.manifest.commands.iter().map(move |c| (p, c)))
            .collect()
    }

    pub fn has_command(&self, id: &str) -> bool {
        self.commands.get(id).is_some_and(|&i| self.plugins[i].enabled)
    }

    /// Run a plugin command. Never blocks on the plugin; the answer arrives
    /// later through [`PluginHost::poll`].
    pub fn execute(&mut self, command: &str, ctx: &EditorContext) -> Result<()> {
        self.execute_item(command, ctx, None)
    }

    /// Run a command for a clicked panel item (`item` = the item's id).
    pub fn execute_item(&mut self, command: &str, ctx: &EditorContext, item: Option<&str>) -> Result<()> {
        let idx = *self.commands.get(command).context("unknown plugin command")?;
        self.send(idx, "execute", json!({ "command": command, "context": ctx, "item": item }))
    }

    /// All panels of enabled plugins: (plugin id, panel).
    pub fn panels(&self) -> Vec<(String, PanelContribution)> {
        self.plugins
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| p.manifest.panels.iter().map(move |c| (p.manifest.id.clone(), c.clone())))
            .collect()
    }

    /// Tell a plugin its panel became visible (starting the plugin if needed);
    /// it answers with a `set_panel` action.
    pub fn open_panel(&mut self, plugin_id: &str, panel: &str, ctx: &EditorContext) -> Result<()> {
        let idx = self.plugins.iter().position(|p| p.manifest.id == plugin_id).context("unknown plugin")?;
        self.send(idx, "event", json!({ "event": "panel_opened", "data": { "panel": panel, "context": ctx } }))
    }

    /// Broadcast an editor event to plugins that subscribed to it.
    pub fn emit(&mut self, event: &str, payload: Value) {
        let targets: Vec<usize> = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, p)| p.enabled && p.manifest.events.iter().any(|e| e == event))
            .map(|(i, _)| i)
            .collect();
        for i in targets {
            if let Err(e) = self.send(i, "event", json!({ "event": event, "data": payload })) {
                let _ = self.tx.send(PluginEvent {
                    plugin_id: self.plugins[i].manifest.id.clone(),
                    kind: PluginEventKind::Error(format!("{e:#}")),
                });
            }
        }
    }

    fn send(&mut self, idx: usize, method: &str, params: Value) -> Result<()> {
        self.ensure_started(idx)?;
        let plugin = &mut self.plugins[idx];
        let running = plugin.process.as_mut().unwrap();
        running.next_id += 1;
        let mut msg = json!({ "id": running.next_id, "method": method, "params": params });
        if let Some(m) = &running.map {
            m.outgoing(&mut msg);
        }
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        if let Err(e) = running.stdin.write_all(line.as_bytes()).and_then(|_| running.stdin.flush()) {
            plugin.process = None;
            bail!("plugin `{}` is not responding: {e}", plugin.manifest.id);
        }
        Ok(())
    }

    fn ensure_started(&mut self, idx: usize) -> Result<()> {
        if self.plugins[idx].is_running() {
            return Ok(());
        }
        let plugin = &mut self.plugins[idx];
        if !plugin.enabled {
            bail!("plugin `{}` is disabled", plugin.manifest.id);
        }
        #[allow(unused_mut)]
        let mut map = None;
        let (proc, stdin, stdout, stderr): Spawned = if plugin.is_wasm() {
            #[cfg(feature = "wasm")]
            {
                let path = plugin.dir.join(&plugin.manifest.wasm);
                let ws = self.workspace.as_deref().filter(|_| plugin.may_read_workspace());
                map = ws.map(|w| zenith_core::pathmap::PathMap::new(&w.to_string_lossy(), wasm::GUEST_WORKSPACE, None));
                let (mut w, out, err) = wasm::spawn(&path, &plugin.manifest.id, ws)
                    .with_context(|| format!("failed to start plugin `{}`", plugin.manifest.id))?;
                let stdin = w.stdin.take().context("no stdin")?;
                (Proc::Wasm(w), Box::new(stdin), Box::new(out), Box::new(err))
            }
            #[cfg(not(feature = "wasm"))]
            bail!("plugin `{}` is WebAssembly, and this build has no WebAssembly support", plugin.manifest.id);
        } else {
            Self::spawn_native(plugin)?
        };
        let id = plugin.manifest.id.clone();
        Self::attach(id, proc, stdin, stdout, stderr, self.tx.clone(), self.waker.clone(), plugin, map)
    }

    fn spawn_native(plugin: &Plugin) -> Result<Spawned> {
        let (prog, args) = plugin.manifest.run.split_first().context("empty `run` in plugin.toml")?;
        let resolve = |s: &String| -> String {
            let p = plugin.dir.join(s);
            if p.exists() {
                p.to_string_lossy().into_owned()
            } else {
                s.clone()
            }
        };
        let mut cmd = Command::new(resolve(prog));
        cmd.args(args.iter().map(resolve))
            .current_dir(&plugin.dir)
            .env("ZENITH_PLUGIN_PROTOCOL", PROTOCOL_VERSION.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child =
            cmd.spawn().with_context(|| format!("failed to start plugin `{}` ({prog})", plugin.manifest.id))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        Ok((Proc::Native(child), Box::new(stdin), Box::new(stdout), Box::new(stderr)))
    }

    /// Read the plugin's output in the background and send `initialize`.
    #[allow(clippy::too_many_arguments)]
    fn attach(
        id: String,
        proc: Proc,
        mut stdin: Box<dyn Write + Send>,
        stdout: Box<dyn Read + Send>,
        stderr: Box<dyn Read + Send>,
        tx: Sender<PluginEvent>,
        waker: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
        plugin: &mut Plugin,
        map: Option<zenith_core::pathmap::PathMap>,
    ) -> Result<()> {
        let tx2 = tx.clone();
        let in_map = map.clone();
        thread::Builder::new().name(format!("plugin-{id}")).spawn(move || {
            let wake = || {
                if let Some(w) = &waker {
                    w();
                }
            };
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                // Paths the sandboxed plugin knows (/workspace/...) back to host paths.
                let line = match (&in_map, serde_json::from_str::<Value>(&line)) {
                    (Some(m), Ok(mut v)) => {
                        m.incoming(&mut v);
                        v.to_string()
                    }
                    _ => line,
                };
                for kind in parse_line(&line) {
                    let _ = tx.send(PluginEvent { plugin_id: id.clone(), kind });
                }
                wake();
            }
            let _ = tx.send(PluginEvent { plugin_id: id.clone(), kind: PluginEventKind::Exited });
            wake();
        })?;
        let id = plugin.manifest.id.clone();
        let tx = tx2;
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                let _ = tx.send(PluginEvent { plugin_id: id.clone(), kind: PluginEventKind::Log(line) });
            }
        });

        let init = json!({
            "id": 0,
            "method": "initialize",
            "params": { "protocol": PROTOCOL_VERSION, "editor": "zenith", "version": env!("CARGO_PKG_VERSION") }
        });
        writeln!(stdin, "{init}")?;
        plugin.process = Some(Running { proc, stdin, next_id: 0, map });
        Ok(())
    }

    /// Drain pending plugin messages (call once per frame).
    pub fn poll(&mut self) -> Vec<PluginEvent> {
        self.rx.try_iter().collect()
    }

    pub fn set_enabled(&mut self, idx: usize, enabled: bool) {
        if let Some(p) = self.plugins.get_mut(idx) {
            p.enabled = enabled;
            if !enabled {
                if let Some(mut r) = p.process.take() {
                    r.kill();
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        for p in &mut self.plugins {
            if let Some(mut r) = p.process.take() {
                let _ = writeln!(r.stdin, "{}", json!({"method": "shutdown"}));
                r.kill();
            }
        }
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn load_manifest(path: &Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(path)?;
    let m: Manifest = toml::from_str(&text)?;
    if m.id.trim().is_empty() {
        bail!("plugin id must not be empty");
    }
    if m.run.is_empty() && m.wasm.is_empty() {
        bail!("set `run` (the command that starts the plugin) or `wasm` (a WebAssembly module)");
    }
    Ok(m)
}

/// Parse one JSON line from a plugin into events.
fn parse_line(line: &str) -> Vec<PluginEventKind> {
    let msg: Incoming = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(_) => return vec![PluginEventKind::Log(line.to_string())],
    };
    let mut out = vec![];
    if let Some(err) = msg.error {
        let text = err.as_str().map(str::to_string).unwrap_or_else(|| err.to_string());
        out.push(PluginEventKind::Error(text));
    }
    let actions_from = |v: &Value| -> Vec<PluginEventKind> {
        let list = v.get("actions").cloned().unwrap_or(Value::Null);
        match serde_json::from_value::<Vec<Action>>(list) {
            Ok(a) => a.into_iter().map(PluginEventKind::Action).collect(),
            Err(e) if v.get("actions").is_some() => vec![PluginEventKind::Error(format!("bad actions: {e}"))],
            Err(_) => vec![],
        }
    };
    if let Some(res) = &msg.result {
        out.extend(actions_from(res));
    }
    match msg.method.as_deref() {
        Some("action") => match serde_json::from_value::<Action>(msg.params) {
            Ok(a) => out.push(PluginEventKind::Action(a)),
            Err(e) => out.push(PluginEventKind::Error(format!("bad action: {e}"))),
        },
        Some("log") => out.push(PluginEventKind::Log(
            msg.params.get("message").and_then(|m| m.as_str()).unwrap_or_default().to_string(),
        )),
        _ => {}
    }
    let _ = msg.id;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a command and wait for its first notification.
    #[cfg(feature = "wasm")]
    fn notification(host: &mut PluginHost, command: &str) -> (String, NotifyLevel) {
        host.execute(command, &EditorContext::default()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            for e in host.poll() {
                if let PluginEventKind::Action(Action::Notify { message, level }) = e.kind {
                    return (message, level);
                }
            }
            assert!(std::time::Instant::now() < deadline, "no answer to {command}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// `read_workspace` shows the folder read-only; without it nothing is visible.
    #[cfg(feature = "wasm")]
    #[test]
    fn wasm_workspace_is_read_only() {
        let ws = std::env::temp_dir().join(format!("zenith-wasm-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(ws.join("b.py"), "x = 1\n").unwrap();
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples");
        let mut host = PluginHost::default();
        host.set_workspace(Some(ws.clone()));
        assert!(host.discover(&[examples]).is_empty());
        let (msg, _) = notification(&mut host, "stats.count");
        assert!(msg.starts_with("3 lines") && msg.contains(".rs: 2 lines in 1 files"), "{msg}");
        let (msg, level) = notification(&mut host, "stats.sandbox");
        assert!(msg.contains("write allowed: false") && msg.contains("mkdir allowed: false"), "{msg}");
        assert_eq!(level, NotifyLevel::Info);
        assert!(!ws.join("zenith-sandbox-test.txt").exists() && !ws.join("zenith-sandbox-dir").exists());
        host.shutdown();

        // The same plugin without the permission sees no workspace.
        let i = host.plugins.iter().position(|p| p.manifest.id == "workspace-stats").unwrap();
        host.plugins[i].manifest.permissions.clear();
        let (msg, _) = notification(&mut host, "stats.count");
        assert!(msg.contains("No workspace"), "{msg}");
        host.shutdown();
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The example WebAssembly plugin answers like any other plugin.
    #[cfg(feature = "wasm")]
    #[test]
    fn runs_a_wasm_plugin() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples");
        let mut host = PluginHost::default();
        assert!(host.discover(&[dir]).is_empty());
        let i = host.plugins.iter().position(|p| p.manifest.id == "case-converter").expect("example plugin");
        assert!(host.plugins[i].is_wasm());
        let ctx = EditorContext { selection: "myVariableName\nparseHTTPServer".into(), ..Default::default() };
        host.execute("case.snake", &ctx).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let text = loop {
            let ev = host.poll();
            if let Some(t) = ev.iter().find_map(|e| match &e.kind {
                PluginEventKind::Action(Action::ReplaceSelection { text }) => Some(text.clone()),
                _ => None,
            }) {
                break t;
            }
            assert!(std::time::Instant::now() < deadline, "no answer: {ev:?}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(text, "my_variable_name\nparse_http_server");
        assert!(host.plugins[i].is_running());
        host.shutdown();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!host.plugins[i].is_running());
    }

    #[test]
    fn parses_result_actions() {
        let ev = parse_line(
            r#"{"id":1,"result":{"actions":[{"type":"notify","message":"hi"},{"type":"insert","text":"x"}]}}"#,
        );
        assert_eq!(ev.len(), 2);
        assert!(
            matches!(&ev[0], PluginEventKind::Action(Action::Notify { message, level: NotifyLevel::Info }) if message == "hi")
        );
        assert!(matches!(&ev[1], PluginEventKind::Action(Action::Insert { text }) if text == "x"));
    }

    #[test]
    fn parses_notifications_and_errors() {
        let ev = parse_line(r#"{"method":"action","params":{"type":"open_file","path":"a.rs","line":3}}"#);
        assert!(matches!(&ev[0], PluginEventKind::Action(Action::OpenFile { line: Some(3), .. })));
        let ev = parse_line(r#"{"id":2,"error":"boom"}"#);
        assert!(matches!(&ev[0], PluginEventKind::Error(e) if e == "boom"));
        let ev = parse_line("not json");
        assert!(matches!(&ev[0], PluginEventKind::Log(_)));
    }

    #[test]
    fn parses_status_items() {
        let ev = parse_line(
            r#"{"method":"action","params":{"type":"set_status","id":"wc","text":"42 words","command":"t.wc","align":"right"}}"#,
        );
        assert!(matches!(&ev[0], PluginEventKind::Action(Action::SetStatus { id, text, command: Some(c), align, .. })
            if id == "wc" && text == "42 words" && c == "t.wc" && align == "right"));
    }

    #[test]
    fn parses_panels() {
        let ev = parse_line(
            r#"{"method":"action","params":{"type":"set_panel","panel":"todos","items":[{"label":"a.rs","header":true},{"label":"TODO x","path":"a.rs","line":4}],"buttons":[{"title":"⟳","command":"t.refresh"}]}}"#,
        );
        let PluginEventKind::Action(Action::SetPanel { panel, items, buttons, message }) = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(panel, "todos");
        assert!(items[0].header && items[1].line == Some(4) && message.is_empty());
        assert_eq!(buttons[0].command, "t.refresh");
        let m: Manifest =
            toml::from_str("id='t'\nname='T'\nrun=['x']\n[[panels]]\nid='todos'\ntitle='TODO'\nicon='☑'\n").unwrap();
        assert_eq!(m.panels[0].icon, "☑");
    }

    #[test]
    fn manifest_roundtrip() {
        let m: Manifest = toml::from_str(
            r#"
            id = "demo"
            name = "Demo"
            run = ["python3", "main.py"]
            events = ["file_saved"]
            [[commands]]
            id = "demo.hello"
            title = "Demo: Hello"
        "#,
        )
        .unwrap();
        assert_eq!(m.commands[0].id, "demo.hello");
    }

    #[cfg(unix)]
    #[test]
    fn end_to_end_with_shell_plugin() {
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("zenith-plugin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A tiny plugin: answers every request with a notify action.
        std::fs::write(
            dir.join("p.sh"),
            "while read -r line; do case \"$line\" in *execute*) echo '{\"id\":1,\"result\":{\"actions\":[{\"type\":\"notify\",\"message\":\"pong\"}]}}';; esac; done\n",
        )
        .unwrap();
        let mut host = PluginHost::default();
        host.add(
            Manifest {
                id: "t".into(),
                name: "T".into(),
                version: String::new(),
                description: String::new(),
                run: vec!["sh".into(), "p.sh".into()],
                wasm: String::new(),
                permissions: vec![],
                commands: vec![CommandContribution { id: "t.ping".into(), title: "Ping".into(), keybinding: None }],
                events: vec![],
                panels: vec![],
            },
            dir.clone(),
        );
        host.execute("t.ping", &EditorContext::default()).unwrap();
        let start = Instant::now();
        loop {
            let got = host.poll();
            if got.iter().any(
                |e| matches!(&e.kind, PluginEventKind::Action(Action::Notify { message, .. }) if message == "pong"),
            ) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no reply from plugin");
            thread::sleep(Duration::from_millis(20));
        }
        host.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
