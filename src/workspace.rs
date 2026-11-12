use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, Styled as _, Subscription, WeakEntity, Window, div, px,
};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dock::{
    DockArea, DockLayout, DockPlacement, DockSkin, InsertTarget, PaneRef, Panel, PanelId,
    PanelStyle, TabGroup, panel_handle,
};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Placement, TitleBar, WindowExt as _, v_flex};

use crate::actions::workspace::{
    CloseActiveTab, FocusFiles, FocusGitChanges, GoToFile, NewTerminal, NextTab, OpenFile,
    PreviousTab, SplitDown, SplitRight, ToggleCommandPalette, ToggleSidebar,
};
use crate::actions::{agent as agent_actions, app as app_actions, git as git_actions};
use crate::agent::panel::{AgentPanel, set_api_key_dialog};
use crate::diff_panel::DiffPanel;
use crate::files::editor_panel::EditorPanel;
use crate::files::tree_panel::FileTreePanel;
use crate::fs_watch::FsWatcher;
use crate::git::changes_panel::{
    GitChangesPanel, checkout_branch_picker, create_branch_dialog, merge_branch_picker,
};
use crate::git::graph_panel::GitGraphPanel;
use crate::git::repo::{GitEvent, GitRepo};
use crate::picker::{PickerItem, open_picker};
use crate::settings::{self, Backend};
use crate::terminal::Terminal;
use crate::terminal::panel::TerminalPanel;

pub enum AddTarget {
    ActiveGroup,
    SplitRight,
    SplitDown,
}

/// The tab group a center panel currently lives in. The tab bar is rendered by the group, so a
/// panel whose title changed has to notify it.
#[derive(Default)]
pub struct TabHandle(Option<WeakEntity<TabGroup>>);

impl TabHandle {
    pub fn set(&mut self, group: WeakEntity<TabGroup>) {
        self.0 = Some(group);
    }

    pub fn refresh(&self, cx: &mut App) {
        if let Some(group) = &self.0 {
            group.update(cx, |_, cx| cx.notify()).ok();
        }
    }
}

pub struct Workspace {
    pub root: PathBuf,
    dock_area: Entity<DockArea>,
    _skin: Rc<DockSkin>,
    fs: Entity<FsWatcher>,
    files: Entity<FileTreePanel>,
    git: Entity<GitRepo>,
    changes: Entity<GitChangesPanel>,
    /// Keeps the workspace on the action dispatch path when no panel has focus.
    focus_handle: FocusHandle,
    active_center: Option<PanelId>,
    focus_subs: HashMap<PanelId, Subscription>,
    _subs: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        root: PathBuf,
        open_file: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (dock_area, skin) = DockSkin::dock_area("workspace", None, window, cx);
        skin.set_close_button_visible(true, cx);
        skin.set_panel_style(PanelStyle::TabBar, cx);

        let workspace = cx.weak_entity();
        let fs = cx.new(|cx| FsWatcher::new(root.clone(), cx));
        let files = cx
            .new(|cx| FileTreePanel::new(root.clone(), fs.clone(), workspace.clone(), window, cx));
        let git = cx.new(|cx| GitRepo::new(root.clone(), fs.clone(), cx));
        let changes = cx.new(|cx| GitChangesPanel::new(git.clone(), workspace, window, cx));
        dock_area.update(cx, |dock, cx| {
            dock.set_dock(
                DockPlacement::Left,
                DockLayout::tabs()
                    .panel_view(panel_handle(files.clone()), cx)
                    .panel_view(panel_handle(changes.clone()), cx)
                    .active_index(0),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Left, px(280.), window, cx);
        });
        // The one place git failures surface; panels only observe the repository.
        let subs = vec![
            cx.subscribe_in(&git, window, |_, _, event: &GitEvent, window, cx| {
                if let GitEvent::OperationFailed(message) = event {
                    window.push_notification(Notification::error(message.clone()), cx);
                }
            }),
        ];

        let mut this = Self {
            root,
            dock_area,
            _skin: skin,
            fs,
            files,
            git,
            changes,
            focus_handle: cx.focus_handle(),
            active_center: None,
            focus_subs: HashMap::new(),
            _subs: subs,
        };

