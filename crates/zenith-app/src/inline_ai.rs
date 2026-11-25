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

pub enum State {
    Editing,
    Asking { rx: Receiver<Result<String, String>>, child: Arc<Mutex<Option<Child>>> },
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

    /// Ask `argv` (built by the caller; maybe through ssh) in `cwd`.
    pub fn ask(&mut self, argv: Vec<String>, cwd: Option<PathBuf>, waker: Arc<dyn Fn() + Send + Sync>) {
        let (tx, rx) = channel();
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let slot = child.clone();
        let original = self.original.clone();
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
                let pid_out = {
                    let mut g = slot.lock().unwrap();
                    *g = Some(ch);
                    g.as_mut().and_then(|c| c.stdout.take()).zip(g.as_mut().and_then(|c| c.stderr.take()))
                };
                let (mut so, mut se) = pid_out.ok_or("no output")?;
                let err = std::thread::spawn(move || {
                    let mut s = String::new();
                    let _ = std::io::Read::read_to_string(&mut se, &mut s);
                    s
                });
                let mut out = String::new();
                let _ = std::io::Read::read_to_string(&mut so, &mut out);
                let err = err.join().unwrap_or_default();
                let status = slot.lock().unwrap().take().map(|mut c| c.wait());
                match status {
                    Some(Ok(s)) if s.success() => Ok(clean_answer(&out, &original)),
                    Some(_) => Err(if err.trim().is_empty() { out.trim().to_string() } else { err.trim().to_string() }),
                    None => Err("cancelled".into()),
                }
            })();
            let _ = tx.send(r);
            waker();
        });
        self.state = State::Asking { rx, child };
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
        if let State::Asking { rx, .. } = &self.state {
            if let Ok(r) = rx.try_recv() {
                self.state = match r {
                    Ok(t) if t.trim().is_empty() && !self.original.is_empty() => {
                        State::Failed("The agent answered with no code".into())
                    }
                    Ok(t) => State::Ready(t),
                    Err(e) => State::Failed(e),
                };
            }
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
        assert_eq!(argv, ["claude", "-p", "do it"]);
        let aider = zenith_ai::builtin_agents().into_iter().find(|a| a.id == "aider").unwrap();
        assert!(oneshot_command(&aider, "x").is_none());

        let p = build_prompt("src/a.rs", "Rust", "fn a() {\n", "    1\n", "}\n", "return 2");
        assert!(p.contains("<SELECTION>    1\n</SELECTION>") && p.contains("Instruction: return 2"));
        assert!(build_prompt("a", "Rust", "x", "", "y", "add").contains("x<CURSOR/>y"));

        assert_eq!(clean_answer("```rust\n    2\n```\n", "    1\n"), "    2\n");
        assert_eq!(clean_answer("\n    2\n\n", "    1"), "    2");
        assert_eq!(clean_answer("let x = 1;", ""), "let x = 1;\n");
    }

    /// A fake agent CLI answers; the result arrives through `poll`.
    #[cfg(unix)]
    #[test]
    fn asks_a_cli() {
        let mut doc = crate::document::Document::new_untitled(1, 1, crate::highlight::load_theme("OneHalfDark"));
        doc.buffer.set_text("fn a() {\n    1\n}\n");
        let mut e = InlineEdit::new(&doc, "a.rs".into());
        e.original = "    1\n".into();
        e.ask(vec!["sh".into(), "-c".into(), "printf '```\\n    2\\n```\\n'".into()], None, Arc::new(|| {}));
        let t = std::time::Instant::now();
        while matches!(e.state, State::Asking { .. }) && t.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            e.poll();
        }
        assert!(matches!(&e.state, State::Ready(t) if t == "    2\n"));
        e.ask(vec!["sh".into(), "-c".into(), "echo nope >&2; exit 3".into()], None, Arc::new(|| {}));
        let t = std::time::Instant::now();
        while matches!(e.state, State::Asking { .. }) && t.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            e.poll();
        }
        assert!(matches!(&e.state, State::Failed(m) if m == "nope"));
    }
}
