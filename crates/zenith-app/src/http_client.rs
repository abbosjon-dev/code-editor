//! HTTP requests from `.http` / `.rest` files (the REST Client / JetBrains
//! format): requests separated by `###`, `@name = value` variables used as
//! `{{name}}`, then headers, a blank line and the body. The request under
//! the caret is sent with curl (locally, or on the SSH host so its
//! localhost is reachable) and the response shown in a panel.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{channel, Receiver};
use std::time::Instant;

use eframe::egui::{self, RichText};

use crate::remote::RemoteSpec;
use crate::theme::Palette;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Request {
    pub name: String,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// A new request file.
pub const TEMPLATE: &str = "@base = http://localhost:3000\n\n### List items\nGET {{base}}/items\nAccept: application/json\n\n### Create an item\nPOST {{base}}/items\nContent-Type: application/json\n\n{\n  \"name\": \"example\"\n}\n";

const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// `@name = value` lines anywhere in the file.
pub fn variables(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim().strip_prefix('@')?;
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Replace `{{name}}` with file variables, then `extra` (env file / OS
/// environment); unknown names stay as they are.
pub fn substitute(s: &str, vars: &HashMap<String, String>, extra: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find("}}") {
            Some(j) => {
                let name = after[..j].trim();
                let name = name.strip_prefix("$env ").or(name.strip_prefix("$dotenv ")).unwrap_or(name).trim();
                match vars.get(name).cloned().or_else(|| extra(name)) {
                    // Variables may refer to other variables.
                    Some(v) => out.push_str(&substitute(&v, vars, extra)),
                    None => out.push_str(&rest[i..i + 2 + j + 2]),
                }
                rest = &after[j + 2..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The request around `line` (0-based), with variables filled in.
pub fn request_at(text: &str, line: usize, extra: &dyn Fn(&str) -> Option<String>) -> Option<Request> {
    let lines: Vec<&str> = text.lines().collect();
    let line = line.min(lines.len().saturating_sub(1));
    let start = (0..=line).rev().find(|&i| lines[i].trim_start().starts_with("###")).map_or(0, |i| i + 1);
    let end = (line + 1..lines.len()).find(|&i| lines[i].trim_start().starts_with("###")).unwrap_or(lines.len());
    let name = if start > 0 {
        lines[start - 1].trim_start().trim_start_matches('#').trim().to_string()
    } else {
        String::new()
    };
    let vars = variables(text);
    let mut it = lines[start..end]
        .iter()
        .map(|l| l.trim_end())
        // Comments and variable definitions aren't part of the request.
        .filter(|l| {
            !l.trim_start().starts_with('#') && !l.trim_start().starts_with("//") && !l.trim_start().starts_with('@')
        })
        .skip_while(|l| l.trim().is_empty())
        .peekable();
    let first = it.next()?;
    let mut parts = first.split_whitespace();
    let (method, url) = match parts.next()? {
        m if METHODS.contains(&m.to_uppercase().as_str()) => (m.to_uppercase(), parts.next()?.to_string()),
        u => ("GET".to_string(), u.to_string()),
    };
    let mut headers = vec![];
    while let Some(l) = it.peek() {
        if l.trim().is_empty() {
            it.next();
            break;
        }
        // A URL continued on the next lines (`?a=1`, `&b=2`) is rare; headers aren't.
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), substitute(v.trim(), &vars, extra)));
        }
        it.next();
    }
    let body: Vec<&str> = it.collect();
    let body = body.join("\n").trim_end().to_string();
    Some(Request { name, method, url: substitute(&url, &vars, extra), headers, body: substitute(&body, &vars, extra) })
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub millis: u128,
    pub request: Request,
    pub error: Option<String>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The body, pretty-printed when it is JSON.
    pub fn pretty_body(&self) -> String {
        let json = self.header("content-type").is_some_and(|c| c.contains("json"))
            || self.body.trim_start().starts_with(['{', '[']);
        if json {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&self.body) {
                if let Ok(p) = serde_json::to_string_pretty(&v) {
                    return p;
                }
            }
        }
        self.body.clone()
    }
}

/// `curl -i` output (with -L there can be several header blocks): the last
/// status, its headers and the body.
pub fn parse_response(raw: &[u8]) -> (u16, String, Vec<(String, String)>, String) {
    let text = String::from_utf8_lossy(raw).into_owned();
    let mut rest = text.as_str();
    let mut status = 0;
    let mut reason = String::new();
    let mut headers = vec![];
    // Header blocks start with "HTTP/"; the body follows the last one.
    while rest.starts_with("HTTP/") {
        let (head, body) = match rest.find("\r\n\r\n") {
            Some(i) => (&rest[..i], &rest[i + 4..]),
            None => match rest.find("\n\n") {
                Some(i) => (&rest[..i], &rest[i + 2..]),
                None => (rest, ""),
            },
        };
        let mut lines = head.lines();
        let first = lines.next().unwrap_or_default();
        let mut p = first.splitn(3, ' ');
        p.next();
        status = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        reason = p.next().unwrap_or_default().trim().to_string();
        headers = lines
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
            .collect();
        rest = body;
        // "100 Continue" and redirects: another block follows.
        if !(rest.starts_with("HTTP/")) {
            break;
        }
    }
    (status, reason, headers, rest.to_string())
}

/// Send `req` with curl, on a thread.
pub fn send(req: Request, remote: Option<RemoteSpec>) -> Receiver<Response> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut args: Vec<String> = vec!["-sS".into(), "-i".into(), "-L".into(), "--max-time".into(), "60".into()];
        args.extend(["-X".into(), req.method.clone()]);
        for (k, v) in &req.headers {
            args.extend(["-H".into(), format!("{k}: {v}")]);
        }
        if !req.body.is_empty() {
            args.extend(["--data-binary".into(), "@-".into()]);
        }
        args.push(req.url.clone());
        let mut cmd = match &remote {
            Some(r) => {
                let line: Vec<String> = std::iter::once("curl".to_string())
                    .chain(args.iter().cloned())
                    .map(|a| crate::remote::sh_quote(&a))
                    .collect();
                let mut c = std::process::Command::new("ssh");
                c.args(crate::remote::ssh_args(r, false));
                c.arg(line.join(" "));
                c
            }
            None => {
                let mut c = std::process::Command::new("curl");
                c.args(&args);
                c
            }
        };
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let started = Instant::now();
        let result = cmd.spawn().and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(req.body.as_bytes());
            }
            child.wait_with_output()
        });
        let millis = started.elapsed().as_millis();
        let resp = match result {
            Ok(out) if !out.stdout.is_empty() => {
                let (status, reason, headers, body) = parse_response(&out.stdout);
                Response { status, reason, headers, body, millis, request: req, error: None }
            }
            Ok(out) => {
                let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Response {
                    millis,
                    request: req,
                    error: Some(if e.is_empty() { "no response".into() } else { e }),
                    ..Default::default()
                }
            }
            Err(e) => Response { millis, request: req, error: Some(format!("curl: {e}")), ..Default::default() },
        };
        let _ = tx.send(resp);
    });
    rx
}

