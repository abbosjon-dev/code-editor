//! Debug Adapter Protocol client.
//!
//! The editor talks to a debug adapter (gdb's built-in DAP mode, lldb-dap,
//! debugpy, …) over stdin/stdout with `Content-Length` framed JSON. All IO
//! runs on background threads; messages arrive on a channel and the UI
//! polls them once per frame.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ------------------------------------------------------------------ framing

pub fn write_message(w: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one message. `Ok(None)` at end of stream.
pub fn read_message(r: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0u8; len.unwrap_or(0)];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

// ------------------------------------------------------------------ messages

/// Something the adapter sent.
#[derive(Debug, Clone)]
pub enum DapMessage {
    Response {
        request_seq: i64,
        command: String,
        success: bool,
        message: Option<String>,
        body: Value,
    },
    Event {
        event: String,
        body: Value,
    },
    /// The adapter process ended.
    Exited,
    /// A line the adapter printed on stderr.
    Stderr(String),
}

pub fn parse_message(v: &Value) -> Option<DapMessage> {
    match v.get("type")?.as_str()? {
        "response" => Some(DapMessage::Response {
            request_seq: v.get("request_seq")?.as_i64()?,
            command: v.get("command").and_then(Value::as_str).unwrap_or_default().to_string(),
            success: v.get("success").and_then(Value::as_bool).unwrap_or(false),
            message: v.get("message").and_then(Value::as_str).map(str::to_string),
            body: v.get("body").cloned().unwrap_or(Value::Null),
        }),
        "event" => Some(DapMessage::Event {
            event: v.get("event")?.as_str()?.to_string(),
            body: v.get("body").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

// ------------------------------------------------------------------ client

pub struct DapClient {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Child,
    seq: Arc<Mutex<i64>>,
    /// Path translation when the adapter runs on another machine.
    map: Option<zenith_core::pathmap::PathMap>,
}

impl DapClient {
    pub fn start(
        command: &str,
        args: &[String],
        cwd: &Path,
        tx: Sender<DapMessage>,
        waker: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        Self::start_with(command, args, cwd, tx, waker, None)
    }

    /// Like [`start`](Self::start); with `map`, paths in messages are
    /// translated (the adapter runs on another machine, e.g. through ssh).
    pub fn start_with(
        command: &str,
        args: &[String],
        cwd: &Path,
        tx: Sender<DapMessage>,
        waker: Arc<dyn Fn() + Send + Sync>,
        map: Option<zenith_core::pathmap::PathMap>,
    ) -> Result<Self> {
        let program = which::which(command).unwrap_or_else(|_| PathBuf::from(command));
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().with_context(|| format!("failed to start debug adapter `{command}`"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().context("no stdin")?));
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let seq = Arc::new(Mutex::new(0));
        {
            let tx = tx.clone();
            let waker = waker.clone();
            let stdin = stdin.clone();
            let seq = seq.clone();
            let map = map.clone();
            thread::Builder::new().name("dap-reader".into()).spawn(move || {
                let mut reader = BufReader::new(stdout);
                let trace = std::env::var_os("ZENITH_DAP_TRACE").is_some();
                while let Ok(Some(mut v)) = read_message(&mut reader) {
                    if let Some(m) = &map {
                        m.incoming(&mut v);
                    }
                    if trace {
                        eprintln!("dap <- {v}");
                    }
                    // Reverse requests we don't support get an error reply,
                    // so the adapter doesn't wait forever.
                    if v.get("type").and_then(Value::as_str) == Some("request") {
                        let mut s = seq.lock().unwrap();
                        *s += 1;
                        let reply = json!({
                            "seq": *s, "type": "response",
                            "request_seq": v.get("seq").cloned().unwrap_or(Value::Null),
                            "command": v.get("command").cloned().unwrap_or(Value::Null),
                            "success": false, "message": "not supported by Zenith"
                        });
                        let _ = write_message(&mut *stdin.lock().unwrap(), &reply);
                        continue;
                    }
                    if let Some(m) = parse_message(&v) {
                        let _ = tx.send(m);
                        waker();
                    }
                }
                let _ = tx.send(DapMessage::Exited);
                waker();
            })?;
        }
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                let _ = tx.send(DapMessage::Stderr(line));
            }
        });
        Ok(Self { stdin, child, seq, map })
    }

    /// Send a request; returns its sequence number (matched by responses).
    pub fn request(&mut self, command: &str, arguments: Value) -> i64 {
        let seq = {
            let mut s = self.seq.lock().unwrap();
            *s += 1;
            *s
        };
        let mut msg = json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments });
        if let Some(m) = &self.map {
            m.outgoing(&mut msg);
        }
        if std::env::var_os("ZENITH_DAP_TRACE").is_some() {
            eprintln!("dap -> {msg}");
        }
        if let Err(e) = write_message(&mut *self.stdin.lock().unwrap(), &msg) {
            log::warn!("dap: {command}: {e}");
        }
        seq
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        self.kill();
    }
}

