//! The left-dock Git panel: working tree changes, the commit box, branches and stashes.

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, relative, uniform_list,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dock::PanelEvent;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, InteractiveElementExt as _, Sizable as _,
    WindowExt as _, h_flex, v_flex,
};

use super::model::{FileState, GitRef, RefKind, Sha, Stash, StatusEntry};
use super::repo::GitRepo;
use crate::picker::{PickerItem, open_picker};
use crate::workspace::Workspace;

const ROW_HEIGHT: f32 = 24.;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Conflicts,
    Staged,
    Changes,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Staged => "Staged",
            Self::Changes => "Changes",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum ChangesRow {
    Header(Section, usize),
    File(Section, StatusEntry),
}

fn is_conflicted(entry: &StatusEntry) -> bool {
    entry.staged == Some(FileState::Conflicted) || entry.unstaged == Some(FileState::Conflicted)
}

/// Flattens the status into list rows: Conflicts (only when there are any), Staged, Changes.
/// A file with both staged and unstaged changes appears in both sections.
pub fn changes_rows(status: &[StatusEntry]) -> Vec<ChangesRow> {
    let section = |section: Section| -> Vec<&StatusEntry> {
        status
            .iter()
            .filter(|entry| match section {
                Section::Conflicts => is_conflicted(entry),
                Section::Staged => !is_conflicted(entry) && entry.staged.is_some(),
                Section::Changes => !is_conflicted(entry) && entry.unstaged.is_some(),
            })
            .collect()
    };
    let mut rows = Vec::new();
    for kind in [Section::Conflicts, Section::Staged, Section::Changes] {
        let entries = section(kind);
        if kind == Section::Conflicts && entries.is_empty() {
            continue;
        }
        rows.push(ChangesRow::Header(kind, entries.len()));
        rows.extend(
            entries
                .into_iter()
                .map(|entry| ChangesRow::File(kind, entry.clone())),
        );
    }
    rows
}

