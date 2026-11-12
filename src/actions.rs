use gpui::{Action, App, KeyBinding, actions};

pub mod app {
    use super::*;
    actions!(
        app,
        [
            Quit,
            OpenFolder,
            NewWindow,
            OpenSettings,
            ReloadSettings,
            ToggleTheme,
            ZoomIn,
            ZoomOut,
            ZoomReset
        ]
    );
}

pub mod workspace {
    use super::*;
    actions!(
        workspace,
        [
            ToggleCommandPalette,
            NewTerminal,
            SplitRight,
            SplitDown,
            CloseActiveTab,
            NextTab,
            PreviousTab,
            ToggleSidebar,
            FocusFiles,
            FocusGitChanges,
            GoToFile,
            OpenFile
        ]
    );
}

pub mod editor {
    use super::*;
    actions!(editor, [Save]);
}

pub mod terminal {
    use super::*;
    actions!(
        terminal,
        [
            Copy,
            Paste,
            SelectAll,
            Clear,
            ScrollPageUp,
            ScrollPageDown,
            ScrollToTop,
            ScrollToBottom,
            Find
        ]
    );

    /// Sends the escape sequence of a keystroke that an ancestor key context would otherwise
    /// intercept (e.g. Root's `tab` focus navigation).
    #[derive(Clone, PartialEq, Debug, gpui::Action)]
    #[action(namespace = terminal, no_json)]
    pub struct SendKeystroke(pub &'static str);
}

pub mod git {
    use super::*;
    actions!(
        git,
        [
            ShowGraph,
            Fetch,
            Pull,
            Push,
            Stash,
            CreateBranch,
            CheckoutBranch,
            MergeBranch,
            SelectPrevious,
            SelectNext
        ]
    );
}

pub mod agent {
    use super::*;
    actions!(
        agent,
        [NewThread, NewAcpThread, NewNativeThread, SetApiKey, Cancel]
    );
}

pub fn bind_keys(cx: &mut App) {
    const WORKSPACE: Option<&str> = Some("Workspace");
    const TERMINAL: Option<&str> = Some("Terminal");
    cx.bind_keys([
        KeyBinding::new("cmd-shift-p", workspace::ToggleCommandPalette, WORKSPACE),
        KeyBinding::new("cmd-t", workspace::NewTerminal, WORKSPACE),
        KeyBinding::new("cmd-d", workspace::SplitRight, WORKSPACE),
        KeyBinding::new("cmd-shift-d", workspace::SplitDown, WORKSPACE),
        KeyBinding::new("cmd-w", workspace::CloseActiveTab, WORKSPACE),
        KeyBinding::new("cmd-}", workspace::NextTab, WORKSPACE),
        KeyBinding::new("cmd-{", workspace::PreviousTab, WORKSPACE),
        KeyBinding::new("cmd-b", workspace::ToggleSidebar, WORKSPACE),
        KeyBinding::new("cmd-shift-e", workspace::FocusFiles, WORKSPACE),
        KeyBinding::new("ctrl-shift-g", workspace::FocusGitChanges, WORKSPACE),
        KeyBinding::new("cmd-p", workspace::GoToFile, WORKSPACE),
        KeyBinding::new("cmd-s", editor::Save, Some("Editor")),
        KeyBinding::new("cmd-o", app::OpenFolder, None),
        KeyBinding::new("cmd-shift-n", app::NewWindow, None),
        KeyBinding::new("cmd-,", app::OpenSettings, None),
        KeyBinding::new("cmd-=", app::ZoomIn, None),
        KeyBinding::new("cmd--", app::ZoomOut, None),
        KeyBinding::new("cmd-0", app::ZoomReset, None),
        KeyBinding::new("cmd-q", app::Quit, None),
        KeyBinding::new("cmd-c", terminal::Copy, TERMINAL),
        KeyBinding::new("cmd-v", terminal::Paste, TERMINAL),
        KeyBinding::new("cmd-a", terminal::SelectAll, TERMINAL),
        KeyBinding::new("cmd-k", terminal::Clear, TERMINAL),
        KeyBinding::new("shift-pageup", terminal::ScrollPageUp, TERMINAL),
        KeyBinding::new("shift-pagedown", terminal::ScrollPageDown, TERMINAL),
        KeyBinding::new("cmd-home", terminal::ScrollToTop, TERMINAL),
        KeyBinding::new("cmd-end", terminal::ScrollToBottom, TERMINAL),
        KeyBinding::new("cmd-f", terminal::Find, TERMINAL),
        KeyBinding::new("tab", terminal::SendKeystroke("tab"), TERMINAL),
        KeyBinding::new("shift-tab", terminal::SendKeystroke("shift-tab"), TERMINAL),
        KeyBinding::new("cmd-shift-g", git::ShowGraph, WORKSPACE),
        KeyBinding::new("up", git::SelectPrevious, Some("GitGraph")),
        KeyBinding::new("down", git::SelectNext, Some("GitGraph")),
        KeyBinding::new("cmd-shift-a", agent::NewThread, WORKSPACE),
        KeyBinding::new("escape", agent::Cancel, Some("Agent")),
    ]);
}

/// Commands listed in the palette, in display order.
pub fn palette_commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Terminal: New Terminal", Box::new(workspace::NewTerminal)),
        ("Terminal: Split Right", Box::new(workspace::SplitRight)),
        ("Terminal: Split Down", Box::new(workspace::SplitDown)),
        ("View: Close Tab", Box::new(workspace::CloseActiveTab)),
        ("View: Next Tab", Box::new(workspace::NextTab)),
        ("View: Previous Tab", Box::new(workspace::PreviousTab)),
        ("View: Toggle Sidebar", Box::new(workspace::ToggleSidebar)),
        ("View: Focus Files", Box::new(workspace::FocusFiles)),
        (
            "View: Focus Git Changes",
            Box::new(workspace::FocusGitChanges),
        ),
        ("File: Go to File…", Box::new(workspace::GoToFile)),
        ("File: Open File…", Box::new(workspace::OpenFile)),
        ("File: Save", Box::new(editor::Save)),
        ("File: Open Folder…", Box::new(app::OpenFolder)),
        ("Window: New Window", Box::new(app::NewWindow)),
        ("Settings: Open Settings File", Box::new(app::OpenSettings)),
        ("Settings: Reload", Box::new(app::ReloadSettings)),
        ("View: Toggle Dark/Light Theme", Box::new(app::ToggleTheme)),
        ("View: Zoom In", Box::new(app::ZoomIn)),
        ("View: Zoom Out", Box::new(app::ZoomOut)),
        ("View: Reset Zoom", Box::new(app::ZoomReset)),
        ("Terminal: Clear", Box::new(terminal::Clear)),
        ("Terminal: Find", Box::new(terminal::Find)),
        ("Terminal: Copy", Box::new(terminal::Copy)),
        ("Terminal: Paste", Box::new(terminal::Paste)),
        ("Git: Show Graph", Box::new(git::ShowGraph)),
        ("Git: Fetch", Box::new(git::Fetch)),
        ("Git: Pull", Box::new(git::Pull)),
        ("Git: Push", Box::new(git::Push)),
        ("Git: Stash Changes", Box::new(git::Stash)),
        ("Git: Create Branch…", Box::new(git::CreateBranch)),
        ("Git: Checkout Branch…", Box::new(git::CheckoutBranch)),
        ("Git: Merge Branch…", Box::new(git::MergeBranch)),
        ("Agent: New Thread", Box::new(agent::NewThread)),
        ("Agent: New Thread (ACP)", Box::new(agent::NewAcpThread)),
        (
            "Agent: New Thread (Native)",
            Box::new(agent::NewNativeThread),
        ),
        ("Agent: Set Anthropic API Key", Box::new(agent::SetApiKey)),
        ("redcode: Quit", Box::new(app::Quit)),
    ]
}
