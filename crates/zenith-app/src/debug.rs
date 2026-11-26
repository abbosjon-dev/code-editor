//! Debugger front end: runs a debug session over [`zenith_dap`] and keeps
//! the state the Run & Debug panel shows (call stack, variables, console).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};
use zenith_dap::{DapClient, DapMessage, LaunchConfig, StackFrame, Variable};

/// Options of one breakpoint (all empty = a plain breakpoint).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bp {
    /// Stop only when this expression is true.
    pub condition: String,
    /// Logpoint: print this message (`{expr}` is interpolated) instead of stopping.
    pub log: String,
    /// Stop after this many hits (e.g. "5", ">= 3").
    pub hit: String,
    pub disabled: bool,
}

impl Bp {
    pub fn is_plain(&self) -> bool {
        *self == Bp::default()
    }
}

/// Breakpoints of one file: 0-based line → options.
pub type Breakpoints = BTreeMap<usize, Bp>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Building,
    Starting,
    Running,
    Stopped { thread: i64, reason: String },
}

/// What the app must do after polling.
pub enum DebugEvent {
    /// Show this location (the debugger stopped / a frame was selected).
    Show(PathBuf, usize),
    /// The session ended: clear the stop markers.
    Ended,
    Message(String),
    /// Value of an expression under the mouse (document id, char index, text).
    HoverValue(u64, usize, String),
}

#[derive(Debug)]
enum Req {
    Initialize,
    Launch,
    SetBreakpoints,
    ConfigurationDone,
    StackTrace,
    Scopes,
    Variables(i64),
    Evaluate,
    Watch(usize),
    Hover(u64, usize, String),
    Other,
}

pub struct Session {
    client: Option<DapClient>,
    pub config: LaunchConfig,
    root: PathBuf,
    pub state: State,
    pending: HashMap<i64, Req>,
    pub frames: Vec<StackFrame>,
    pub frame: usize,
    /// Scopes of the selected frame, and the children of expanded variables.
    pub scopes: Vec<Variable>,
    pub children: HashMap<i64, Vec<Variable>>,
    pub expanded: HashSet<i64>,
    configured: bool,
    /// `initialized` arrived at; configuration waits for the launch
    /// response (gdb) or a short timeout (debugpy answers launch later).
    initialized_at: Option<std::time::Instant>,
    launched: bool,
}

pub struct Debugger {
    /// Folder opened over SSH: adapters and builds run on the host.
    pub remote: Option<crate::remote::HostInfo>,
    pub session: Option<Session>,
    pub output: Vec<(String, String)>,
    pub console_input: String,
    tx: Sender<DapMessage>,
    rx: Receiver<DapMessage>,
    build_rx: Option<Receiver<Result<(), String>>>,
    waker: Arc<dyn Fn() + Send + Sync>,
    /// Breakpoints per file as last sent to the adapter.
    sent: HashMap<PathBuf, Breakpoints>,
    /// Selected launch configuration (index into the current list).
    pub selected: usize,
    /// The last output line has no newline yet.
    open_line: bool,
    /// Watch expressions and their values in the selected frame.
    pub watches: Vec<String>,
    pub watch_values: Vec<Option<Result<String, String>>>,
}

const MAX_OUTPUT: usize = 2000;

impl Debugger {
    pub fn new(waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = channel();
        Self {
            remote: None,
            session: None,
            output: vec![],
            console_input: String::new(),
            tx,
            rx,
            build_rx: None,
            waker,
            sent: HashMap::new(),
            selected: 0,
            open_line: false,
            watches: vec![],
            watch_values: vec![],
        }
    }

    pub fn active(&self) -> bool {
        self.session.is_some()
    }

    pub fn stopped_thread(&self) -> Option<i64> {
        match self.session.as_ref()?.state {
            State::Stopped { thread, .. } => Some(thread),
            _ => None,
        }
    }