#[derive(Clone, PartialEq, Debug)]
pub enum RefRow {
    /// A collapsible top-level section with its item count.
    Section(&'static str, usize),
    Group(&'static str),
    Ref(GitRef),
    Stash(Stash),
}

/// Rows of the branches/stashes list: Branches (local, remote, tags) then Stashes.
pub fn ref_rows(refs: &[GitRef], stashes: &[Stash], branches_open: bool) -> Vec<RefRow> {
    let mut rows = vec![RefRow::Section("Branches", refs.len())];
    if branches_open {
        for (label, kind) in [
            ("Local", RefKind::LocalBranch),
            ("Remote", RefKind::RemoteBranch),
            ("Tags", RefKind::Tag),
        ] {
            let mut group = refs.iter().filter(|r| r.kind == kind).peekable();
            if group.peek().is_some() {
                rows.push(RefRow::Group(label));
                rows.extend(group.cloned().map(RefRow::Ref));
            }
        }
    }
    rows.push(RefRow::Section("Stashes", stashes.len()));
    rows.extend(stashes.iter().cloned().map(RefRow::Stash));
    rows
}

/// `git diff` arguments shared by the working tree and staged views.
fn diff_args(cached: bool, path: &str) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    if cached {
        args.push("--cached".into());
    }
    args.extend(
        [
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
            path,
        ]
        .map(String::from),
    );
    args
}

/// An untracked file is diffed against `/dev/null`; git exits with 1 because they differ.
fn untracked_diff_args(path: &str) -> Vec<String> {
    [
        "diff",
        "--no-index",
        "--no-color",
        "--no-ext-diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--",
        "/dev/null",
        path,
    ]
    .map(String::from)
    .to_vec()
}

pub struct GitChangesPanel {
    git: Entity<GitRepo>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    message: Entity<TextareaState>,
    amend: bool,
    branches_open: bool,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl GitChangesPanel {
    pub fn new(
        git: Entity<GitRepo>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = cx.new(|cx| TextareaState::new(window, cx).placeholder("Commit message"));
        let subs = vec![
            cx.observe(&git, |_, _, cx| cx.notify()),
            cx.subscribe(&message, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        Self {
            git,
            workspace,
            focus_handle: cx.focus_handle(),
            message,
            amend: false,
            branches_open: true,
            task: None,
            _subs: subs,
        }
    }

    fn open_file(
        &mut self,
        section: Section,
        entry: &StatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = entry.path.to_string_lossy().into_owned();
        if section == Section::Conflicts {
            // Conflict markers are resolved in the editor.
            let file = self.git.read(cx).root.join(&entry.path);
            let workspace = self.workspace.clone();
            window.defer(cx, move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.open_path(file, None, window, cx)
                    })
                    .ok();
            });
            return;
        }
        let (title, args) = match section {
            Section::Staged => (format!("{path} (Staged)"), diff_args(true, &path)),
            _ if entry.unstaged == Some(FileState::Untracked) => {
                (format!("{path} (Working Tree)"), untracked_diff_args(&path))
            }
            _ => (format!("{path} (Working Tree)"), diff_args(false, &path)),
        };
        let output = self.git.update(cx, |git, cx| git.read_raw(args, cx));
        self.task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let output = output.await;
                this.update_in(cx, |this, window, cx| match output {
                    // 1 means "the inputs differ" for `--no-index`.
                    Ok(output) if output.code <= 1 => {
                        let patch = String::from_utf8_lossy(&output.stdout).into_owned();
                        this.workspace
                            .update(cx, |workspace, cx| {
                                workspace.open_diff(title.into(), patch, window, cx)
                            })
                            .ok();
                    }
                    Ok(output) => window.push_notification(
                        Notification::error(output.stderr.trim().to_string()),
                        cx,
                    ),
                    Err(err) => window.push_notification(Notification::error(err.to_string()), cx),
                })
                .ok();
            },
        ));
    }

    fn set_amend(&mut self, amend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.amend = amend;
        cx.notify();
        if !amend || !self.message.read(cx).value().trim().is_empty() {
            return;
        }
        // Amending starts from the previous message.
        let previous = self.git.update(cx, |git, cx| {
            git.read(["log", "-1", "--format=%B"].map(String::from).to_vec(), cx)
        });
        self.task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let Ok(previous) = previous.await else {
                    return;
                };
                let previous = String::from_utf8_lossy(&previous).trim_end().to_string();
                this.update_in(cx, |this, window, cx| {
                    if this.amend && this.message.read(cx).value().trim().is_empty() {
                        this.message
                            .update(cx, |input, cx| input.set_value(previous, window, cx));
                        cx.notify();
                    }
                })
                .ok();
            },
        ));
    }

    /// (staged ≥ 1 and a message) or amending or (merging with every conflict resolved).
    fn can_commit(&self, cx: &App) -> bool {
        let git = self.git.read(cx);
        if git.busy.is_some() {
            return false;
        }
        let conflicts = git
            .status
            .iter()
            .filter(|entry| is_conflicted(entry))
            .count();
        let staged = git
            .status
            .iter()
            .any(|entry| !is_conflicted(entry) && entry.staged.is_some());
        let has_message = !self.message.read(cx).value().trim().is_empty();
        (staged && has_message) || self.amend || (git.merging && conflicts == 0)
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_commit(cx) {
            return;
        }
        let message = self.message.read(cx).value().to_string();
        let amend = self.amend;
        let commit = self
            .git
            .update(cx, |git, cx| git.commit(message, amend, cx));
        self.task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                if commit.await.is_err() {
                    return;
                }
                this.update_in(cx, |this, window, cx| {
                    this.amend = false;
                    this.message
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    cx.notify();
                })
                .ok();
            },
        ));
    }

    fn delete_branch(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        delete_branch(self.git.clone(), name, window, cx);
    }

    fn render_changes_row(
        &self,
        ix: usize,
        row: &ChangesRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        match row {
            ChangesRow::Header(section, count) => {
                let section = *section;
                let action = match section {
                    Section::Staged if *count > 0 => Some(("Unstage All", false)),
                    Section::Changes if *count > 0 => Some(("Stage All", true)),
                    _ => None,
                };
                h_flex()
                    .id(("changes-header", ix))
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .flex_1()
                            .child(format!("{} ({count})", section.label())),
                    )
                    .when_some(action, |this, (label, stage)| {
                        this.child(
                            Button::new(("changes-all", ix))
                                .label(label)
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.git.update(cx, |git, cx| {
                                        if stage {
                                            git.stage_all(cx)
                                        } else {
                                            git.unstage_all(cx)
                                        }
                                    });
                                })),
                        )
                    })
                    .into_any_element()
            }
            ChangesRow::File(section, entry) => {
                let section = *section;
                let state = match section {
                    Section::Conflicts => Some(FileState::Conflicted),
                    Section::Staged => entry.staged,
                    Section::Changes => entry.unstaged,
                };
                let (letter, color) = match state {
                    Some(FileState::Added) => ("A", theme.success),
                    Some(FileState::Deleted) => ("D", theme.danger),
                    Some(FileState::Conflicted) => ("U", theme.danger),
                    Some(FileState::Untracked) => ("?", theme.muted_foreground),
                    Some(state) => (state.letter(), theme.warning),
                    None => ("", theme.muted_foreground),
                };
                let stage = section != Section::Staged;
                let path: Arc<std::path::Path> = entry.path.clone();
                let open_entry = entry.clone();
                h_flex()
                    .id(("changes-file", ix))
                    .w_full()
                    .group("changes-row")
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .gap_2()
                    .text_sm()
                    .hover(|style| style.bg(theme.muted))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file(section, &open_entry, window, cx);
                    }))
                    .child(div().w(px(12.)).flex_none().text_color(color).child(letter))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(entry.path.to_string_lossy().into_owned()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .invisible()
                            .group_hover("changes-row", |style| style.visible())
                            .child(
                                Button::new(("changes-stage", ix))
                                    .icon(Icon::new(if stage {
                                        IconName::Plus
                                    } else {
                                        IconName::Minus
                                    }))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        let paths = vec![path.clone()];
                                        this.git.update(cx, |git, cx| {
                                            if stage {
                                                git.stage(paths, cx)
                                            } else {
                                                git.unstage(paths, cx)
                                            }
                                        });
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_ref_row(
        &self,
        ix: usize,
        row: &RefRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let base = h_flex()
            .id(("ref-row", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .gap_1()
            .text_sm();
        match row {
            RefRow::Section(label, count) => {
                let collapsible = *label == "Branches";
                let open = !collapsible || self.branches_open;
                base.text_xs()
                    .text_color(theme.muted_foreground)
                    .when(collapsible, |this| {
                        this.child(Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        }))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.branches_open = !this.branches_open;
                            cx.notify();
                        }))
                    })
                    .child(format!("{label} ({count})"))
                    .into_any_element()
            }
            RefRow::Group(label) => base
                .pl_4()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(*label)
                .into_any_element(),
            RefRow::Ref(git_ref) => {
                let icon = match git_ref.kind {
                    RefKind::Tag => IconName::Tag,
                    _ => IconName::GitBranch,
                };
                let git = self.git.clone();
                let panel = cx.weak_entity();
                let checkout_ref = git_ref.clone();
                let menu_ref = git_ref.clone();
                base.pl_6()
                    .hover(|style| style.bg(theme.muted))
                    .when(git_ref.is_head, |this| {
                        this.font_weight(gpui::FontWeight::BOLD)
                    })
                    .child(Icon::new(icon).small())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(git_ref.name.clone()),
                    )
                    .when_some(git_ref.upstream.clone(), |this, upstream| {
                        this.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(upstream),
                        )
                    })
                    .on_double_click({
                        let git = git.clone();
                        move |_, _, cx| {
                            git.update(cx, |git, cx| git.checkout(&checkout_ref, cx));
                        }
                    })
                    .context_menu(move |menu, _, _| {
                        let git_ref = menu_ref.clone();
                        let name = git_ref.name.clone();
                        let menu = menu.item(PopupMenuItem::new("Checkout").on_click({
                            let git = git.clone();
                            let git_ref = git_ref.clone();
                            move |_, _, cx| git.update(cx, |git, cx| git.checkout(&git_ref, cx))
                        }));
                        if git_ref.is_head {
                            return menu;
                        }
                        let menu =
                            menu.item(PopupMenuItem::new("Merge into Current Branch").on_click({
                                let git = git.clone();
                                let name = name.clone();
                                move |_, _, cx| {
                                    git.update(cx, |git, cx| git.merge(name.to_string(), cx))
                                }
                            }));
                        if git_ref.kind != RefKind::LocalBranch {
                            return menu;
                        }
                        menu.separator()
                            .item(PopupMenuItem::new("Delete").on_click({
                                let panel = panel.clone();
                                let name = name.clone();
                                move |_, window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.delete_branch(name.clone(), window, cx)
                                        })
                                        .ok();
                                }
                            }))
                    })
                    .into_any_element()
            }
            RefRow::Stash(stash) => {
                let git = self.git.clone();
                let index = stash.index;
                base.pl_6()
                    .hover(|style| style.bg(theme.muted))
                    .child(Icon::new(IconName::Archive).small())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(stash.message.clone()),
                    )
                    .context_menu(move |menu, _, _| {
                        menu.item(PopupMenuItem::new("Apply").on_click({
                            let git = git.clone();
                            move |_, _, cx| git.update(cx, |git, cx| git.stash_apply(index, cx))
                        }))
                        .item(PopupMenuItem::new("Pop").on_click({
                            let git = git.clone();
                            move |_, _, cx| git.update(cx, |git, cx| git.stash_pop(index, cx))
                        }))
                        .separator()
                        .item(PopupMenuItem::new("Drop").on_click({
                            let git = git.clone();
                            move |_, window, cx| drop_stash_dialog(git.clone(), index, window, cx)
                        }))
                    })
                    .into_any_element()
            }
        }
    }

    fn render_not_a_repo(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.git.read(cx).busy.is_some();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .text_color(cx.theme().muted_foreground)
            .child("Git 저장소가 아님")
            .child(
                Button::new("git-init")
                    .label("Initialize Repository")
                    .small()
                    .loading(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git.update(cx, |git, cx| git.init_repository(cx));
                    })),
            )
    }
}

