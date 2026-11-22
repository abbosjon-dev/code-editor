//! Built-in commands (shown in the command palette and menus).

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

static LABELS: LazyLock<RwLock<HashMap<Cmd, String>>> = LazyLock::new(Default::default);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmd {
    NewFile,
    OpenFile,
    OpenFolder,
    AddFolderToWorkspace,
    Save,
    SaveAs,
    SaveAll,
    CloseTab,
    CloseAll,
    QuickOpen,
    CommandPalette,
    GotoLine,
    Find,
    Replace,
    FindInFiles,
    ToggleSidebar,
    ToggleTerminal,
    ToggleAiPanel,
    NewTerminal,
    SplitTerminal,
    NextTab,
    PrevTab,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleMinimap,
    ToggleWordWrap,
    SelectTheme,
    SelectSyntaxTheme,
    SelectLanguage,
    OpenSettings,
    OpenSettingsFile,
    OpenKeybindings,
    ToggleVimMode,
    OpenPluginsFolder,
    ReloadPlugins,
    AskAgent,
    SendSelectionToAgent,
    SendFileToAgent,
    ReviewAgentChanges,
    ShowPlugins,
    ShowExplorer,
    ShowAgents,
    ShowSourceControl,
    ShowProblems,
    GitOpenChanges,
    MarkdownPreview,
    GitSwitchBranch,
    ReloadFile,
    GotoDefinition,
    NextProblem,
    RestartLanguageServers,
    SplitEditor,
    FocusOtherPane,
    RenameSymbol,
    FindReferences,
    FormatDocument,
    Fold,
    Unfold,
    FoldAll,
    UnfoldAll,
    CodeActions,
    GotoSymbol,
    TriggerParameterHints,
    DebugStart,
    DebugStop,
    DebugRestart,
    DebugPause,
    DebugStepOver,
    DebugStepInto,
    DebugStepOut,
    ToggleBreakpoint,
    ShowDebug,
    OpenLaunchConfig,
    About,
    Quit,
}

