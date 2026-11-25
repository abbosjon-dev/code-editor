//! Inline AI edit (Ctrl+K): an instruction for the selected code (or the
//! caret position), answered by the default agent CLI in its one-shot mode
//! (`claude -p`, `codex exec`, ...). The answer is shown as a diff and only
//! changes the file when accepted.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};

use zenith_ai::AgentProfile;

/// Lines of surrounding code sent along for context.
const CONTEXT_LINES: usize = 40;

/// The command line for one prompt, or `None` if the agent has no one-shot mode.
pub fn oneshot_command(agent: &AgentProfile, prompt: &str) -> Option<Vec<String>> {
    if agent.oneshot_args.is_empty() {
        return None;
    }
    let mut argv = vec![agent.command.clone()];
    argv.extend(agent.args.iter().cloned());
    argv.extend(agent.oneshot_args.iter().map(|a| a.replace("{prompt}", prompt)));
    Some(argv)
}

pub fn build_prompt(file: &str, language: &str, before: &str, code: &str, after: &str, instruction: &str) -> String {
    let task = if code.trim().is_empty() {
        "Write new code to insert where the CURSOR marker is, following the instruction."
    } else {
        "Rewrite the code inside the SELECTION tags, following the instruction."
    };
    let middle = if code.is_empty() { "<CURSOR/>".to_string() } else { format!("<SELECTION>{code}</SELECTION>") };
    format!(
        "You are editing {file} ({language}). {task}\n\
         Answer with ONLY the code that replaces the selection (or is inserted), nothing else: \
         no explanation, no markdown fences, keep the surrounding indentation. Don't modify any files.\n\n\
         Instruction: {instruction}\n\n\
         {before}{middle}{after}"
    )
}

/// The agent's answer as plain code: no ``` fences, and ending with a
/// newline exactly when the original did.
pub fn clean_answer(out: &str, original: &str) -> String {
    let mut text = out.trim_matches('\n').to_string();
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() >= 2 && lines[0].trim_start().starts_with("```") && lines.last().is_some_and(|l| l.trim() == "```") {
        text = lines[1..lines.len() - 1].join("\n");
    }
    let text = text.trim_end().to_string();
    if original.ends_with('\n') || (original.is_empty() && !text.is_empty()) {
        text + "\n"
    } else {
        text
    }
}

/// What the worker reports while the agent answers.
pub enum Msg {
    /// The answer so far (raw).
    Partial(String),
    Done(Result<String, String>),
}

/// One line of Claude Code's `stream-json` output: a text delta, the final
/// result, or nothing of interest.
enum Event {
    Delta(String),
    Result(Result<String, String>),
    Other,
}

fn parse_stream_json(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return Event::Other };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("stream_event") => {
            let d = &v["event"]["delta"];
            match (v["event"]["type"].as_str(), d["type"].as_str(), d["text"].as_str()) {
                (Some("content_block_delta"), Some("text_delta"), Some(t)) => Event::Delta(t.to_string()),
                _ => Event::Other,
            }
        }
        Some("result") => {
            let text = v["result"].as_str().unwrap_or("").to_string();
            if v["is_error"].as_bool() == Some(true) {
                Event::Result(Err(if text.is_empty() { "the agent reported an error".into() } else { text }))
            } else {
                Event::Result(Ok(text))
            }
        }
        _ => Event::Other,
    }
}

/// A partial answer shown while it streams: without an opening ``` line
/// (and a closing one if it already arrived).
pub fn clean_partial(raw: &str) -> String {
    let mut t = raw.trim_start_matches('\n');
    if t.trim_start().starts_with("```") {
        t = t.split_once('\n').map_or("", |(_, rest)| rest);
    }
    if let Some(i) = t.rfind("\n```") {
        if t[i + 4..].trim().is_empty() {
            t = &t[..=i];
        }
    }
    t.to_string()
}

pub enum State {
    Editing,
    Asking { rx: Receiver<Msg>, child: Arc<Mutex<Option<Child>>>, partial: String },
    Ready(String),
    Failed(String),
}

pub struct InlineEdit {
    pub doc_id: u64,
    /// Char range of the selection (empty: insert at `start`).
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub file: String,
    pub language: String,
    pub before: String,
    pub after: String,
    pub instruction: String,
    pub state: State,
    pub agent_name: String,
    pub focus: bool,
}