/// Deletes a local branch; when git refuses (e.g. unmerged), offers a forced delete.
pub fn delete_branch(git: Entity<GitRepo>, name: SharedString, window: &mut Window, cx: &mut App) {
    let delete = git.update(cx, |git, cx| git.delete_branch(name.to_string(), false, cx));
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let Err(err) = delete.await else {
                return;
            };
            cx.update(|window, cx| {
                let message = format!("삭제 실패: {err}. 강제 삭제할까요?");
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let git = git.clone();
                    let name = name.clone();
                    alert
                        .title("브랜치 삭제")
                        .description(message.clone())
                        .confirm()
                        .ok_text("강제 삭제")
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, _, cx| {
                            git.update(cx, |git, cx| git.delete_branch(name.to_string(), true, cx))
                                .detach();
                            true
                        })
                });
            })
            .ok();
        })
        .detach();
}

fn drop_stash_dialog(git: Entity<GitRepo>, index: usize, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let git = git.clone();
        alert
            .title("Stash 삭제")
            .description(format!(
                "stash@{{{index}}}를 삭제할까요? 되돌릴 수 없습니다."
            ))
            .confirm()
            .ok_text("Drop")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
                git.update(cx, |git, cx| git.stash_drop(index, cx));
                true
            })
    });
}

