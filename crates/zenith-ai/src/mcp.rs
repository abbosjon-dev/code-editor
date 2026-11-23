//! MCP (Model Context Protocol) servers for AI agents.
//!
//! The workspace's `.mcp.json` (read by Claude Code and other agents) is the
//! source of truth; Zenith edits it in place, keeping any keys it doesn't
//! know, and can mirror the servers into `.gemini/settings.json` for Gemini
//! CLI. A stdio server can be probed: started, initialized, and asked for
//! its tools.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::time::Duration;

use serde_json::{json, Map, Value};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    /// Remote server URL (`type: http`); empty for a local stdio server.
    pub url: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Keys of the entry Zenith doesn't edit (kept as they are).
    pub extra: Map<String, Value>,
}

impl McpServer {
    pub fn is_remote(&self) -> bool {
        !self.url.trim().is_empty()
    }

    fn from_json(name: &str, v: &Value) -> Self {
        let mut extra = v.as_object().cloned().unwrap_or_default();
        let take_str = |m: &mut Map<String, Value>, k: &str| m.remove(k).and_then(|v| v.as_str().map(str::to_string));
        let command = take_str(&mut extra, "command").unwrap_or_default();
        let url = take_str(&mut extra, "url").unwrap_or_default();
        let args = extra
            .remove("args")
            .and_then(|a| a.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()))
            .unwrap_or_default();
        let env = extra
            .remove("env")
            .and_then(|e| {
                e.as_object()
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
            })
            .unwrap_or_default();
        // `type` follows from url/command.
        extra.remove("type");
        Self { name: name.to_string(), url, command, args, env, extra }
    }

    fn to_json(&self) -> Value {
        let mut m = self.extra.clone();
        if self.is_remote() {
            m.insert("type".into(), json!("http"));
            m.insert("url".into(), json!(self.url.trim()));
        } else {
            m.insert("command".into(), json!(self.command.trim()));
            m.insert("args".into(), json!(self.args));
            if !self.env.is_empty() {
                m.insert("env".into(), json!(self.env));
            }
        }
        Value::Object(m)
    }
}

pub fn mcp_json_path(workspace: &Path) -> PathBuf {
    workspace.join(".mcp.json")
}

/// Servers listed in a `.mcp.json` text.
pub fn parse_mcp_json(text: &str) -> Result<Vec<McpServer>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!(".mcp.json: {e}"))?;
    Ok(v.get("mcpServers")
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| McpServer::from_json(k, v)).collect())
        .unwrap_or_default())
}

/// `existing` (a JSON config, or nothing) with its `mcpServers` replaced.
pub fn with_servers(existing: Option<&str>, servers: &[McpServer]) -> Result<String, String> {
    let mut root = match existing.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => serde_json::from_str::<Value>(t).map_err(|e| format!("can't update the existing file: {e}"))?,
        None => json!({}),
    };
    let obj = root.as_object_mut().ok_or("the existing file is not a JSON object")?;
    let servers: Map<String, Value> = servers.iter().map(|s| (s.name.clone(), s.to_json())).collect();
    obj.insert("mcpServers".into(), Value::Object(servers));
    serde_json::to_string_pretty(&root).map(|s| s + "\n").map_err(|e| e.to_string())
}

pub fn load(workspace: &Path) -> Result<Vec<McpServer>, String> {
    match std::fs::read_to_string(mcp_json_path(workspace)) {
        Ok(t) => parse_mcp_json(&t),
        Err(_) => Ok(vec![]),
    }
}

