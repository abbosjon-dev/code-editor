//! Zenith example WebAssembly plugin that asks for a permission:
//! `read_workspace` shows the open folder at `/workspace`, read-only.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

const SKIP: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__", "dist", "build"];

/// Files and lines per extension under `dir`.
fn count(dir: &Path, out: &mut BTreeMap<String, (usize, usize)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if !SKIP.contains(&name.as_str()) {
                count(&p, out);
            }
            continue;
        }
        let Some(ext) = p.extension().map(|e| e.to_string_lossy().to_lowercase()) else { continue };
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let entry = out.entry(ext).or_default();
        entry.0 += 1;
        entry.1 += text.lines().count();
    }
}

fn notify(msg: String, level: &str) -> Value {
    json!([{ "type": "notify", "message": msg, "level": level }])
}

fn run(command: &str) -> Value {
    let root = Path::new("/workspace");
    if !root.exists() {
        return notify("No workspace is open (or the plugin has no permission to read it)".into(), "warning");
    }
    match command {
        "stats.count" => {
            let mut by_ext = BTreeMap::new();
            count(root, &mut by_ext);
            let mut rows: Vec<_> = by_ext.into_iter().collect();
            rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
            let total: usize = rows.iter().map(|r| r.1 .1).sum();
            let top: Vec<String> =
                rows.iter().take(5).map(|(ext, (files, lines))| format!(".{ext}: {lines} lines in {files} files")).collect();
            notify(format!("{total} lines. {}", top.join(" · ")), "info")
        }
        "stats.sandbox" => {
            // Reading works; changing anything must be refused.
            let read = std::fs::read_dir(root).map(|d| d.count()).unwrap_or(0);
            let write = std::fs::write(root.join("zenith-sandbox-test.txt"), "x").is_ok();
            let mkdir = std::fs::create_dir(root.join("zenith-sandbox-dir")).is_ok();
            let level = if write || mkdir { "error" } else { "info" };
            notify(format!("read: {read} entries; write allowed: {write}; mkdir allowed: {mkdir}"), level)
        }
        _ => notify(format!("unknown command {command}"), "error"),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        match msg.get("method").and_then(Value::as_str) {
            Some("shutdown") => break,
            Some("execute") => {
                let command = msg["params"]["command"].as_str().unwrap_or("");
                let reply = json!({ "id": msg["id"], "result": { "actions": run(command) } });
                let _ = writeln!(stdout, "{reply}");
                let _ = stdout.flush();
            }
            _ => {}
        }
    }
}