pub enum Action {
    Close,
    /// Open the body in a new editor tab.
    OpenBody(String),
    Resend(Request),
    /// Ask the AI agent about the request and its response.
    AskAi(String),
}

/// A prompt about a request and its response (body cut short).
pub fn ai_prompt(r: &Response) -> String {
    let q = &r.request;
    let mut s = String::from("Look at this HTTP request and its response: explain the result and, if something is wrong, why and how to fix it (in the request or the server code of this project).\n\n");
    s.push_str(&format!("```http\n{} {}\n", q.method, q.url));
    for (k, v) in &q.headers {
        // Don't hand secrets to the agent.
        let v = if k.eq_ignore_ascii_case("authorization")
            || k.to_lowercase().contains("token")
            || k.to_lowercase().contains("key")
        {
            "•••"
        } else {
            v.as_str()
        };
        s.push_str(&format!("{k}: {v}\n"));
    }
    if !q.body.is_empty() {
        s.push_str(&format!("\n{}\n", q.body.chars().take(2000).collect::<String>()));
    }
    s.push_str("```\n");
    match &r.error {
        Some(e) => s.push_str(&format!("No response: {e}\n")),
        None => {
            s.push_str(&format!("Response: {} {} in {} ms\n```\n", r.status, r.reason, r.millis));
            for (k, v) in r.headers.iter().take(15) {
                s.push_str(&format!("{k}: {v}\n"));
            }
            let body = r.pretty_body();
            let cut: String = body.chars().take(4000).collect();
            s.push_str(&format!("\n{cut}{}\n```\n", if body.chars().count() > 4000 { "\n…(cut)" } else { "" }));
        }
    }
    s
}

/// The response panel: the current response and earlier ones.
#[derive(Default)]
pub struct Panel {
    pub history: Vec<Response>,
    pub shown: usize,
    pub pending: Option<(Request, Receiver<Response>)>,
    headers_tab: bool,
}

impl Panel {
    pub fn start(&mut self, req: Request, remote: Option<RemoteSpec>) {
        let rx = send(req.clone(), remote);
        self.pending = Some((req, rx));
    }