// ------------------------------------------------------------------ adapters & launch configs

/// A debug adapter program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub id: &'static str,
    pub name: &'static str,
    /// Candidate commands; the first one installed is used.
    pub commands: &'static [(&'static str, &'static [&'static str])],
    pub install_hint: &'static str,
}

pub const ADAPTERS: &[Adapter] = &[
    Adapter {
        id: "gdb",
        name: "GDB (Rust, C, C++)",
        commands: &[("gdb", &["-i", "dap"])],
        install_hint: "Install GDB 14 or newer (apt install gdb / brew install gdb)",
    },
    Adapter {
        id: "lldb",
        name: "LLDB (Rust, C, C++, Swift)",
        commands: &[("lldb-dap", &[]), ("lldb-vscode", &[])],
        install_hint: "Install LLDB (it ships lldb-dap; on macOS: xcrun lldb-dap)",
    },
    Adapter {
        id: "flutter",
        name: "Flutter",
        commands: &[("flutter", &["debug_adapter"])],
        install_hint: "Install the Flutter SDK (flutter.dev) and put flutter on PATH",
    },
    Adapter {
        id: "dart",
        name: "Dart",
        commands: &[("dart", &["debug_adapter"])],
        install_hint: "Install the Dart SDK (dart.dev) and put dart on PATH",
    },
    Adapter {
        id: "debugpy",
        name: "Python (debugpy)",
        commands: &[("python3", &["-m", "debugpy.adapter"]), ("python", &["-m", "debugpy.adapter"])],
        install_hint: "pip install debugpy",
    },
];

pub fn adapter(id: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.id == id)
}

impl Adapter {
    /// Command line to start the adapter, if it is installed.
    pub fn resolve(&self) -> Option<(String, Vec<String>)> {
        self.commands
            .iter()
            .find(|(c, _)| which::which(c).is_ok())
            .map(|(c, a)| (c.to_string(), a.iter().map(|s| s.to_string()).collect()))
    }
}

/// One entry of `.zenith/launch.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    pub name: String,
    /// "gdb", "lldb", "debugpy" — or any program in `command` for a custom adapter.
    pub adapter: String,
    /// Custom adapter command line (overrides `adapter`).
    pub command: Vec<String>,
    /// Program to debug (relative to the workspace).
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    /// Shell command run before launching (e.g. "cargo build").
    pub build: String,
    pub stop_on_entry: bool,
    pub env: BTreeMap<String, String>,
    /// Extra adapter-specific launch arguments, passed through as JSON.
    pub extra: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LaunchFile {
    configurations: Vec<LaunchConfig>,
}

pub fn parse_launch_file(text: &str) -> Result<Vec<LaunchConfig>> {
    let f: LaunchFile = toml::from_str(text)?;
    Ok(f.configurations)
}

fn resolve_path(root: &Path, p: &str) -> String {
    if p.is_empty() {
        return root.to_string_lossy().into_owned();
    }
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        root.join(path).to_string_lossy().into_owned()
    }
}

/// `launch` request arguments for a configuration.
pub fn launch_arguments(cfg: &LaunchConfig, root: &Path) -> Value {
    let program = resolve_path(root, &cfg.program);
    let cwd = resolve_path(root, &cfg.cwd);
    let mut v = match cfg.adapter.as_str() {
        "gdb" => json!({
            "program": program, "args": cfg.args, "cwd": cwd,
            "stopAtBeginningOfMainSubprogram": cfg.stop_on_entry,
        }),
        "debugpy" => json!({
            "type": "python", "request": "launch", "program": program, "args": cfg.args, "cwd": cwd,
            "console": "internalConsole", "justMyCode": true, "stopOnEntry": cfg.stop_on_entry,
        }),
        // Flutter / Dart: `toolArgs` go to `flutter run` (e.g. ["-d", "emulator-5554"]).
        "flutter" | "dart" => json!({
            "program": if cfg.program.is_empty() { resolve_path(root, "lib/main.dart") } else { program },
            "args": cfg.args, "cwd": cwd,
        }),
        _ => json!({ "program": program, "args": cfg.args, "cwd": cwd, "stopOnEntry": cfg.stop_on_entry }),
    };
    if !cfg.env.is_empty() {
        v["env"] = json!(cfg.env);
    }
    for (k, val) in &cfg.extra {
        if let Ok(j) = serde_json::to_value(val) {
            v[k] = j;
        }
    }
    // debugpy runs either a file or a module (`python -m pytest`), not both.
    if cfg.extra.contains_key("module") && cfg.program.is_empty() {
        if let Some(o) = v.as_object_mut() {
            o.remove("program");
        }
    }
    v
}