impl Cmd {
    pub fn all() -> &'static [Cmd] {
        use Cmd::*;
        &[
            NewFile,
            OpenFile,
            OpenFolder,
            AddFolderToWorkspace,
            Save,
            SaveAs,
            SaveAll,
            CloseTab,
            CloseAll,
            QuickOpen,
            CommandPalette,
            GotoLine,
            Find,
            Replace,
            FindInFiles,
            ToggleSidebar,
            ToggleTerminal,
            ToggleAiPanel,
            NewTerminal,
            SplitTerminal,
            NextTab,
            PrevTab,
            ZoomIn,
            ZoomOut,
            ZoomReset,
            ToggleMinimap,
            ToggleWordWrap,
            SelectTheme,
            SelectSyntaxTheme,
            SelectLanguage,
            OpenSettings,
            OpenSettingsFile,
            OpenKeybindings,
            ToggleVimMode,
            OpenPluginsFolder,
            ReloadPlugins,
            AskAgent,
            SendSelectionToAgent,
            SendFileToAgent,
            ReviewAgentChanges,
            ShowPlugins,
            ShowExplorer,
            ShowAgents,
            ShowSourceControl,
            ShowProblems,
            GitOpenChanges,
            MarkdownPreview,
            GitSwitchBranch,
            ReloadFile,
            GotoDefinition,
            NextProblem,
            RestartLanguageServers,
            SplitEditor,
            FocusOtherPane,
            RenameSymbol,
            FindReferences,
            FormatDocument,
            Fold,
            Unfold,
            FoldAll,
            UnfoldAll,
            CodeActions,
            GotoSymbol,
            TriggerParameterHints,
            DebugStart,
            DebugStop,
            DebugRestart,
            DebugPause,
            DebugStepOver,
            DebugStepInto,
            DebugStepOut,
            ToggleBreakpoint,
            ShowDebug,
            OpenLaunchConfig,
            About,
            Quit,
        ]
    }

    pub fn title(self) -> &'static str {
        use Cmd::*;
        match self {
            NewFile => "File: New File",
            CodeActions => "Edit: Quick Fix / Code Actions…",
            GotoSymbol => "Go: Go to Symbol in File…",
            TriggerParameterHints => "Edit: Trigger Parameter Hints",
            Fold => "Editor: Fold",
            Unfold => "Editor: Unfold",
            FoldAll => "Editor: Fold All",
            UnfoldAll => "Editor: Unfold All",
            RenameSymbol => "Edit: Rename Symbol",
            FindReferences => "Go: Find All References",
            FormatDocument => "Edit: Format Document",
            SplitEditor => "View: Split Editor (toggle)",
            FocusOtherPane => "View: Focus Other Editor Pane",
            ShowSourceControl => "View: Show Source Control",
            ShowProblems => "View: Show Problems",
            GitOpenChanges => "Git: Open Changes (diff with HEAD)",
            MarkdownPreview => "Markdown: Toggle Preview",
            GitSwitchBranch => "Git: Switch / Create Branch…",
            GotoDefinition => "Go: Go to Definition",
            NextProblem => "Go: Next Problem",
            RestartLanguageServers => "Language Servers: Restart",
            OpenFile => "File: Open File…",
            OpenFolder => "File: Open Folder…",
            AddFolderToWorkspace => "File: Add Folder to Workspace…",
            Save => "File: Save",
            SaveAs => "File: Save As…",
            SaveAll => "File: Save All",
            CloseTab => "View: Close Editor",
            CloseAll => "View: Close All Editors",
            QuickOpen => "Go: Quick Open File…",
            CommandPalette => "View: Command Palette",
            GotoLine => "Go: Go to Line…",
            Find => "Edit: Find",
            Replace => "Edit: Replace",
            FindInFiles => "Search: Find in Files",
            ToggleSidebar => "View: Toggle Sidebar",
            ToggleTerminal => "View: Toggle Terminal",
            ToggleAiPanel => "View: Toggle AI Agent Panel",
            NewTerminal => "Terminal: New Terminal",
            SplitTerminal => "Terminal: Split Terminal (toggle)",
            NextTab => "View: Next Editor",
            PrevTab => "View: Previous Editor",
            ZoomIn => "View: Zoom In",
            ZoomOut => "View: Zoom Out",
            ZoomReset => "View: Reset Zoom",
            ToggleMinimap => "View: Toggle Minimap",
            ToggleWordWrap => "View: Toggle Word Wrap",
            SelectTheme => "Preferences: Color Theme",
            SelectSyntaxTheme => "Preferences: Syntax Theme",
            SelectLanguage => "Editor: Change Language Mode",
            OpenSettings => "Preferences: Open Settings",
            OpenSettingsFile => "Preferences: Open Settings (TOML)",
            OpenKeybindings => "Preferences: Keyboard Shortcuts (TOML)",
            ToggleVimMode => "Preferences: Toggle Vim Mode",
            OpenPluginsFolder => "Plugins: Open Plugins Folder",
            ReloadPlugins => "Plugins: Reload Plugins",
            AskAgent => "AI: Ask Agent About Selection…",
            SendSelectionToAgent => "AI: Send Selection to Agent",
            SendFileToAgent => "AI: Send Current File to Agent",
            ReviewAgentChanges => "AI: Review All Agent Changes",
            ShowPlugins => "View: Show Plugins",
            ShowExplorer => "View: Show Explorer",
            ShowAgents => "AI: Show Agents",
            ReloadFile => "File: Revert / Reload From Disk",
            DebugStart => "Debug: Start / Continue",
            DebugStop => "Debug: Stop",
            DebugRestart => "Debug: Restart",
            DebugPause => "Debug: Pause",
            DebugStepOver => "Debug: Step Over",
            DebugStepInto => "Debug: Step Into",
            DebugStepOut => "Debug: Step Out",
            ToggleBreakpoint => "Debug: Toggle Breakpoint",
            ShowDebug => "View: Show Run and Debug",
            OpenLaunchConfig => "Debug: Open launch.toml",
            About => "Help: About Zenith",
            Quit => "File: Quit",
        }
    }

    /// Stable id used in keybindings.toml: the variant name in snake_case.
    pub fn id(self) -> String {
        let name = format!("{self:?}");
        let mut out = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if i > 0 {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    pub fn from_id(id: &str) -> Option<Cmd> {
        Cmd::all().iter().copied().find(|c| c.id() == id)
    }

    /// Shortcut shown in menus and the palette (user bindings included).
    pub fn shortcut(self) -> String {
        if let Some(s) = LABELS.read().ok().and_then(|m| m.get(&self).cloned()) {
            return s;
        }
        self.default_shortcut().to_string()
    }

    /// Replace the shown shortcuts after keybindings.toml changed.
    pub fn set_labels(labels: HashMap<Cmd, String>) {
        if let Ok(mut m) = LABELS.write() {
            *m = labels;
        }
    }

    pub fn default_shortcut(self) -> &'static str {
        use Cmd::*;
        let mac = cfg!(target_os = "macos");
        macro_rules! k {
            ($m:literal, $o:literal) => {
                if mac {
                    $m
                } else {
                    $o
                }
            };
        }
        match self {
            NewFile => k!("⌘N", "Ctrl+N"),
            OpenFile => k!("⌘O", "Ctrl+O"),
            OpenFolder => k!("⌘⇧O", "Ctrl+Shift+O"),
            Save => k!("⌘S", "Ctrl+S"),
            SaveAs => k!("⌘⇧S", "Ctrl+Shift+S"),
            SaveAll => k!("⌘⌥S", "Ctrl+Alt+S"),
            CloseTab => k!("⌘W", "Ctrl+W"),
            QuickOpen => k!("⌘P", "Ctrl+P"),
            CommandPalette => k!("⌘⇧P", "Ctrl+Shift+P"),
            GotoLine => k!("⌘G", "Ctrl+G"),
            Find => k!("⌘F", "Ctrl+F"),
            Replace => k!("⌘H", "Ctrl+H"),
            FindInFiles => k!("⌘⇧F", "Ctrl+Shift+F"),
            ToggleSidebar => k!("⌘B", "Ctrl+B"),
            ToggleTerminal => k!("⌃`", "Ctrl+`"),
            ToggleAiPanel => k!("⌘⇧A", "Ctrl+Shift+A"),
            NewTerminal => k!("⌃⇧`", "Ctrl+Shift+`"),
            NextTab => k!("⌃Tab", "Ctrl+Tab"),
            PrevTab => k!("⌃⇧Tab", "Ctrl+Shift+Tab"),
            ZoomIn => k!("⌘=", "Ctrl+="),
            ZoomOut => k!("⌘-", "Ctrl+-"),
            ZoomReset => k!("⌘0", "Ctrl+0"),
            OpenSettings => k!("⌘,", "Ctrl+,"),
            AskAgent => k!("⌘⇧K", "Ctrl+Shift+I"),
            SendSelectionToAgent => k!("⌘⇧L", "Ctrl+Shift+L"),
            Quit => k!("⌘Q", "Ctrl+Q"),
            GotoDefinition => "F12",
            MarkdownPreview => k!("⌘⌥P", "Ctrl+Alt+P"),
            ToggleWordWrap => k!("⌥Z", "Alt+Z"),
            CodeActions => k!("⌘.", "Ctrl+."),
            GotoSymbol => k!("⌘T", "Ctrl+T"),
            TriggerParameterHints => k!("⌘⇧Space", "Ctrl+Shift+Space"),
            Fold => k!("⌘⇧[", "Ctrl+Shift+["),
            Unfold => k!("⌘⇧]", "Ctrl+Shift+]"),
            RenameSymbol => "F2",
            FindReferences => "Shift+F12",
            FormatDocument => k!("⇧⌥F", "Shift+Alt+F"),
            SplitEditor => k!("⌘\\", "Ctrl+\\"),
            FocusOtherPane => k!("⌃⌥\\", "Ctrl+Alt+\\"),
            ShowSourceControl => k!("⌃⇧G", "Ctrl+Shift+G"),
            ShowProblems => k!("⌘⇧M", "Ctrl+Shift+M"),
            NextProblem => "F8",
            DebugStart => "F5",
            DebugStop => "Shift+F5",
            DebugRestart => k!("⌘⇧F5", "Ctrl+Shift+F5"),
            DebugPause => "F6",
            DebugStepOver => "F10",
            DebugStepInto => "F11",
            DebugStepOut => "Shift+F11",
            ToggleBreakpoint => "F9",
            _ => "",
        }
    }
}