    fn log(&mut self, category: &str, text: impl Into<String>) {
        let text = text.into();
        let mut rest = text.as_str();
        // Output can arrive in pieces: continue an unfinished line first.
        if self.open_line && self.output.last().is_some_and(|(c, _)| c == category) {
            let (head, tail) = match rest.find('\n') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            if let Some(last) = self.output.last_mut() {
                last.1.push_str(head);
            }
            match tail {
                None => return,
                Some(t) => rest = t,
            }
        }
        self.open_line = false;
        if rest.is_empty() {
            return;
        }
        for line in rest.trim_end_matches('\n').split('\n') {
            self.output.push((category.to_string(), line.to_string()));
        }
        self.open_line = !rest.ends_with('\n');
        if self.output.len() > MAX_OUTPUT {
            self.output.drain(..self.output.len() - MAX_OUTPUT);
        }
    }

    /// A complete line of Zenith's own (not program output).
    fn log_line(&mut self, category: &str, text: impl Into<String>) {
        let mut t = text.into();
        t.push('\n');
        self.open_line = false;
        self.log(category, t);
    }

    /// Start debugging `cfg` (building first when it has a build command).
    pub fn start(&mut self, cfg: LaunchConfig, root: PathBuf) {
        self.stop();
        self.output.clear();
        self.sent.clear();
        let building = !cfg.build.trim().is_empty();
        self.session = Some(Session {
            client: None,
            config: cfg.clone(),
            root: root.clone(),
            state: if building { State::Building } else { State::Starting },
            pending: HashMap::new(),
            frames: vec![],
            frame: 0,
            scopes: vec![],
            children: HashMap::new(),
            expanded: HashSet::new(),
            configured: false,
            initialized_at: None,
            launched: false,
        });
        if building {
            self.log_line("console", format!("> {}", cfg.build));
            let (tx, rx) = channel();
            let out_tx = self.tx.clone();
            let waker = self.waker.clone();
            // On a remote folder the build runs on the host.
            let argv = self.remote.as_ref().filter(|h| root.starts_with(&h.cache)).map(|h| h.shell(&root, &cfg.build));
            thread::spawn(move || {
                let r = run_build(&cfg.build, argv, &root, &out_tx);
                let _ = tx.send(r);
                waker();
            });
            self.build_rx = Some(rx);
        } else {
            self.launch_adapter();
        }
    }

    fn launch_adapter(&mut self) {
        let Some(s) = &mut self.session else { return };
        let host = self.remote.as_ref().filter(|h| s.root.starts_with(&h.cache));
        let cmd = if !s.config.command.is_empty() {
            Some((s.config.command[0].clone(), s.config.command[1..].to_vec()))
        } else if let Some(h) = host {
            // The first of the adapter's commands the host has.
            zenith_dap::adapter(&s.config.adapter).and_then(|a| {
                a.commands
                    .iter()
                    .find(|(p, _)| h.found.contains_key(*p))
                    .map(|(p, args)| (p.to_string(), args.iter().map(|x| x.to_string()).collect()))
            })
        } else {
            zenith_dap::adapter(&s.config.adapter).and_then(|a| a.resolve())
        };
        // Remote: start it through ssh and translate paths.
        let (cmd, map) = match (cmd, host) {
            (Some((p, a)), Some(h)) => {
                let argv = h.command(&s.root, &p, &a);
                (Some((argv[0].clone(), argv[1..].to_vec())), h.path_map(&s.root))
            }
            (c, _) => (c, None),
        };
        let Some((program, args)) = cmd else {
            let hint = zenith_dap::adapter(&s.config.adapter).map(|a| a.install_hint).unwrap_or("check launch.toml");
            let msg = format!("Debug adapter `{}` not found. {hint}", s.config.adapter);
            self.session = None;
            self.log_line("stderr", msg);
            return;
        };
        match DapClient::start_with(&program, &args, &s.root, self.tx.clone(), self.waker.clone(), map) {
            Ok(mut c) => {
                let seq = c.request(
                    "initialize",
                    json!({
                        "clientID": "zenith", "clientName": "Zenith", "adapterID": s.config.adapter,
                        "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
                        "supportsVariableType": true, "supportsRunInTerminalRequest": false
                    }),
                );
                s.pending.insert(seq, Req::Initialize);
                s.client = Some(c);
                s.state = State::Starting;
            }
            Err(e) => {
                self.session = None;
                self.log_line("stderr", format!("{e:#}"));
            }
        }
    }