        match this.create_terminal(this.root.clone(), window, cx) {
            Ok(panel) => {
                let id = PanelId::from(panel.entity_id());
                this.track_focus(&panel, window, cx);
                this.dock_area.update(cx, |dock, cx| {
                    dock.set_center(
                        DockLayout::tabs().panel_view(panel_handle(panel.clone()), cx),
                        window,
                        cx,
                    );
                });
                panel.focus_handle(cx).focus(window, cx);
                this.active_center = Some(id);
            }
            Err(err) => notify_terminal_error(&err, window, cx),
        }
        if let Some(file) = open_file {
            this.open_path(file, None, window, cx);
        }
        this
    }

    pub fn fs(&self) -> &Entity<FsWatcher> {
        &self.fs
    }

    fn root_name(&self) -> SharedString {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.to_string_lossy().into_owned())
            .into()
    }

    fn create_terminal(
        &self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Entity<TerminalPanel>> {
        let terminal = Terminal::spawn(cwd, cx)?;
        let workspace = cx.weak_entity();
        Ok(cx.new(|cx| TerminalPanel::new(terminal, workspace, window, cx)))
    }

    fn track_focus<P: Panel>(
        &mut self,
        panel: &Entity<P>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = PanelId::from(panel.entity_id());
        let subscription =
            cx.on_focus_in(&panel.focus_handle(cx), window, move |workspace, _, _| {
                workspace.active_center = Some(id);
            });
        self.focus_subs.insert(id, subscription);
    }

    fn center_panels(&self, cx: &App) -> Vec<PanelId> {
        self.dock_area
            .read(cx)
            .layout(DockPlacement::Center)
            .map(|tree| tree.panels().collect())
            .unwrap_or_default()
    }

    pub fn add_center_panel<P: Panel>(
        &mut self,
        panel: Entity<P>,
        target: AddTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = PanelId::from(panel.entity_id());
        self.track_focus(&panel, window, cx);
        let active = self.active_center;
        self.dock_area.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            );
            let node = active.and_then(|active| {
                dock.layout(DockPlacement::Center)
                    .and_then(|tree| tree.find_panel_node(active))
            });
            if let Some(node) = node {
                let target = match target {
                    AddTarget::ActiveGroup => InsertTarget::Tabs {
                        node,
                        ix: None,
                        activate: true,
                    },
                    AddTarget::SplitRight => InsertTarget::Split {
                        node,
                        placement: Placement::Right,
                        size: None,
                    },
                    AddTarget::SplitDown => InsertTarget::Split {
                        node,
                        placement: Placement::Bottom,
                        size: None,
                    },
                };
                dock.move_panel(id, target, window, cx);
            }
        });
        panel.focus_handle(cx).focus(window, cx);
        self.active_center = Some(id);
    }

    /// Removes a center panel of any type from the dock.
    fn remove_dock_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.dock_area.read(cx).panel(id).map(|panel| panel.view()) else {
            return;
        };
        macro_rules! remove_as {
            ($($panel:ty),+) => {$(
                if let Ok(panel) = view.clone().downcast::<$panel>() {
                    self.dock_area
                        .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    return;
                }
            )+};
        }
        remove_as!(
            TerminalPanel,
            EditorPanel,
            DiffPanel,
            GitGraphPanel,
            AgentPanel
        );
    }

    pub fn close_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        let panels = self.center_panels(cx);
        if !panels.contains(&id) {
            return;
        }
        if panels.len() == 1 {
            // The last center tab stays, except that an exited terminal is replaced by a new shell.
            let exited = self
                .panel_as::<TerminalPanel>(id, cx)
                .is_some_and(|panel| panel.read(cx).exited(cx));
            if !exited {
                return;
            }
            self.new_terminal(AddTarget::ActiveGroup, window, cx);
            if self.center_panels(cx).len() == 1 {
                return;
            }
            self.remove_dock_panel(id, window, cx);
            self.focus_subs.remove(&id);
            return;
        }
        self.remove_dock_panel(id, window, cx);
        self.focus_subs.remove(&id);
        if self.active_center == Some(id) {
            self.active_center = self.center_panels(cx).first().copied();
            if let Some(next) = self.active_center {
                self.reveal_panel(next, window, cx);
            }
        }
    }

    /// Shows the panel's tab and focuses it.
    pub fn reveal_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
        let focus = self
            .dock_area
            .read(cx)
            .panel(id)
            .map(|panel| panel.focus_handle(cx));
        if let Some(focus) = focus {
            focus.focus(window, cx);
        }
        if self.center_panels(cx).contains(&id) {
            self.active_center = Some(id);
        }
    }

    fn panel_as<P: Panel>(&self, id: PanelId, cx: &App) -> Option<Entity<P>> {
        self.dock_area
            .read(cx)
            .panel(id)
            .and_then(|panel| panel.view().downcast::<P>().ok())
    }

    pub fn active_panel<P: Panel>(&self, cx: &App) -> Option<Entity<P>> {
        self.panel_as(self.active_center?, cx)
    }

    pub fn find_panel<P: Panel>(&self, cx: &App, pred: impl Fn(&P) -> bool) -> Option<Entity<P>> {
        self.center_panels(cx)
            .into_iter()
            .filter_map(|id| self.panel_as::<P>(id, cx))
            .find(|panel| pred(panel.read(cx)))
    }

    pub fn new_terminal(&mut self, target: AddTarget, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self
            .active_panel::<TerminalPanel>(cx)
            .and_then(|panel| panel.read(cx).working_directory(cx))
            .unwrap_or_else(|| self.root.clone());
        self.new_terminal_in(cwd, target, window, cx);
    }

    /// Opens a terminal tab in `cwd` in the active group.
    pub fn new_terminal_at(&mut self, cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal_in(cwd, AddTarget::ActiveGroup, window, cx);
    }

    fn new_terminal_in(
        &mut self,
        cwd: PathBuf,
        target: AddTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.create_terminal(cwd, window, cx) {
            Ok(panel) => self.add_center_panel(panel, target, window, cx),
            Err(err) => notify_terminal_error(&err, window, cx),
        }
    }

    fn on_toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev = window.focused(cx);
        let commands = crate::actions::palette_commands();
        let items = commands
            .iter()
            .map(|(label, action)| PickerItem {
                label: (*label).into(),
                action: Some(action.boxed_clone()),
            })
            .collect();
        open_picker(
            "명령 입력…",
            items,
            move |ix, window, cx| {
                let action = commands[ix].1.boxed_clone();
                match &prev {
                    Some(prev) => prev.dispatch_action(&*action, window, cx),
                    None => window.dispatch_action(action, cx),
                }
            },
            window,
            cx,
        );
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(AddTarget::ActiveGroup, window, cx);
    }

    fn on_split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(AddTarget::SplitRight, window, cx);
    }

    fn on_split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(AddTarget::SplitDown, window, cx);
    }

    /// Reveals the tab `offset` positions away in the active panel's group, wrapping around.
    fn cycle_tab(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_center else {
            return;
        };
        let next = {
            let dock = self.dock_area.read(cx);
            let Some(tree) = dock.layout(DockPlacement::Center) else {
                return;
            };
            let node = tree
                .find_panel_node(active)
                .and_then(|node| tree.find_node(node));
            match node.map(|node| node.kind()) {
                Some(PaneRef::Tabs { panels, active_ix }) if !panels.is_empty() => {
                    let len = panels.len() as isize;
                    panels[(active_ix as isize + offset).rem_euclid(len) as usize]
                }
                _ => return,
            }
        };
        self.reveal_panel(next, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }

    fn on_previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, window, cx);
    }

    fn on_close_active_tab(
        &mut self,
        _: &CloseActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_center else {
            return;
        };
        let dirty_editor = self
            .panel_as::<EditorPanel>(id, cx)
            .filter(|editor| editor.read(cx).is_dirty());
        // The last center tab never closes, so there is nothing to confirm for it.
        let Some(editor) = dirty_editor.filter(|_| self.center_panels(cx).len() > 1) else {
            self.close_panel(id, window, cx);
            return;
        };
        let name = editor.read(cx).file_name();
        let workspace = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            alert
                .title("저장하지 않은 변경 사항")
                .description(format!("{name}의 변경 사항을 버리고 닫을까요?"))
                .confirm()
                .ok_text("저장 안 함")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| workspace.close_panel(id, window, cx))
                        .ok();
                    true
                })
        });
    }

    // --- Sidebar and files -------------------------------------------------------------------

    fn on_toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dock_area.update(cx, |dock, cx| {
            dock.toggle_dock(DockPlacement::Left, window, cx);
        });
    }

    /// Opens the left dock if needed, then shows and focuses one of its panels.
    fn focus_sidebar_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        self.dock_area.update(cx, |dock, cx| {
            if !dock.is_dock_open(DockPlacement::Left) {
                dock.toggle_dock(DockPlacement::Left, window, cx);
            }
        });
        self.reveal_panel(id, window, cx);
    }

    fn on_focus_files(&mut self, _: &FocusFiles, window: &mut Window, cx: &mut Context<Self>) {
        let id = PanelId::from(self.files.entity_id());
        self.focus_sidebar_panel(id, window, cx);
    }

    fn on_focus_git_changes(
        &mut self,
        _: &FocusGitChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = PanelId::from(self.changes.entity_id());
        self.focus_sidebar_panel(id, window, cx);
    }

    fn on_go_to_file(&mut self, _: &GoToFile, window: &mut Window, cx: &mut Context<Self>) {
        let files = self.files.read(cx).files();
        let items = files
            .iter()
            .map(|path| PickerItem {
                label: path.to_string_lossy().into_owned().into(),
                action: None,
            })
            .collect();
        let root = self.root.clone();
        let workspace = cx.weak_entity();
        open_picker(
            "파일 이름…",
            items,
            move |ix, window, cx| {
                let path = root.join(&files[ix]);
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.open_path(path, None, window, cx)
                    })
                    .ok();
            },
            window,
            cx,
        );
    }

    fn on_open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let Ok(Ok(Some(paths))) = paths.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                this.update_in(cx, |workspace, window, cx| {
                    workspace.open_path(path, None, window, cx);
                })
                .ok();
            },
        )
        .detach();
    }

    fn on_open_settings(
        &mut self,
        _: &app_actions::OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path(settings::path(), None, window, cx);
    }

    // --- Git ---------------------------------------------------------------------------------

    fn on_git_show_graph(
        &mut self,
        _: &git_actions::ShowGraph,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.find_panel::<GitGraphPanel>(cx, |_| true) {
            self.reveal_panel(PanelId::from(panel.entity_id()), window, cx);
            return;
        }
        let git = self.git.clone();
        let workspace = cx.weak_entity();
        let panel = cx.new(|cx| GitGraphPanel::new(git, workspace, window, cx));
        self.add_center_panel(panel, AddTarget::ActiveGroup, window, cx);
    }

    fn on_git_fetch(&mut self, _: &git_actions::Fetch, _: &mut Window, cx: &mut Context<Self>) {
        self.git.update(cx, |git, cx| git.fetch(cx));
    }

    fn on_git_pull(&mut self, _: &git_actions::Pull, _: &mut Window, cx: &mut Context<Self>) {
        self.git.update(cx, |git, cx| git.pull(cx));
    }

    fn on_git_push(&mut self, _: &git_actions::Push, _: &mut Window, cx: &mut Context<Self>) {
        self.git.update(cx, |git, cx| git.push(cx));
    }

    fn on_git_stash(&mut self, _: &git_actions::Stash, _: &mut Window, cx: &mut Context<Self>) {
        self.git
            .update(cx, |git, cx| git.stash_push(None, true, cx));
    }

    fn on_git_create_branch(
        &mut self,
        _: &git_actions::CreateBranch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        create_branch_dialog(self.git.clone(), None, window, cx);
    }

    fn on_git_checkout_branch(
        &mut self,
        _: &git_actions::CheckoutBranch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        checkout_branch_picker(self.git.clone(), window, cx);
    }

    fn on_git_merge_branch(
        &mut self,
        _: &git_actions::MergeBranch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        merge_branch_picker(self.git.clone(), window, cx);
    }

    // --- Agent -------------------------------------------------------------------------------

    fn new_agent_thread(&mut self, backend: Backend, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let workspace = cx.weak_entity();
        let panel = cx.new(|cx| AgentPanel::new(backend, root, workspace, window, cx));
        self.add_center_panel(panel, AddTarget::ActiveGroup, window, cx);
    }

    fn on_agent_new_thread(
        &mut self,
        _: &agent_actions::NewThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let backend = settings::get(cx).agent.backend;
        self.new_agent_thread(backend, window, cx);
    }

    fn on_agent_new_acp_thread(
        &mut self,
        _: &agent_actions::NewAcpThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_thread(Backend::Acp, window, cx);
    }

    fn on_agent_new_native_thread(
        &mut self,
        _: &agent_actions::NewNativeThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_thread(Backend::Native, window, cx);
    }

    fn on_agent_set_api_key(
        &mut self,
        _: &agent_actions::SetApiKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        set_api_key_dialog(window, cx);
    }
}