/// Asks for a branch name and creates the branch at `start` (HEAD when `None`).
pub fn create_branch_dialog(
    git: Entity<GitRepo>,
    start: Option<Sha>,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("브랜치 이름"));
    let focus_on_mount = Cell::new(true);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        if focus_on_mount.replace(false) {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        let git = git.clone();
        let start = start.clone();
        let name_input = input.clone();
        alert
            .title("브랜치 만들기")
            .child(Input::new(&input))
            .confirm()
            .ok_text("Create")
            .on_ok(move |_, _, cx| {
                let name = name_input.read(cx).value().trim().to_string();
                if name.is_empty() {
                    return false;
                }
                git.update(cx, |git, cx| git.create_branch(name, start.clone(), cx));
                true
            })
    });
}

fn branch_picker(
    git: Entity<GitRepo>,
    include_head: bool,
    on_pick: impl Fn(&mut GitRepo, &GitRef, &mut Context<GitRepo>) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let refs: Vec<GitRef> = git
        .read(cx)
        .refs
        .iter()
        .filter(|r| r.kind != RefKind::Tag && (include_head || !r.is_head))
        .cloned()
        .collect();
    let items = refs
        .iter()
        .map(|r| PickerItem {
            label: r.name.clone(),
            action: None,
        })
        .collect();
    open_picker(
        "브랜치…",
        items,
        move |ix, _, cx| git.update(cx, |git, cx| on_pick(git, &refs[ix], cx)),
        window,
        cx,
    );
}

