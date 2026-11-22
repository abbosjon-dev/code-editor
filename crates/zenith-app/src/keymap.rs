//! User keyboard shortcuts (`keybindings.toml`).
//!
//! ```toml
//! [bindings]
//! "Ctrl+Shift+D" = "split_editor"          # bind a built-in command
//! "Alt+T" = "plugin:todo-tree.ask-ai"      # bind a plugin command
//! "Ctrl+B" = ""                            # remove a default shortcut
//! ```
//!
//! `Mod` means Ctrl on Linux/Windows and ⌘ on macOS; `Ctrl` is always Ctrl.

use std::collections::BTreeMap;
use std::path::PathBuf;

use eframe::egui::{Key, Modifiers};
use serde::Deserialize;

use crate::commands::Cmd;

/// What a key runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Cmd(Cmd),
    Plugin(String),
}

impl Target {
    pub fn parse(s: &str) -> Option<Self> {
        match s.strip_prefix("plugin:") {
            Some(id) if !id.is_empty() => Some(Target::Plugin(id.to_string())),
            Some(_) => None,
            None => Cmd::from_id(s).map(Target::Cmd),
        }
    }
}

/// One user binding; `target == None` removes the key's default binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserBinding {
    pub mods: Modifiers,
    pub key: Key,
    pub target: Option<Target>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct File {
    bindings: BTreeMap<String, String>,
}

pub fn path() -> PathBuf {
    crate::settings::config_dir().join("keybindings.toml")
}

/// Parse "Ctrl+Shift+P", "Mod+K", "Alt+Z", "F5", "Ctrl+`" (case-insensitive).
pub fn parse_chord(s: &str) -> Option<(Modifiers, Key)> {
    let s = s.trim();
    // A trailing "+" is the plus key ("Ctrl++").
    let (head, last) = match s.strip_suffix("++") {
        Some(h) => (h, "+"),
        None => match s.rsplit_once('+') {
            Some((h, l)) => (h, l),
            None => ("", s),
        },
    };
    let mut mods = Modifiers::NONE;
    for part in head.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "⌃" => mods |= Modifiers::CTRL,
            "mod" | "cmdorctrl" | "cmd" | "command" | "super" | "meta" | "⌘" => mods |= Modifiers::COMMAND,
            "shift" | "⇧" => mods |= Modifiers::SHIFT,
            "alt" | "option" | "opt" | "⌥" => mods |= Modifiers::ALT,
            _ => return None,
        }
    }
    let key = key_from_name(last.trim())?;
    Some((mods, key))
}

fn key_from_name(name: &str) -> Option<Key> {
    let alias = match name.to_ascii_lowercase().as_str() {
        "`" | "backtick" | "grave" => Some(Key::Backtick),
        "\\" | "backslash" => Some(Key::Backslash),
        "/" | "slash" => Some(Key::Slash),
        "," | "comma" => Some(Key::Comma),
        "." | "period" => Some(Key::Period),
        ";" | "semicolon" => Some(Key::Semicolon),
        "-" | "minus" => Some(Key::Minus),
        "=" | "equals" => Some(Key::Equals),
        "+" | "plus" => Some(Key::Plus),
        "[" | "openbracket" => Some(Key::OpenBracket),
        "]" | "closebracket" => Some(Key::CloseBracket),
        "'" | "quote" => Some(Key::Quote),
        "esc" | "escape" => Some(Key::Escape),
        "enter" | "return" => Some(Key::Enter),
        "space" => Some(Key::Space),
        "tab" => Some(Key::Tab),
        "del" | "delete" => Some(Key::Delete),
        "backspace" => Some(Key::Backspace),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        _ => None,
    };
    if alias.is_some() {
        return alias;
    }
    if name.len() == 1 {
        return Key::from_name(&name.to_ascii_uppercase());
    }
    Key::from_name(name).or_else(|| {
        // "f5" → "F5"
        let mut c = name.chars();
        let first = c.next()?.to_ascii_uppercase();
        Key::from_name(&format!("{first}{}", c.as_str()))
    })
}