impl InlineEdit {
    pub fn new(doc: &crate::document::Document, file: String) -> Self {
        let b = &doc.buffer;
        let (start, end) = (b.sel.start(), b.sel.end());
        let rope = b.rope();
        let original = rope.slice(start..end).to_string();
        let first = rope.char_to_line(start);
        let last = rope.char_to_line(end);
        let from = rope.line_to_char(first.saturating_sub(CONTEXT_LINES));
        let to_line = (last + CONTEXT_LINES + 1).min(rope.len_lines());
        let to = if to_line >= rope.len_lines() { rope.len_chars() } else { rope.line_to_char(to_line) };
        Self {
            doc_id: doc.id,
            start,
            end,
            original,
            file,
            language: doc.language().to_string(),
            before: rope.slice(from..start).to_string(),
            after: rope.slice(end..to).to_string(),
            instruction: String::new(),
            state: State::Editing,
            agent_name: String::new(),
            focus: true,
        }
    }

    /// Ask `argv` (built by the caller; maybe through ssh) in `cwd`;
    /// `format` is the agent's `oneshot_format`.
    pub fn ask(&mut self, argv: Vec<String>, cwd: Option<PathBuf>, format: &str, waker: Arc<dyn Fn() + Send + Sync>) {
        let (tx, rx) = channel();
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let slot = child.clone();
        let original = self.original.clone();
        let json = format == "claude-stream-json";
        std::thread::spawn(move || {
            let r = (|| {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
                if let Some(d) = &cwd {
                    c.current_dir(d);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    c.creation_flags(0x0800_0000);
                }
                let ch = c.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
                let pipes = {
                    let mut g = slot.lock().unwrap();
                    *g = Some(ch);
                    g.as_mut().and_then(|c| c.stdout.take()).zip(g.as_mut().and_then(|c| c.stderr.take()))
                };
                let (so, mut se) = pipes.ok_or("no output")?;
                let err = std::thread::spawn(move || {
                    let mut s = String::new();
                    let _ = std::io::Read::read_to_string(&mut se, &mut s);
                    s
                });
                // Read the answer as it comes, reporting it so far.
                let mut text = String::new();
                let mut result: Option<Result<String, String>> = None;
                if json {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(so).lines().map_while(Result::ok) {
                        match parse_stream_json(&line) {
                            Event::Delta(d) => {
                                text.push_str(&d);
                                let _ = tx.send(Msg::Partial(text.clone()));
                                waker();
                            }
                            Event::Result(r) => result = Some(r),
                            Event::Other => {}
                        }
                    }
                } else {
                    let mut so = so;
                    let mut bytes = vec![];
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = std::io::Read::read(&mut so, &mut buf) {
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        text = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = tx.send(Msg::Partial(text.clone()));
                        waker();
                    }
                }
                let err = err.join().unwrap_or_default();
                let status = slot.lock().unwrap().take().map(|mut c| c.wait());
                match (status, result) {
                    (None, _) => Err("cancelled".into()),
                    (Some(_), Some(Err(e))) => Err(e),
                    (Some(Ok(s)), r) if s.success() => {
                        // The final result is authoritative; else what streamed.
                        let full = match r {
                            Some(Ok(t)) if !t.is_empty() => t,
                            _ => text,
                        };
                        Ok(clean_answer(&full, &original))
                    }
                    (Some(_), _) => {
                        Err(if err.trim().is_empty() { text.trim().to_string() } else { err.trim().to_string() })
                    }
                }
            })();
            let _ = tx.send(Msg::Done(r));
            waker();
        });
        self.state = State::Asking { rx, child, partial: String::new() };
    }

    pub fn cancel(&mut self) {
        if let State::Asking { child, .. } = &self.state {
            if let Some(mut c) = child.lock().unwrap().take() {
                let _ = c.kill();
            }
        }
        self.state = State::Editing;
    }

    pub fn poll(&mut self) {
        let mut done = None;
        if let State::Asking { rx, partial, .. } = &mut self.state {
            for m in rx.try_iter() {
                match m {
                    Msg::Partial(p) => *partial = p,
                    Msg::Done(r) => done = Some(r),
                }
            }
        }
        if let Some(r) = done {
            self.state = match r {
                Ok(t) if t.trim().is_empty() && !self.original.is_empty() => {
                    State::Failed("The agent answered with no code".into())
                }
                Ok(t) => State::Ready(t),
                Err(e) => State::Failed(e),
            };
        }
    }

    pub fn prompt(&self) -> String {
        build_prompt(&self.file, &self.language, &self.before, &self.original, &self.after, &self.instruction)
    }
}

