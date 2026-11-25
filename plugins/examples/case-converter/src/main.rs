//! Zenith example plugin compiled to WebAssembly (WASI).
//!
//! The protocol is the same as for any plugin: one JSON object per line on
//! stdin (from the editor) and stdout (to the editor). The module runs
//! inside the editor with no access to files, network or environment.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// Split an identifier or phrase into lowercase words.
fn words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        // A new word starts at an upper-case letter after a lower-case one
        // (fooBar) or before a lower-case one in a run of capitals (HTTPServer).
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = c.is_uppercase()
            && (prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(char::is_uppercase) && next.is_some_and(char::is_lowercase)));
        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn capitalize(w: &str) -> String {
    let mut c = w.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn convert(command: &str, s: &str) -> Option<String> {
    let w = words(s);
    Some(match command {
        "case.snake" => w.join("_"),
        "case.kebab" => w.join("-"),
        "case.camel" => w
            .iter()
            .enumerate()
            .map(|(i, x)| if i == 0 { x.clone() } else { capitalize(x) })
            .collect(),
        "case.pascal" => w.iter().map(|x| capitalize(x)).collect(),
        "case.title" => w.iter().map(|x| capitalize(x)).collect::<Vec<_>>().join(" "),
        _ => return None,
    })
}

fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method")?.as_str()?;
    if method != "execute" {
        return None;
    }
    let params = &msg["params"];
    let command = params["command"].as_str().unwrap_or("");
    let sel = params["context"]["selection"].as_str().unwrap_or("");
    let actions = if sel.trim().is_empty() {
        json!([{ "type": "notify", "message": "Select some text first", "level": "warning" }])
    } else {
        // Convert each line on its own, keeping the line breaks.
        let lines: Vec<String> =
            sel.split('\n').map(|l| convert(command, l).filter(|_| !l.trim().is_empty()).unwrap_or(l.to_string())).collect();
        json!([{ "type": "replace_selection", "text": lines.join("\n") }])
    };
    Some(json!({ "id": id, "result": { "actions": actions } }))
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if msg.get("method").and_then(Value::as_str) == Some("shutdown") {
            break;
        }
        if let Some(reply) = handle(&msg) {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases() {
        assert_eq!(words("parseHTTPServer_v2"), ["parse", "http", "server", "v2"]);
        assert_eq!(convert("case.snake", "myVariableName").unwrap(), "my_variable_name");
        assert_eq!(convert("case.camel", "my_variable_name").unwrap(), "myVariableName");
        assert_eq!(convert("case.pascal", "my-variable name").unwrap(), "MyVariableName");
        assert_eq!(convert("case.kebab", "MyVariableName").unwrap(), "my-variable-name");
        assert_eq!(convert("case.title", "my_variable").unwrap(), "My Variable");
    }
}