    fn request(&mut self, command: &str, args: Value, kind: Req) {
        if let Some(s) = &mut self.session {
            if let Some(c) = &mut s.client {
                let seq = c.request(command, args);
                s.pending.insert(seq, kind);
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut s) = self.session.take() {
            if let Some(c) = &mut s.client {
                c.request("disconnect", json!({ "terminateDebuggee": true }));
                // Give the adapter a moment to clean up, then make sure it's gone.
                thread::sleep(std::time::Duration::from_millis(50));
                c.kill();
            }
        }
        self.build_rx = None;
    }

    /// Flutter / Dart: apply code changes to the running app (`hotReload`)
    /// or restart it (`hotRestart`); false when not debugging one.
    pub fn hot_reload(&mut self, restart: bool) -> bool {
        let flutter = self.session.as_ref().is_some_and(|s| matches!(s.config.adapter.as_str(), "flutter" | "dart"));
        if flutter {
            self.request(if restart { "hotRestart" } else { "hotReload" }, json!({}), Req::Other);
        }
        flutter
    }

    pub fn continue_(&mut self) {
        if let Some(t) = self.stopped_thread() {
            self.request("continue", json!({ "threadId": t }), Req::Other);
            self.set_running();
        }
    }

    pub fn step(&mut self, kind: &str) {
        if let Some(t) = self.stopped_thread() {
            self.request(kind, json!({ "threadId": t }), Req::Other);
            self.set_running();
        }
    }

    pub fn pause(&mut self) {
        if matches!(self.session.as_ref().map(|s| &s.state), Some(State::Running)) {
            self.request("pause", json!({ "threadId": 1 }), Req::Other);
        }
    }

    fn set_running(&mut self) {
        if let Some(s) = &mut self.session {
            s.state = State::Running;
            s.frames.clear();
            s.scopes.clear();
            s.children.clear();
        }
    }

    pub fn select_frame(&mut self, i: usize) -> Option<DebugEvent> {
        let s = self.session.as_mut()?;
        let f = s.frames.get(i)?.clone();
        s.frame = i;
        s.scopes.clear();
        s.children.clear();
        self.request("scopes", json!({ "frameId": f.id }), Req::Scopes);
        f.path.map(|p| DebugEvent::Show(p, f.line))
    }

    /// Expand / collapse a variable with children.
    pub fn toggle_var(&mut self, reference: i64) {
        let Some(s) = &mut self.session else { return };
        if !s.expanded.remove(&reference) {
            s.expanded.insert(reference);
            if !s.children.contains_key(&reference) {
                self.request("variables", json!({ "variablesReference": reference }), Req::Variables(reference));
            }
        }
    }

    pub fn evaluate(&mut self, expr: String) {
        self.log_line("input", format!("› {expr}"));
        let frame_id = self.session.as_ref().and_then(|s| s.frames.get(s.frame)).map(|f| f.id);
        // "watch": evaluate an expression in the selected frame (with gdb,
        // "repl" would run it as a gdb command).
        let mut args = json!({ "expression": expr, "context": "watch" });
        if let Some(id) = frame_id {
            args["frameId"] = json!(id);
        }
        self.request("evaluate", args, Req::Evaluate);
    }

    /// Send breakpoints for files whose set changed (all files once configured).
    pub fn sync_breakpoints(&mut self, all: &BTreeMap<PathBuf, Breakpoints>) {
        let configured = self.session.as_ref().is_some_and(|s| s.client.is_some() && s.configured);
        if !configured {
            return;
        }
        let mut files: Vec<PathBuf> = all.keys().cloned().collect();
        files.extend(self.sent.keys().filter(|p| !all.contains_key(*p)).cloned());
        for path in files {
            let lines = all.get(&path).cloned().unwrap_or_default();
            if self.sent.get(&path) == Some(&lines) {
                continue;
            }
            self.send_breakpoints(&path, &lines);
        }
    }

    fn send_breakpoints(&mut self, path: &Path, lines: &Breakpoints) {
        let bps: Vec<Value> = lines
            .iter()
            .filter(|(_, b)| !b.disabled)
            .map(|(l, b)| {
                let mut v = json!({ "line": l + 1 });
                if !b.condition.trim().is_empty() {
                    v["condition"] = json!(b.condition.trim());
                }
                if !b.log.trim().is_empty() {
                    v["logMessage"] = json!(b.log.trim());
                }
                if !b.hit.trim().is_empty() {
                    v["hitCondition"] = json!(b.hit.trim());
                }
                v
            })
            .collect();
        self.request(
            "setBreakpoints",
            json!({ "source": { "path": path, "name": path.file_name().map(|n| n.to_string_lossy()) }, "breakpoints": bps }),
            Req::SetBreakpoints,
        );
        if lines.is_empty() {
            self.sent.remove(path);
        } else {
            self.sent.insert(path.to_path_buf(), lines.clone());
        }
    }

    /// Re-evaluate every watch expression in the selected frame.
    pub fn evaluate_watches(&mut self) {
        self.watch_values = vec![None; self.watches.len()];
        let Some(frame_id) = self.session.as_ref().and_then(|s| s.frames.get(s.frame)).map(|f| f.id) else {
            return;
        };
        for (i, w) in self.watches.clone().into_iter().enumerate() {
            self.request(
                "evaluate",
                json!({ "expression": w, "context": "watch", "frameId": frame_id }),
                Req::Watch(i),
            );
        }
    }

    /// Evaluate the expression under the mouse (shown in the hover popup).
    pub fn evaluate_hover(&mut self, doc: u64, idx: usize, expr: String) {
        let Some(frame_id) = self.session.as_ref().and_then(|s| s.frames.get(s.frame)).map(|f| f.id) else {
            return;
        };
        self.request(
            "evaluate",
            json!({ "expression": expr, "context": "hover", "frameId": frame_id }),
            Req::Hover(doc, idx, expr),
        );
    }

    /// Handle adapter messages; `breakpoints` is used when the adapter asks
    /// for its configuration.
    pub fn poll(&mut self, breakpoints: &BTreeMap<PathBuf, Breakpoints>) -> Vec<DebugEvent> {
        let mut out = vec![];
        if let Some(rx) = &self.build_rx {
            if let Ok(r) = rx.try_recv() {
                self.build_rx = None;
                match r {
                    Ok(()) => self.launch_adapter(),
                    Err(e) => {
                        self.log_line("stderr", format!("Build failed: {e}"));
                        self.session = None;
                        out.push(DebugEvent::Message("Build failed — see the debug console".into()));
                        out.push(DebugEvent::Ended);
                    }
                }
            }
        }
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                DapMessage::Stderr(l) => self.log_line("stderr", l),
                DapMessage::Exited => {
                    if self.session.as_ref().is_some_and(|s| s.client.is_some()) {
                        self.session = None;
                        self.log_line("console", "Debug session ended.");
                        out.push(DebugEvent::Ended);
                    }
                }
                DapMessage::Event { event, body } => self.on_event(&event, &body, breakpoints, &mut out),
                DapMessage::Response { request_seq, command, success, message, body } => {
                    let kind = self.session.as_mut().and_then(|s| s.pending.remove(&request_seq));
                    self.on_response(kind, &command, success, message, &body, &mut out);
                }
            }
        }
        self.maybe_configure(breakpoints);
        out
    }

    /// Send breakpoints and `configurationDone` once the adapter is ready.
    fn maybe_configure(&mut self, breakpoints: &BTreeMap<PathBuf, Breakpoints>) {
        let Some(s) = &mut self.session else { return };
        let ready = !s.configured
            && s.initialized_at.is_some_and(|t| s.launched || t.elapsed() > std::time::Duration::from_millis(400));
        if !ready {
            return;
        }
        s.configured = true;
        for (path, lines) in breakpoints {
            self.send_breakpoints(path, lines);
        }
        self.request("configurationDone", json!({}), Req::ConfigurationDone);
    }

    fn on_event(
        &mut self,
        event: &str,
        body: &Value,
        breakpoints: &BTreeMap<PathBuf, Breakpoints>,
        out: &mut Vec<DebugEvent>,
    ) {
        match event {
            "initialized" => {
                if let Some(s) = &mut self.session {
                    s.initialized_at = Some(std::time::Instant::now());
                }
                self.maybe_configure(breakpoints);
            }
            "output" => {
                // Skip the adapter's start-up banner (e.g. gdb's licence text).
                if self.session.as_ref().is_some_and(|s| s.client.is_some() && s.initialized_at.is_none()) {
                    return;
                }
                let cat = body.get("category").and_then(Value::as_str).unwrap_or("console").to_string();
                if cat != "telemetry" {
                    let text = body.get("output").and_then(Value::as_str).unwrap_or_default().to_string();
                    self.log(&cat, text);
                }
            }
            "stopped" => {
                let thread = body.get("threadId").and_then(Value::as_i64).unwrap_or(1);
                let reason = body.get("reason").and_then(Value::as_str).unwrap_or("paused").to_string();
                if let Some(s) = &mut self.session {
                    s.state = State::Stopped { thread, reason };
                    s.frame = 0;
                }
                self.request("stackTrace", json!({ "threadId": thread, "levels": 50 }), Req::StackTrace);
            }
            "continued" => self.set_running(),
            "terminated" | "exited" => {
                if event == "exited" {
                    let code = body.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
                    self.log_line("console", format!("Program exited with code {code}."));
                }
                if event == "terminated" || self.session.is_some() {
                    self.stop();
                    out.push(DebugEvent::Ended);
                }
            }
            _ => {}
        }
    }

    fn on_response(
        &mut self,
        kind: Option<Req>,
        command: &str,
        success: bool,
        message: Option<String>,
        body: &Value,
        out: &mut Vec<DebugEvent>,
    ) {
        if !success {
            let m = message.unwrap_or_else(|| "failed".into());
            if let Some(Req::Watch(i)) = kind {
                if let Some(slot) = self.watch_values.get_mut(i) {
                    *slot = Some(Err(m));
                }
                return;
            }
            if matches!(kind, Some(Req::Hover(..))) {
                return;
            }
            if matches!(kind, Some(Req::Evaluate)) {
                self.log_line("stderr", m);
            } else if !matches!(kind, Some(Req::Variables(_)) | Some(Req::Scopes) | Some(Req::ConfigurationDone)) {
                self.log_line("stderr", format!("{command}: {m}"));
                if matches!(kind, Some(Req::Launch) | Some(Req::Initialize)) {
                    self.stop();
                    out.push(DebugEvent::Message(format!("Debugging failed: {command}: {m}")));
                    out.push(DebugEvent::Ended);
                }
            }
            return;
        }
        match kind {
            Some(Req::Initialize) => {
                let Some(s) = &self.session else { return };
                let args = zenith_dap::launch_arguments(&s.config, &s.root);
                self.request("launch", args, Req::Launch);
            }
            Some(Req::Launch) => {
                if let Some(s) = &mut self.session {
                    s.launched = true;
                    if s.configured && !matches!(s.state, State::Stopped { .. }) {
                        s.state = State::Running;
                    }
                }
            }
            Some(Req::ConfigurationDone) => {
                if let Some(s) = &mut self.session {
                    if s.state == State::Starting {
                        s.state = State::Running;
                    }
                }
            }
            Some(Req::StackTrace) => {
                let frames = zenith_dap::parse_frames(body);
                if let Some(s) = &mut self.session {
                    s.frames = frames;
                    // Start in the first frame that has source (skip library frames).
                    s.frame = s.frames.iter().position(|f| f.path.as_ref().is_some_and(|p| p.exists())).unwrap_or(0);
                }
                let i = self.session.as_ref().map_or(0, |s| s.frame);
                if let Some(ev) = self.select_frame(i) {
                    out.push(ev);
                }
            }
            Some(Req::Scopes) => {
                self.evaluate_watches();
                let scopes: Vec<Variable> = zenith_dap::parse_variables(body);
                let auto: Vec<i64> = scopes
                    .iter()
                    .filter(|v| v.reference > 0 && !v.name.to_lowercase().contains("register"))
                    .map(|v| v.reference)
                    .collect();
                if let Some(s) = &mut self.session {
                    s.scopes = scopes;
                    s.expanded.extend(auto.iter().copied());
                }
                for r in auto {
                    self.request("variables", json!({ "variablesReference": r }), Req::Variables(r));
                }
            }
            Some(Req::Variables(r)) => {
                let vars = zenith_dap::parse_variables(body);
                if let Some(s) = &mut self.session {
                    s.children.insert(r, vars);
                }
            }
            Some(Req::Evaluate) => {
                let v = body.get("result").and_then(Value::as_str).unwrap_or_default().to_string();
                self.log_line("result", v);
            }
            Some(Req::Watch(i)) => {
                let mut v = body.get("result").and_then(Value::as_str).unwrap_or_default().to_string();
                if v.is_empty() && body.get("variablesReference").and_then(Value::as_i64).unwrap_or(0) > 0 {
                    v = "{…}".into(); // a struct: gdb gives no summary
                }
                if let Some(slot) = self.watch_values.get_mut(i) {
                    *slot = Some(Ok(v));
                }
            }
            Some(Req::Hover(doc, idx, expr)) => {
                let v = body.get("result").and_then(Value::as_str).unwrap_or_default();
                if !v.is_empty() {
                    out.push(DebugEvent::HoverValue(doc, idx, format!("{expr} = {v}")));
                }
            }
            _ => {}
        }
    }
}