// ------------------------------------------------------------------ data

#[derive(Debug, Clone, PartialEq)]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    pub path: Option<PathBuf>,
    /// 0-based.
    pub line: usize,
}

pub fn parse_frames(body: &Value) -> Vec<StackFrame> {
    body.get("stackFrames")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(StackFrame {
                        id: f.get("id")?.as_i64()?,
                        name: f.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
                        path: f.pointer("/source/path").and_then(Value::as_str).map(PathBuf::from),
                        line: (f.get("line").and_then(Value::as_u64).unwrap_or(1) as usize).saturating_sub(1),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub type_name: String,
    /// > 0 when it has children (fetch with a `variables` request).
    pub reference: i64,
}

/// Parse a `scopes` (key "scopes") or `variables` (key "variables") body.
pub fn parse_variables(body: &Value) -> Vec<Variable> {
    let list = body.get("variables").or_else(|| body.get("scopes")).and_then(Value::as_array);
    list.map(|a| {
        a.iter()
            .map(|v| Variable {
                name: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                value: v.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
                type_name: v.get("type").and_then(Value::as_str).unwrap_or_default().to_string(),
                reference: v.get("variablesReference").and_then(Value::as_i64).unwrap_or(0),
            })
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_and_parsing() {
        let mut buf = vec![];
        write_message(&mut buf, &json!({"type":"event","event":"stopped","body":{"threadId":1}})).unwrap();
        let v = read_message(&mut io::BufReader::new(&buf[..])).unwrap().unwrap();
        let Some(DapMessage::Event { event, body }) = parse_message(&v) else { panic!() };
        assert_eq!((event.as_str(), body["threadId"].as_i64()), ("stopped", Some(1)));
        let r = json!({"type":"response","request_seq":4,"command":"threads","success":true,"body":{}});
        assert!(matches!(parse_message(&r), Some(DapMessage::Response { request_seq: 4, success: true, .. })));
    }

    #[test]
    fn frames_and_variables() {
        let f = parse_frames(&json!({"stackFrames":[{"id":7,"name":"main","line":12,"source":{"path":"/a/b.rs"}}]}));
        assert_eq!(f[0].line, 11);
        assert_eq!(f[0].path.as_deref(), Some(Path::new("/a/b.rs")));
        let v = parse_variables(&json!({"variables":[{"name":"x","value":"5","type":"i32","variablesReference":0}]}));
        assert_eq!((v[0].name.as_str(), v[0].value.as_str(), v[0].reference), ("x", "5", 0));
        let s = parse_variables(&json!({"scopes":[{"name":"Locals","variablesReference":3}]}));
        assert_eq!(s[0].reference, 3);
    }

    #[test]
    fn launch_config() {
        let cfgs = parse_launch_file(
            "[[configurations]]\nname='app'\nadapter='gdb'\nprogram='target/debug/app'\nargs=['-v']\nbuild='cargo build'\n[configurations.extra]\nfoo=1\n",
        )
        .unwrap();
        let a = launch_arguments(&cfgs[0], Path::new("/ws"));
        let expected = Path::new("/ws").join("target/debug/app");
        assert_eq!(a["program"], expected.to_string_lossy().as_ref());
        assert_eq!(a["args"][0], "-v");
        assert_eq!(a["foo"], 1);
        assert_eq!(cfgs[0].build, "cargo build");
        let py = launch_arguments(
            &LaunchConfig { adapter: "debugpy".into(), program: "main.py".into(), ..Default::default() },
            Path::new("/ws"),
        );
        assert_eq!(py["console"], "internalConsole");
    }
}