fn notify_terminal_error(err: &anyhow::Error, window: &mut Window, cx: &mut App) {
    window.push_notification(
        Notification::error(format!("터미널을 시작할 수 없습니다: {err}")),
        cx,
    );
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            self.focus_handle.focus(window, cx);
        }
        v_flex()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .on_action(cx.listener(Self::on_toggle_command_palette))
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_split_down))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(Self::on_close_active_tab))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_focus_files))
            .on_action(cx.listener(Self::on_focus_git_changes))
            .on_action(cx.listener(Self::on_go_to_file))
            .on_action(cx.listener(Self::on_open_file))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_git_show_graph))
            .on_action(cx.listener(Self::on_git_fetch))
            .on_action(cx.listener(Self::on_git_pull))
            .on_action(cx.listener(Self::on_git_push))
            .on_action(cx.listener(Self::on_git_stash))
            .on_action(cx.listener(Self::on_git_create_branch))
            .on_action(cx.listener(Self::on_git_checkout_branch))
            .on_action(cx.listener(Self::on_git_merge_branch))
            .on_action(cx.listener(Self::on_agent_new_thread))
            .on_action(cx.listener(Self::on_agent_new_acp_thread))
            .on_action(cx.listener(Self::on_agent_new_native_thread))
            .on_action(cx.listener(Self::on_agent_set_api_key))
            .child(TitleBar::new().child(self.root_name()))
            .child(div().flex_1().min_h_0().child(self.dock_area.clone()))
    }
}
