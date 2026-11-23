//! AI agent integration.
//!
//! Zenith does not re-implement agents: it runs the real agent CLIs
//! (Claude Code, OpenAI Codex, Gemini CLI, Aider, ...) inside its built-in
//! terminal, rooted at the open workspace, and gives you shortcuts to send
//! the current file / selection to them. Any CLI can be added via settings.

pub mod mcp;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentProfile {
    /// Stable id, e.g. "claude".
    pub id: String,
    /// Display name, e.g. "Claude Code".
    pub name: String,
    /// Executable to run.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables (e.g. API base URLs). Secrets are better
    /// kept in your shell environment, which agents inherit.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// How to install the CLI, shown when it is missing.
    #[serde(default)]
    pub install_hint: String,
    /// Short icon/glyph shown in the UI.
    #[serde(default = "default_icon")]
    pub icon: String,
    /// Extra arguments to continue the previous conversation when the
    /// editor restores a session (e.g. `--continue` for Claude Code).
    #[serde(default)]
    pub resume_args: Vec<String>,
}

fn default_icon() -> String {
    "AI".into()
}

impl AgentProfile {
    fn builtin(id: &str, name: &str, command: &str, icon: &str, hint: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            command: command.into(),
            args: vec![],
            env: vec![],
            install_hint: hint.into(),
            icon: icon.into(),
            resume_args: vec![],
        }
    }

    /// Whether the executable can be found on PATH.
    pub fn is_installed(&self) -> bool {
        which::which(&self.command).is_ok()
    }
}

/// The agents Zenith knows about out of the box.
pub fn builtin_agents() -> Vec<AgentProfile> {
    vec![
        AgentProfile {
            resume_args: vec!["--continue".into()],
            ..AgentProfile::builtin("claude", "Claude Code", "claude", "✳", "npm install -g @anthropic-ai/claude-code")
        },
        AgentProfile::builtin("codex", "OpenAI Codex", "codex", "◎", "npm install -g @openai/codex"),
        AgentProfile::builtin("gemini", "Gemini CLI", "gemini", "◈", "npm install -g @google/gemini-cli"),
        AgentProfile::builtin("aider", "Aider", "aider", "◆", "python -m pip install aider-install && aider-install"),
        AgentProfile::builtin("opencode", "OpenCode", "opencode", "◇", "npm install -g opencode-ai"),
    ]
}

/// Merge user-defined agents over the built-ins (same id = override).
pub fn merge_agents(user: &[AgentProfile]) -> Vec<AgentProfile> {
    let mut all = builtin_agents();
    for u in user {
        if let Some(existing) = all.iter_mut().find(|a| a.id == u.id) {
            *existing = u.clone();
        } else {
            all.push(u.clone());
        }
    }
    all
}

/// Builds the text sent to an agent when the user asks about code.
pub fn build_context_prompt(
    file: Option<&str>,
    line_range: Option<(usize, usize)>,
    selection: &str,
    question: &str,
) -> String {
    let mut s = String::new();
    if let Some(f) = file {
        match line_range {
            Some((a, b)) => s.push_str(&format!("In @{f} (lines {}-{}):\n", a + 1, b + 1)),
            None => s.push_str(&format!("In @{f}:\n")),
        }
    }
    if !selection.is_empty() {
        s.push_str("```\n");
        s.push_str(selection.trim_end());
        s.push_str("\n```\n");
    }
    s.push_str(question);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agents_override_builtins() {
        let mut custom = builtin_agents()[0].clone();
        custom.args = vec!["--model".into(), "opus".into()];
        let extra = AgentProfile::builtin("my", "Mine", "my-agent", "M", "");
        let all = merge_agents(&[custom.clone(), extra]);
        assert_eq!(all[0], custom);
        assert!(all.iter().any(|a| a.id == "my"));
    }

    #[test]
    fn prompt_includes_selection() {
        let p = build_context_prompt(Some("src/main.rs"), Some((0, 2)), "fn main() {}\n", "Explain this");
        assert!(p.starts_with("In @src/main.rs (lines 1-3):"));
        assert!(p.contains("```\nfn main() {}\n```"));
        assert!(p.ends_with("Explain this"));
    }
}