pub fn checkout_branch_picker(git: Entity<GitRepo>, window: &mut Window, cx: &mut App) {
    branch_picker(
        git,
        true,
        |git, git_ref, cx| git.checkout(git_ref, cx),
        window,
        cx,
    );
}

pub fn merge_branch_picker(git: Entity<GitRepo>, window: &mut Window, cx: &mut App) {
    branch_picker(
        git,
        false,
        |git, git_ref, cx| git.merge(git_ref.name.to_string(), cx),
        window,
        cx,
    );
}

impl gpui_kit::base::dock::Panel for GitChangesPanel {
    fn panel_name(&self) -> &'static str {
        "GitChanges"
    }

    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl gpui_kit::component::dock::Panel for GitChangesPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        "Git"
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for GitChangesPanel {}

impl Focusable for GitChangesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitChangesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.git.read(cx).is_not_repo() {
            return div()
                .track_focus(&self.focus_handle)
                .size_full()
                .child(self.render_not_a_repo(cx))
                .into_any_element();
        }

        let git = self.git.read(cx);
        let changes = changes_rows(&git.status);
        let refs = ref_rows(&git.refs, &git.stashes, self.branches_open);
        let conflicts = git
            .status
            .iter()
            .filter(|entry| is_conflicted(entry))
            .count();
        let merging = git.merging;
        let busy = git.busy.clone();
        let branch: SharedString = match (&git.head.branch, &git.head.oid) {
            (Some(branch), _) => branch.clone(),
            (None, Some(oid)) => format!("HEAD ({})", &oid[..oid.len().min(7)]).into(),
            (None, None) => "HEAD".into(),
        };
        let (ahead, behind) = (git.head.ahead, git.head.behind);
        let can_commit = self.can_commit(cx);
        let theme = cx.theme();

        let header = h_flex()
            .h(px(28.))
            .px_2()
            .gap_2()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(IconName::GitBranch).small())
            .child(div().flex_1().min_w_0().overflow_hidden().child(branch))
            .when(ahead > 0 || behind > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("↑{ahead} ↓{behind}")),
                )
            })
            .when_some(busy, |this, label| {
                this.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
            });

        let banner = merging.then(|| {
            h_flex()
                .px_2()
                .py_1()
                .gap_2()
                .text_sm()
                .bg(theme.muted)
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.warning)
                        .child(format!("병합 중: 충돌 {conflicts}개")),
                )
                .child(
                    Button::new("abort-merge")
                        .label("Abort Merge")
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.git.update(cx, |git, cx| git.abort_merge(cx));
                        })),
                )
        });

        let changes_list = uniform_list(
            "git-changes",
            changes.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                range
                    .map(|ix| {
                        this.render_changes_row(ix, &changes[ix], cx)
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0();

        let commit_box = v_flex()
            .flex_none()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .child(Textarea::new(&self.message).h(px(72.)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("git-amend")
                            .label("Amend")
                            .checked(self.amend)
                            .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                this.set_amend(*checked, window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("git-commit")
                            .label("Commit")
                            .primary()
                            .small()
                            .disabled(!can_commit)
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            );

        let refs_list = uniform_list(
            "git-refs",
            refs.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                range
                    .map(|ix| this.render_ref_row(ix, &refs[ix], cx).into_any_element())
                    .collect::<Vec<_>>()
            }),
        )
        .flex_none()
        .h(relative(0.35))
        .border_t_1()
        .border_color(theme.border);

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(header)
            .children(banner)
            .child(changes_list)
            .child(commit_box)
            .child(refs_list)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn entry(path: &str, staged: Option<FileState>, unstaged: Option<FileState>) -> StatusEntry {
        StatusEntry {
            path: Arc::from(Path::new(path)),
            staged,
            unstaged,
        }
    }

    #[test]
    fn rows_split_status_into_sections() {
        let both = entry(
            "both.rs",
            Some(FileState::Modified),
            Some(FileState::Modified),
        );
        let conflict = entry(
            "c.rs",
            Some(FileState::Conflicted),
            Some(FileState::Conflicted),
        );
        let untracked = entry("new.rs", None, Some(FileState::Untracked));
        let rows = changes_rows(&[both.clone(), conflict.clone(), untracked.clone()]);
        assert_eq!(
            rows,
            vec![
                ChangesRow::Header(Section::Conflicts, 1),
                ChangesRow::File(Section::Conflicts, conflict),
                ChangesRow::Header(Section::Staged, 1),
                ChangesRow::File(Section::Staged, both.clone()),
                ChangesRow::Header(Section::Changes, 2),
                ChangesRow::File(Section::Changes, both),
                ChangesRow::File(Section::Changes, untracked),
            ]
        );
    }

    #[test]
    fn clean_tree_keeps_staged_and_changes_headers() {
        assert_eq!(
            changes_rows(&[]),
            vec![
                ChangesRow::Header(Section::Staged, 0),
                ChangesRow::Header(Section::Changes, 0),
            ]
        );
    }

    #[test]
    fn ref_rows_group_by_kind_and_collapse() {
        let git_ref = |kind, name: &str| GitRef {
            kind,
            name: name.to_string().into(),
            target: Sha::from("abc"),
            upstream: None,
            is_head: false,
        };
        let refs = vec![
            git_ref(RefKind::Tag, "v1"),
            git_ref(RefKind::LocalBranch, "main"),
            git_ref(RefKind::RemoteBranch, "origin/main"),
        ];
        let open = ref_rows(&refs, &[], true);
        assert_eq!(
            open,
            vec![
                RefRow::Section("Branches", 3),
                RefRow::Group("Local"),
                RefRow::Ref(refs[1].clone()),
                RefRow::Group("Remote"),
                RefRow::Ref(refs[2].clone()),
                RefRow::Group("Tags"),
                RefRow::Ref(refs[0].clone()),
                RefRow::Section("Stashes", 0),
            ]
        );
        assert_eq!(
            ref_rows(&refs, &[], false),
            vec![
                RefRow::Section("Branches", 3),
                RefRow::Section("Stashes", 0)
            ]
        );
    }
}