/// Build output is shown like console text (cargo writes progress to stderr).
fn build_line(l: String) -> DapMessage {
    DapMessage::Event { event: "output".into(), body: json!({ "category": "console", "output": format!("{l}\n") }) }
}

fn run_build(cmd: &str, argv: Option<Vec<String>>, root: &Path, tx: &Sender<DapMessage>) -> Result<(), String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let mut c = if let Some(a) = argv.filter(|a| !a.is_empty()) {
        let mut c = Command::new(&a[0]);
        c.args(&a[1..]);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    c.current_dir(root).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let stderr = child.stderr.take();
    let tx2 = tx.clone();
    let t = thread::spawn(move || {
        if let Some(e) = stderr {
            for l in std::io::BufReader::new(e).lines().map_while(Result::ok) {
                let _ = tx2.send(build_line(l));
            }
        }
    });
    if let Some(o) = child.stdout.take() {
        for l in std::io::BufReader::new(o).lines().map_while(Result::ok) {
            let _ = tx.send(build_line(l));
        }
    }
    let _ = t.join();
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{cmd}` exited with {status}"))
    }
}

/// The expression under the mouse for a hover value: the identifier at
/// `idx` with the `a.b` / `a::b` / `a->b` chain before it.
pub fn expression_at(buf: &zenith_core::Buffer, idx: usize) -> Option<String> {
    let pos = buf.char_to_pos(idx);
    let line: Vec<char> = buf.line_text(pos.line).chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut e = pos.col;
    if e >= line.len() || !word(line[e]) {
        return None;
    }
    while e < line.len() && word(line[e]) {
        e += 1;
    }
    let mut s = pos.col;
    loop {
        while s > 0 && word(line[s - 1]) {
            s -= 1;
        }
        // Continue through member access.
        if s >= 1 && line[s - 1] == '.' && s >= 2 && word(line[s - 2]) {
            s -= 1;
        } else if s >= 2 && ((line[s - 2] == ':' && line[s - 1] == ':') || (line[s - 2] == '-' && line[s - 1] == '>')) {
            s -= 2;
        } else {
            break;
        }
    }
    let expr: String = line[s..e].iter().collect();
    // Keywords and numbers aren't worth evaluating.
    if expr.chars().next().is_some_and(|c| c.is_ascii_digit())
        || matches!(
            expr.as_str(),
            "fn" | "let"
                | "mut"
                | "pub"
                | "def"
                | "for"
                | "in"
                | "if"
                | "else"
                | "return"
                | "while"
                | "match"
                | "struct"
                | "impl"
                | "use"
                | "self"
                | "import"
                | "class"
        )
    {
        return None;
    }
    Some(expr)
}

// ------------------------------------------------------------------ configurations

pub fn launch_file(root: &Path) -> PathBuf {
    root.join(".zenith").join("launch.toml")
}

pub const LAUNCH_TEMPLATE: &str = r#"# Debug configurations for this workspace (F5 starts the selected one).
#
# adapter: "gdb" (GDB 14+, Rust/C/C++), "lldb" (lldb-dap), "debugpy" (Python)
# or set `command = ["my-adapter", "--dap"]` for any other DAP adapter.

[[configurations]]
name = "Debug program"
adapter = "gdb"
program = "target/debug/my-program"
args = []
cwd = "."
build = "cargo build"
stop_on_entry = false

# [[configurations]]
# name = "Python: current project"
# adapter = "debugpy"
# program = "main.py"
"#;

/// Configurations for the workspace: launch.toml, else sensible defaults
/// (Cargo binaries, the active Python file).
pub fn configurations(root: Option<&Path>, active_file: Option<&Path>) -> (Vec<LaunchConfig>, Option<String>) {
    let mut out = vec![];
    let mut error = None;
    if let Some(root) = root {
        if let Ok(text) = std::fs::read_to_string(launch_file(root)) {
            match zenith_dap::parse_launch_file(&text) {
                Ok(c) => out.extend(c),
                Err(e) => error = Some(format!("launch.toml: {e}")),
            }
        }
    }
    if let Some(f) = active_file.filter(|f| f.extension().is_some_and(|e| e == "py")) {
        out.push(LaunchConfig {
            name: format!("Python: {}", f.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
            adapter: "debugpy".into(),
            program: root.and_then(|r| f.strip_prefix(r).ok()).unwrap_or(f).to_string_lossy().into_owned(),
            ..Default::default()
        });
    }
    // Flutter apps (also one folder down, e.g. `mobile/`).
    if let Some(root) = root {
        let dirs = std::iter::once(String::new()).chain(
            std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.')),
        );
        for d in dirs {
            let pubspec = std::fs::read_to_string(root.join(&d).join("pubspec.yaml")).unwrap_or_default();
            if pubspec.contains("flutter:") && root.join(&d).join("lib/main.dart").exists() {
                let at = |p: &str| if d.is_empty() { p.to_string() } else { format!("{d}/{p}") };
                out.push(LaunchConfig {
                    name: if d.is_empty() { "Flutter".into() } else { format!("Flutter: {d}") },
                    adapter: "flutter".into(),
                    program: at("lib/main.dart"),
                    cwd: d.clone(),
                    ..Default::default()
                });
            }
        }
    }
    if let Some(root) = root.filter(|r| r.join("Cargo.toml").exists()) {
        let native = if zenith_dap::adapter("gdb").and_then(|a| a.resolve()).is_some() { "gdb" } else { "lldb" };
        for bin in cargo_bins(root) {
            let exe = if cfg!(windows) { format!("{bin}.exe") } else { bin.clone() };
            out.push(LaunchConfig {
                name: format!("Rust: {bin}"),
                adapter: native.into(),
                program: format!("target/debug/{exe}"),
                build: format!("cargo build --bin {bin}"),
                ..Default::default()
            });
        }
    }
    (out, error)
}

/// Binary targets of the Cargo workspace (no network: `--no-deps`).
fn cargo_bins(root: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1", "--offline"])
        .current_dir(root)
        .output()
    else {
        return vec![];
    };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return vec![] };
    let mut bins = vec![];
    for p in v.get("packages").and_then(Value::as_array).into_iter().flatten() {
        for t in p.get("targets").and_then(Value::as_array).into_iter().flatten() {
            let is_bin = t.get("kind").and_then(Value::as_array).is_some_and(|k| k.iter().any(|x| x == "bin"));
            if is_bin {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    bins.push(n.to_string());
                }
            }
        }
    }
    bins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_expressions() {
        let b = zenith_core::Buffer::from_str("let x = foo.bar.baz + t.ism;\nstd::env::args(); p->next 42");
        let at = |line: usize, col: usize| expression_at(&b, b.pos_to_char(zenith_core::Position::new(line, col)));
        assert_eq!(at(0, 17).as_deref(), Some("foo.bar.baz")); // on "baz"
        assert_eq!(at(0, 13).as_deref(), Some("foo.bar")); // on "bar"
        assert_eq!(at(0, 8).as_deref(), Some("foo"));
        assert_eq!(at(0, 25).as_deref(), Some("t.ism"));
        assert_eq!(at(0, 0), None); // keyword
        assert_eq!(at(0, 6), None); // "="
        assert_eq!(at(1, 6).as_deref(), Some("std::env"));
        assert_eq!(at(1, 21).as_deref(), Some("p->next"));
        assert_eq!(at(1, 27), None); // number
    }

    #[test]
    fn output_pieces_join_into_lines() {
        let mut d = Debugger::new(Arc::new(|| {}));
        d.log("stdout", "jami:");
        d.log("stdout", " ");
        d.log("stdout", "14\nnext\n");
        d.log_line("console", "done");
        d.log("stdout", "a");
        d.log("stderr", "b\n");
        let got: Vec<(&str, &str)> = d.output.iter().map(|(c, l)| (c.as_str(), l.as_str())).collect();
        assert_eq!(
            got,
            vec![("stdout", "jami: 14"), ("stdout", "next"), ("console", "done"), ("stdout", "a"), ("stderr", "b")]
        );
    }

    /// Full session against the real gdb (`gdb -i dap`, GDB 14+) on a tiny
    /// Rust program: breakpoint, stack, locals, step, continue to the end.
    #[test]
    #[ignore]
    fn gdb_session() {
        let dir = std::env::temp_dir().join(format!("zenith-dbg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("main.rs");
        std::fs::write(&src, "fn sq(x: i32) -> i32 {\n    let y = x * x;\n    y\n}\nfn main() {\n    let t = sq(3);\n    println!(\"{t}\");\n}\n").unwrap();
        let ok = std::process::Command::new("rustc").args(["-g", "-o", "app", "main.rs"]).current_dir(&dir).status();
        assert!(ok.is_ok_and(|s| s.success()));
        let src = src.canonicalize().unwrap();
        let mut d = Debugger::new(Arc::new(|| {}));
        let bps = BTreeMap::from([(src.clone(), Breakpoints::from([(1usize, Bp::default())]))]);
        d.start(
            LaunchConfig { name: "t".into(), adapter: "gdb".into(), program: "app".into(), ..Default::default() },
            dir.clone(),
        );
        let wait = |d: &mut Debugger, f: &dyn Fn(&Debugger) -> bool| {
            let t0 = std::time::Instant::now();
            let mut evs = vec![];
            while t0.elapsed().as_secs() < 20 && !f(d) {
                evs.extend(d.poll(&bps));
                thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(f(d), "timed out; output: {:?}", d.output);
            evs
        };
        let evs = wait(&mut d, &|d| {
            d.session.as_ref().is_some_and(|s| {
                matches!(s.state, State::Stopped { .. }) && s.children.values().any(|v| v.iter().any(|v| v.name == "x"))
            })
        });
        assert!(evs.iter().any(|e| matches!(e, DebugEvent::Show(p, 1) if p.ends_with("main.rs"))));
        let s = d.session.as_ref().unwrap();
        assert_eq!(s.frames[0].name.split("::").last(), Some("sq"));
        let x = s.children.values().flatten().find(|v| v.name == "x").unwrap();
        assert_eq!(x.value, "3");
        d.continue_();
        wait(&mut d, &|d| d.session.is_none());
        assert!(d.output.iter().any(|(_, l)| l.contains("exited with code 0")), "{:?}", d.output);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