/// Display a chord the way the platform shows shortcuts.
pub fn format_chord(mods: Modifiers, key: Key) -> String {
    let k = key.symbol_or_name();
    if cfg!(target_os = "macos") {
        let mut s = String::new();
        if mods.ctrl && !mods.mac_cmd {
            s.push('⌃');
        }
        if mods.alt {
            s.push('⌥');
        }
        if mods.shift {
            s.push('⇧');
        }
        if mods.command || mods.mac_cmd {
            s.push('⌘');
        }
        s + k
    } else {
        let mut parts = vec![];
        if mods.ctrl || mods.command {
            parts.push("Ctrl");
        }
        if mods.shift {
            parts.push("Shift");
        }
        if mods.alt {
            parts.push("Alt");
        }
        parts.push(k);
        parts.join("+")
    }
}

/// Read the user's bindings; returns them and human-readable problems.
pub fn load() -> (Vec<UserBinding>, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path()) else { return (vec![], vec![]) };
    parse_file(&text)
}

pub fn parse_file(text: &str) -> (Vec<UserBinding>, Vec<String>) {
    let file: File = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => return (vec![], vec![format!("keybindings.toml: {e}")]),
    };
    let mut out = vec![];
    let mut errors = vec![];
    for (chord, cmd) in file.bindings {
        let Some((mods, key)) = parse_chord(&chord) else {
            errors.push(format!("keybindings.toml: unknown key \"{chord}\""));
            continue;
        };
        let target = if cmd.trim().is_empty() {
            None
        } else {
            match Target::parse(cmd.trim()) {
                Some(t) => Some(t),
                None => {
                    errors.push(format!("keybindings.toml: unknown command \"{cmd}\""));
                    continue;
                }
            }
        };
        out.push(UserBinding { mods, key, target });
    }
    (out, errors)
}

/// Contents for a new keybindings.toml: instructions and every command id.
pub fn template() -> String {
    let mut s = String::from(
        "# Zenith keyboard shortcuts. Saved changes apply immediately.\n\
         #\n\
         # [bindings]\n\
         # \"Ctrl+Shift+D\" = \"split_editor\"       # bind a command\n\
         # \"Alt+T\" = \"plugin:todo-tree.ask-ai\"   # bind a plugin command\n\
         # \"Ctrl+B\" = \"\"                         # remove a default shortcut\n\
         #\n\
         # Modifiers: Ctrl, Shift, Alt, Mod (= Ctrl on Linux/Windows, Cmd on macOS).\n\
         # Keys: A-Z, 0-9, F1-F20, Enter, Space, Tab, Esc, Up/Down/Left/Right,\n\
         # Home, End, PageUp, PageDown, Delete, Backspace, ` \\ / , . ; - = [ ] '\n\
         #\n\
         # Commands (id — title — default shortcut):\n",
    );
    for c in Cmd::all() {
        let sc = c.default_shortcut();
        s.push_str(&format!(
            "#   {:<28} {}{}\n",
            c.id(),
            c.title(),
            if sc.is_empty() { String::new() } else { format!(" ({sc})") }
        ));
    }
    s.push_str("\n[bindings]\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords() {
        assert_eq!(parse_chord("Ctrl+Shift+P"), Some((Modifiers::CTRL | Modifiers::SHIFT, Key::P)));
        assert_eq!(parse_chord("mod+k"), Some((Modifiers::COMMAND, Key::K)));
        assert_eq!(parse_chord("F5"), Some((Modifiers::NONE, Key::F5)));
        assert_eq!(parse_chord("f12"), Some((Modifiers::NONE, Key::F12)));
        assert_eq!(parse_chord("Ctrl+`"), Some((Modifiers::CTRL, Key::Backtick)));
        assert_eq!(parse_chord("Ctrl++"), Some((Modifiers::CTRL, Key::Plus)));
        assert_eq!(parse_chord("Alt+Enter"), Some((Modifiers::ALT, Key::Enter)));
        assert_eq!(parse_chord("Hyper+X"), None);
        assert_eq!(parse_chord("Ctrl+Nope"), None);
    }

    #[test]
    fn file() {
        let (b, e) = parse_file(
            "[bindings]\n\"Ctrl+Shift+D\" = \"split_editor\"\n\"Ctrl+B\" = \"\"\n\"Alt+T\" = \"plugin:x.y\"\n\"Alt+Q\" = \"nope\"\n",
        );
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(b.iter().any(|x| x.target == Some(Target::Cmd(Cmd::SplitEditor))));
        assert!(b.iter().any(|x| x.key == Key::B && x.target.is_none()));
        assert!(b.iter().any(|x| x.target == Some(Target::Plugin("x.y".into()))));
        // Every command id round-trips.
        for c in Cmd::all() {
            assert_eq!(Cmd::from_id(&c.id()), Some(*c));
        }
    }
}