    pub fn poll(&mut self) -> bool {
        let Some((_, rx)) = &self.pending else { return false };
        match rx.try_recv() {
            Ok(r) => {
                self.history.insert(0, r);
                self.history.truncate(30);
                self.shown = 0;
                self.pending = None;
                true
            }
            Err(_) => false,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, p: &Palette, font_size: f32) -> Option<Action> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("HTTP").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").clicked() {
                    action = Some(Action::Close);
                }
                if self.history.len() > 1 {
                    let label = |r: &Response| format!("{} {} → {}", r.request.method, short(&r.request.url), r.status);
                    egui::ComboBox::from_id_salt("http-history")
                        .width(ui.available_width().min(260.0))
                        .selected_text(self.history.get(self.shown).map(label).unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for (i, r) in self.history.iter().enumerate() {
                                ui.selectable_value(&mut self.shown, i, label(r));
                            }
                        });
                }
            });
        });
        if let Some((req, _)) = &self.pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(format!("{} {}", req.method, req.url)).small().color(p.text_dim));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let Some(r) = self.history.get(self.shown) else {
            if self.pending.is_none() {
                ui.label(
                    RichText::new("Put the caret in a request of a .http file and send it (Ctrl+Alt+Enter).")
                        .color(p.text_dim),
                );
            }
            return action;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{} {}", r.request.method, r.request.url)).small().monospace().color(p.text_dim),
            );
        });
        if let Some(e) = &r.error {
            ui.label(RichText::new(e).color(p.error));
            if ui.button("✳ Ask AI").clicked() {
                action = Some(Action::AskAi(ai_prompt(r)));
            }
        } else {
            ui.horizontal(|ui| {
                let color = match r.status {
                    200..=299 => p.success,
                    300..=399 => p.accent,
                    400..=499 => p.warning,
                    _ => p.error,
                };
                ui.label(RichText::new(format!("{} {}", r.status, r.reason)).strong().color(color));
                ui.label(
                    RichText::new(format!("{} ms · {}", r.millis, size(r.body.len()))).small().color(p.text_faint),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("✳ Ask AI")
                        .on_hover_text("Send the request and response to the AI agent")
                        .clicked()
                    {
                        action = Some(Action::AskAi(ai_prompt(r)));
                    }
                    if ui.small_button("↻ Send again").clicked() {
                        action = Some(Action::Resend(r.request.clone()));
                    }
                    if ui.small_button("Open").on_hover_text("Open the body in an editor tab").clicked() {
                        action = Some(Action::OpenBody(r.pretty_body()));
                    }
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(r.body.clone());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.headers_tab, false, "Body");
                ui.selectable_value(&mut self.headers_tab, true, format!("Headers ({})", r.headers.len()));
            });
            ui.separator();
            let font = egui::FontId::monospace(font_size * 0.92);
            egui::ScrollArea::both().id_salt(("http-body", self.shown)).auto_shrink([false, false]).show(ui, |ui| {
                if self.headers_tab {
                    egui::Grid::new("http-headers").striped(true).show(ui, |ui| {
                        for (k, v) in &r.headers {
                            ui.label(RichText::new(k).font(font.clone()).color(p.accent));
                            ui.label(RichText::new(v).font(font.clone()));
                            ui.end_row();
                        }
                    });
                } else {
                    let mut body = r.pretty_body();
                    ui.add(
                        egui::TextEdit::multiline(&mut body)
                            .font(font)
                            .code_editor()
                            .desired_width(f32::INFINITY)
                            .interactive(true),
                    );
                }
            });
        }
        action
    }
}

fn short(u: &str) -> String {
    let u = u.split("://").nth(1).unwrap_or(u);
    if u.chars().count() > 40 {
        format!("{}…", u.chars().take(39).collect::<String>())
    } else {
        u.to_string()
    }
}

fn size(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1048576.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "@host = localhost:8000\n@base = http://{{host}}/api\n@token = abc\n\n### List users\nGET {{base}}/users?limit=2\nAccept: application/json\nAuthorization: Bearer {{token}}\n\n### Create user\n# a comment\nPOST {{base}}/users\nContent-Type: application/json\n\n{\n  \"name\": \"{{who}}\"\n}\n\n###\nhttps://example.com\n";

    #[test]
    fn parses_requests() {
        let env = |k: &str| (k == "who").then(|| "Ali".to_string());
        let r = request_at(FILE, 6, &env).unwrap();
        assert_eq!((r.name.as_str(), r.method.as_str()), ("List users", "GET"));
        assert_eq!(r.url, "http://localhost:8000/api/users?limit=2");
        assert_eq!(r.headers[1], ("Authorization".into(), "Bearer abc".into()));
        assert!(r.body.is_empty());
        let r = request_at(FILE, 14, &env).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body, "{\n  \"name\": \"Ali\"\n}");
        let r = request_at(FILE, 20, &env).unwrap();
        assert_eq!((r.method.as_str(), r.url.as_str()), ("GET", "https://example.com"));
        // Unknown variables stay visible.
        assert_eq!(substitute("{{nope}}/x", &HashMap::new(), &|_| None), "{{nope}}/x");
    }

    #[test]
    fn parses_curl_output() {
        let raw = b"HTTP/1.1 301 Moved\r\nLocation: /b\r\n\r\nHTTP/2 200 \r\ncontent-type: application/json\r\nx-a: 1\r\n\r\n{\"ok\":true}";
        let (status, _, headers, body) = parse_response(raw);
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 2);
        assert_eq!(body, "{\"ok\":true}");
        let r = Response { headers, body, ..Default::default() };
        assert_eq!(r.pretty_body(), "{\n  \"ok\": true\n}");
        let req = Request {
            method: "GET".into(),
            url: "http://x".into(),
            headers: vec![("Authorization".into(), "Bearer s3cret".into())],
            ..Default::default()
        };
        let p = ai_prompt(&Response { request: req, status: 500, ..r });
        assert!(p.contains("Authorization: •••") && !p.contains("s3cret") && p.contains("500"));
    }
}