/// Write the servers to `.mcp.json` (and `.gemini/settings.json` if asked).
pub fn save(workspace: &Path, servers: &[McpServer], gemini: bool) -> Result<(), String> {
    let path = mcp_json_path(workspace);
    let old = std::fs::read_to_string(&path).ok();
    std::fs::write(&path, with_servers(old.as_deref(), servers)?).map_err(|e| e.to_string())?;
    if gemini {
        let dir = workspace.join(".gemini");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let p = dir.join("settings.json");
        let old = std::fs::read_to_string(&p).ok();
        // Gemini CLI uses `httpUrl` for streamable HTTP servers.
        let gem: Vec<McpServer> = servers
            .iter()
            .map(|s| {
                let mut s = s.clone();
                if s.is_remote() {
                    s.extra.insert("httpUrl".into(), json!(s.url.trim()));
                }
                s
            })
            .collect();
        std::fs::write(&p, with_servers(old.as_deref(), &gem)?).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Start a stdio server, initialize it and list its tools (10 s limit).
pub fn probe(server: &McpServer, cwd: &Path) -> Result<Vec<String>, String> {
    if server.is_remote() {
        return Err("remote servers are checked by the agent".into());
    }
    let program = which::which(server.command.trim()).map_err(|_| format!("`{}` not found on PATH", server.command))?;
    let mut cmd = Command::new(program);
    cmd.args(&server.args)
        .envs(&server.env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("failed to start: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let send = |stdin: &mut std::process::ChildStdin, v: Value| -> Result<(), String> {
        writeln!(stdin, "{v}").and_then(|_| stdin.flush()).map_err(|e| format!("server closed: {e}"))
    };
    let wait_for = |id: i64| -> Result<Value, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let v = rx.recv_timeout(left).map_err(|_| "no answer within 10 s".to_string())?;
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(e) = v.get("error") {
                    return Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string());
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    };
    let result = (|| {
        send(
            &mut stdin,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "zenith", "version": env!("CARGO_PKG_VERSION")}
            }}),
        )?;
        wait_for(1)?;
        send(&mut stdin, json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        send(&mut stdin, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))?;
        let r = wait_for(2)?;
        Ok(r.get("tools")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string)).collect())
            .unwrap_or_default())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_unknown_keys() {
        let text = r#"{"other": 1, "mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."], "env": {"A": "b"}, "timeout": 5},
            "web": {"type": "http", "url": "https://example.com/mcp", "headers": {"X": "y"}}
        }}"#;
        let servers = parse_mcp_json(text).unwrap();
        assert_eq!(servers.len(), 2);
        let fs = servers.iter().find(|s| s.name == "fs").unwrap();
        assert_eq!((fs.command.as_str(), fs.args.len(), fs.env["A"].as_str()), ("npx", 3, "b"));
        let web = servers.iter().find(|s| s.name == "web").unwrap();
        assert!(web.is_remote());
        let out = with_servers(Some(text), &servers).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["other"], 1);
        assert_eq!(v["mcpServers"]["fs"]["timeout"], 5);
        assert_eq!(v["mcpServers"]["web"]["headers"]["X"], "y");
        assert_eq!(v["mcpServers"]["web"]["type"], "http");
        assert_eq!(parse_mcp_json(&out).unwrap(), servers);
        // Removing a server, and a fresh file.
        let out = with_servers(None, &servers[..1]).unwrap();
        assert_eq!(parse_mcp_json(&out).unwrap().len(), 1);
        assert!(with_servers(Some("[1]"), &servers).is_err());
    }

    /// A tiny MCP server in Python answers initialize and tools/list.
    #[test]
    fn probes_a_stdio_server() {
        let py = ["python3", "python"].into_iter().find(|p| which::which(p).is_ok());
        let Some(py) = py else { return };
        let script = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("method") == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif m.get("method") == "tools/list":
        r = {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}, {"name": "add", "inputSchema": {"type": "object"}}]}
    else:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zenith-mcp-{}.py", std::process::id()));
        std::fs::write(&path, script).unwrap();
        let s = McpServer {
            name: "t".into(),
            command: py.into(),
            args: vec![path.to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert_eq!(probe(&s, &dir).unwrap(), vec!["echo".to_string(), "add".to_string()]);
        let missing = McpServer { name: "x".into(), command: "zenith-no-such-cmd".into(), ..Default::default() };
        assert!(probe(&missing, &dir).unwrap_err().contains("not found"));
        let _ = std::fs::remove_file(path);
    }
}