impl Drop for InlineEdit {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_command_and_answer() {
        let a = zenith_ai::builtin_agents().into_iter().find(|a| a.id == "claude").unwrap();
        let argv = oneshot_command(&a, "do it").unwrap();
        assert_eq!(argv[..3], ["claude", "-p", "do it"]);
        assert!(argv.contains(&"stream-json".to_string()));
        let aider = zenith_ai::builtin_agents().into_iter().find(|a| a.id == "aider").unwrap();
        assert!(oneshot_command(&aider, "x").is_none());

        let p = build_prompt("src/a.rs", "Rust", "fn a() {\n", "    1\n", "}\n", "return 2");
        assert!(p.contains("<SELECTION>    1\n</SELECTION>") && p.contains("Instruction: return 2"));
        assert!(build_prompt("a", "Rust", "x", "", "y", "add").contains("x<CURSOR/>y"));

        assert_eq!(clean_answer("```rust\n    2\n```\n", "    1\n"), "    2\n");
        assert_eq!(clean_answer("\n    2\n\n", "    1"), "    2");
        assert_eq!(clean_answer("let x = 1;", ""), "let x = 1;\n");
    }

    #[test]
    fn stream_json_and_partials() {
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"let x"}}}"#;
        assert!(matches!(parse_stream_json(delta), Event::Delta(t) if t == "let x"));
        let res = r#"{"type":"result","subtype":"success","is_error":false,"result":"let x = 2;"}"#;
        assert!(matches!(parse_stream_json(res), Event::Result(Ok(t)) if t == "let x = 2;"));
        let bad = r#"{"type":"result","is_error":true,"result":"rate limited"}"#;
        assert!(matches!(parse_stream_json(bad), Event::Result(Err(t)) if t == "rate limited"));
        assert!(matches!(parse_stream_json(r#"{"type":"system"}"#), Event::Other));
        assert_eq!(clean_partial("```rust\nlet x = 1;\nlet y"), "let x = 1;\nlet y");
        assert_eq!(clean_partial("```\nlet x = 1;\n```"), "let x = 1;\n");
        assert_eq!(clean_partial("let x"), "let x");
    }

    /// Claude-style streaming JSON from a fake CLI: partials, then the result.
    #[cfg(unix)]
    #[test]
    fn streams_claude_json() {
        let mut doc = crate::document::Document::new_untitled(1, 1, crate::highlight::load_theme("OneHalfDark"));
        doc.buffer.set_text("x\n");
        let mut e = InlineEdit::new(&doc, "a.rs".into());
        e.original = "    1\n".into();
        let script = r#"
d() { printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"%s"}}}\n' "$1"; }
echo '{"type":"system","subtype":"init"}'; d '    2'; sleep 0.3; d '0'; sleep 0.3
echo '{"type":"result","is_error":false,"result":"    20"}'"#;
        e.ask(vec!["sh".into(), "-c".into(), script.into()], None, "claude-stream-json", Arc::new(|| {}));
        let t = std::time::Instant::now();
        let mut seen_partial = false;
        while matches!(e.state, State::Asking { .. }) && t.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            e.poll();
            if let State::Asking { partial, .. } = &e.state {
                seen_partial |= partial == "    2";
            }
        }
        assert!(seen_partial, "no partial answer seen");
        assert!(
            matches!(&e.state, State::Ready(t) if t == "    20\n"),
            "{}",
            match &e.state {
                State::Failed(m) => m.clone(),
                _ => String::new(),
            }
        );
    }

    /// A fake agent CLI answers; the result arrives through `poll`.
    #[cfg(unix)]
    #[test]
    fn asks_a_cli() {
        let mut doc = crate::document::Document::new_untitled(1, 1, crate::highlight::load_theme("OneHalfDark"));
        doc.buffer.set_text("fn a() {\n    1\n}\n");
        let mut e = InlineEdit::new(&doc, "a.rs".into());
        e.original = "    1\n".into();
        e.ask(vec!["sh".into(), "-c".into(), "printf '```\\n    2\\n```\\n'".into()], None, "", Arc::new(|| {}));
        let t = std::time::Instant::now();
        while matches!(e.state, State::Asking { .. }) && t.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            e.poll();
        }
        assert!(matches!(&e.state, State::Ready(t) if t == "    2\n"));
        e.ask(vec!["sh".into(), "-c".into(), "echo nope >&2; exit 3".into()], None, "", Arc::new(|| {}));
        let t = std::time::Instant::now();
        while matches!(e.state, State::Asking { .. }) && t.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            e.poll();
        }
        assert!(matches!(&e.state, State::Failed(m) if m == "nope"));
    }
}
