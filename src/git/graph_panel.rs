//! The commit graph tab: lanes, ref chips, commit details and per-file diffs.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use gpui::{
    App, AsyncWindowContext, BorderStyle, Bounds, ClipboardItem, ContentMask, Context, Entity,
    EventEmitter, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, PathBuilder, Pixels, Point, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window, canvas, div, point, prelude::FluentBuilder as _, px, quad, size,
    uniform_list,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{PanelEvent, TabGroup};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, h_flex, resizable_panel, v_flex, v_resizable,
};

use super::changes_panel::{create_branch_dialog, delete_branch};
use super::graph::{Edge, GraphRow, LANE_COLORS};
use super::model::{self, Commit, FileState, GitRef, RefKind, Sha};
use super::repo::GitRepo;
use crate::actions::git::{SelectNext, SelectPrevious};
use crate::actions::workspace::FocusGitChanges;
use crate::workspace::{TabHandle, Workspace};

const ROW_HEIGHT: f32 = 24.;
const LANE_WIDTH: f32 = 14.;
/// Lanes beyond this are clipped rather than widening the graph column further.
const MAX_VISIBLE_LANES: u16 = 16;
const NODE_RADIUS: f32 = 4.;

/// `YYYY-MM-DD HH:MM` in local time.
fn format_time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// The refs pointing at each commit, in the order git lists them.
fn refs_by_commit(refs: &[GitRef]) -> HashMap<Sha, Vec<GitRef>> {
    let mut map: HashMap<Sha, Vec<GitRef>> = HashMap::new();
    for git_ref in refs {
        map.entry(git_ref.target.clone())
            .or_default()
            .push(git_ref.clone());
    }
    map
}

/// Draws one row of the graph: incoming edges to the row's middle, outgoing edges from it, and
/// the commit node.
fn paint_graph_row(
    row: &GraphRow,
    is_merge: bool,
    bounds: Bounds<Pixels>,
    colors: &[Hsla; LANE_COLORS as usize],
    background: Hsla,
    window: &mut Window,
) {
    let lane_x = |lane: u16| bounds.left() + px(LANE_WIDTH / 2. + LANE_WIDTH * f32::from(lane));
    let top = bounds.top();
    let middle = top + bounds.size.height / 2.;
    let bottom = bounds.bottom();
    let stroke = |edge: &Edge, from_y: Pixels, to_y: Pixels, window: &mut Window| {
        let from = point(lane_x(edge.from), from_y);
        let to = point(lane_x(edge.to), to_y);
        let mut path = PathBuilder::stroke(px(1.5));
        path.move_to(from);
        if edge.from == edge.to {
            path.line_to(to);
        } else {
            // Leave and arrive vertically.
            path.cubic_bezier_to(to, point(from.x, to_y), point(to.x, from_y));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, colors[usize::from(edge.color % LANE_COLORS)]);
        }
    };
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for edge in &row.top {
            stroke(edge, top, middle, window);
        }
        for edge in &row.bottom {
            stroke(edge, middle, bottom, window);
        }
        let color = colors[usize::from(row.color % LANE_COLORS)];
        let center: Point<Pixels> = point(lane_x(row.column), middle);
        let node = Bounds::new(
            point(center.x - px(NODE_RADIUS), center.y - px(NODE_RADIUS)),
            size(px(NODE_RADIUS * 2.), px(NODE_RADIUS * 2.)),
        );
        // A merge commit is a ring; everything else a filled dot.
        let (fill, border) = if is_merge {
            (background, px(1.5))
        } else {
            (color, px(0.))
        };
        window.paint_quad(quad(
            node,
            px(NODE_RADIUS),
            fill,
            border,
            color,
            BorderStyle::default(),
        ));
    });
}

struct Detail {
    sha: Sha,
    message: SharedString,
    files: Vec<(FileState, Arc<Path>)>,
}

pub struct GitGraphPanel {
    git: Entity<GitRepo>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    selected: Option<Sha>,
    refs: HashMap<Sha, Vec<GitRef>>,
    detail: Option<Detail>,
    detail_task: Option<Task<()>>,
    diff_task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    tab: TabHandle,
    _subs: Vec<Subscription>,
}

impl GitGraphPanel {
    pub fn new(
        git: Entity<GitRepo>,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.observe(&git, |this, git, cx| {
            let git = git.read(cx);
            this.refs = refs_by_commit(&git.refs);
            // A rewritten history can drop the selected commit.
            if let Some(selected) = &this.selected
                && !git.commits.iter().any(|commit| &commit.id == selected)
            {
                this.selected = None;
                this.detail = None;
            }
            cx.notify();
        })];
        let refs = refs_by_commit(&git.read(cx).refs);
        Self {
            git,
            workspace,
            focus_handle: cx.focus_handle(),
            selected: None,
            refs,
            detail: None,
            detail_task: None,
            diff_task: None,
            scroll: UniformListScrollHandle::new(),
            tab: TabHandle::default(),
            _subs: subs,
        }
    }

    fn selected_index(&self, cx: &App) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.git
            .read(cx)
            .commits
            .iter()
            .position(|commit| &commit.id == selected)
    }

    /// Selects the commit at `ix` and loads its message and changed files.
    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sha) = self
            .git
            .read(cx)
            .commits
            .get(ix)
            .map(|commit| commit.id.clone())
        else {
            return;
        };
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        if self.selected.as_ref() == Some(&sha) {
            cx.notify();
            return;
        }
        self.selected = Some(sha.clone());
        self.detail = None;
        cx.notify();

        let (message, files) = self.git.update(cx, |git, cx| {
            let message = ["show", "-s", "--format=%B", &sha]
                .map(String::from)
                .to_vec();
            let files = [
                "show",
                "--diff-merges=first-parent",
                "--format=",
                "--name-status",
                "-z",
                &sha,
            ]
            .map(String::from)
            .to_vec();
            (git.read(message, cx), git.read(files, cx))
        });
        self.detail_task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let message = message.await.unwrap_or_default();
                let files = files.await.unwrap_or_default();
                this.update(cx, |this, cx| {
                    if this.selected.as_ref() != Some(&sha) {
                        return;
                    }
                    this.detail = Some(Detail {
                        sha,
                        message: String::from_utf8_lossy(&message)
                            .trim_end()
                            .to_string()
                            .into(),
                        files: model::parse_name_status(&files),
                    });
                    cx.notify();
                })
                .ok();
            },
        ));
    }

    fn select_sha(&mut self, sha: &Sha, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self
            .git
            .read(cx)
            .commits
            .iter()
            .position(|commit| &commit.id == sha);
        if let Some(ix) = ix {
            self.select(ix, window, cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index(cx).map_or(0, |ix| ix.saturating_sub(1));
        self.select(ix, window, cx);
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index(cx).map_or(0, |ix| ix + 1);
        self.select(ix, window, cx);
    }

    fn open_file_diff(
        &mut self,
        sha: Sha,
        path: Arc<Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.to_string_lossy().into_owned();
        let title = format!("{path} @ {}", short(&sha));
        let args = [
            "show",
            "--diff-merges=first-parent",
            "--format=",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--patch",
            &sha,
            "--",
            &path,
        ]
        .map(String::from)
        .to_vec();
        let patch = self.git.update(cx, |git, cx| git.read(args, cx));
        self.diff_task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let Ok(patch) = patch.await else {
                    return;
                };
                let patch = String::from_utf8_lossy(&patch).into_owned();
                this.update_in(cx, |this, window, cx| {
                    this.workspace
                        .update(cx, |workspace, cx| {
                            workspace.open_diff(title.into(), patch, window, cx)
                        })
                        .ok();
                })
                .ok();
            },
        ));
    }

    fn lane_colors(cx: &App) -> [Hsla; LANE_COLORS as usize] {
        let theme = cx.theme();
        [
            theme.blue,
            theme.green,
            theme.red,
            theme.yellow,
            theme.magenta,
            theme.cyan,
            theme.blue_light,
            theme.green_light,
        ]
    }

    fn render_chip(git_ref: &GitRef, cx: &App) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (background, foreground) = match git_ref.kind {
            RefKind::LocalBranch => (theme.accent, theme.accent_foreground),
            RefKind::RemoteBranch => (theme.muted, theme.muted_foreground),
            RefKind::Tag => (theme.warning.opacity(0.2), theme.warning),
        };
        let label: SharedString = if git_ref.is_head {
            format!("● {}", git_ref.name).into()
        } else {
            git_ref.name.clone()
        };
        div()
            .flex_none()
            .px_1()
            .rounded(px(4.))
            .border_1()
            .border_color(theme.border)
            .bg(background)
            .text_color(foreground)
            .text_xs()
            .when(git_ref.is_head, |this| {
                this.font_weight(gpui::FontWeight::BOLD)
            })
            .child(label)
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let git = self.git.read(cx);
        let (Some(commit), Some(row)) = (git.commits.get(ix), git.rows.get(ix)) else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let commit: Commit = commit.clone();
        let row: GraphRow = row.clone();
        let graph_width =
            px(f32::from(git.graph_width.min(MAX_VISIBLE_LANES)) * LANE_WIDTH + LANE_WIDTH);
        let refs = self.refs.get(&commit.id).cloned().unwrap_or_default();
        let selected = self.selected.as_ref() == Some(&commit.id);
        let is_merge = commit.parents.len() > 1;
        let colors = Self::lane_colors(cx);
        let theme = cx.theme();
        let background = theme.background;

        let graph = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                paint_graph_row(&row, is_merge, bounds, &colors, background, window);
            },
        )
        .flex_none()
        .w(graph_width)
        .h_full();

        let chips: Vec<_> = refs
            .iter()
            .map(|git_ref| Self::render_chip(git_ref, cx).into_any_element())
            .collect();

        let git_entity = self.git.clone();
        let sha = commit.id.clone();
        h_flex()
            .id(("commit-row", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .pr_2()
            .gap_2()
            .text_sm()
            .when(selected, |this| this.bg(theme.selection))
            .when(!selected, |this| this.hover(|style| style.bg(theme.muted)))
            .on_click(cx.listener(move |this, _, window, cx| this.select(ix, window, cx)))
            .child(graph)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .children(chips)
                    .child(div().whitespace_nowrap().child(commit.subject.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(140.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(theme.muted_foreground)
                    .child(commit.author.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(124.))
                    .whitespace_nowrap()
                    .text_color(theme.muted_foreground)
                    .child(format_time(commit.time)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(64.))
                    .whitespace_nowrap()
                    .text_color(theme.muted_foreground)
                    .child(short(&commit.id).to_string()),
            )
            .context_menu(move |menu, _, _| {
                let mut menu = menu;
                for git_ref in refs.iter().filter(|r| r.kind != RefKind::Tag && !r.is_head) {
                    menu = menu.item(
                        PopupMenuItem::new(format!("Checkout {}", git_ref.name)).on_click({
                            let git = git_entity.clone();
                            let git_ref = git_ref.clone();
                            move |_, _, cx| git.update(cx, |git, cx| git.checkout(&git_ref, cx))
                        }),
                    );
                }
                menu = menu
                    .item(PopupMenuItem::new("Checkout (detached)").on_click({
                        let git = git_entity.clone();
                        let sha = sha.clone();
                        move |_, _, cx| {
                            git.update(cx, |git, cx| git.checkout_detached(sha.clone(), cx))
                        }
                    }))
                    .item(PopupMenuItem::new("Create Branch Here…").on_click({
                        let git = git_entity.clone();
                        let sha = sha.clone();
                        move |_, window, cx| {
                            create_branch_dialog(git.clone(), Some(sha.clone()), window, cx)
                        }
                    }));
                for git_ref in refs.iter().filter(|r| r.kind != RefKind::Tag && !r.is_head) {
                    menu = menu.item(
                        PopupMenuItem::new(format!("Merge {} into Current Branch", git_ref.name))
                            .on_click({
                                let git = git_entity.clone();
                                let name = git_ref.name.to_string();
                                move |_, _, cx| {
                                    git.update(cx, |git, cx| git.merge(name.clone(), cx))
                                }
                            }),
                    );
                }
                for git_ref in refs
                    .iter()
                    .filter(|r| r.kind == RefKind::LocalBranch && !r.is_head)
                {
                    menu = menu.item(
                        PopupMenuItem::new(format!("Delete Branch {}", git_ref.name)).on_click({
                            let git = git_entity.clone();
                            let name = git_ref.name.clone();
                            move |_, window, cx| {
                                delete_branch(git.clone(), name.clone(), window, cx)
                            }
                        }),
                    );
                }
                menu.separator()
                    .item(PopupMenuItem::new("Copy SHA").on_click({
                        let sha = sha.clone();
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()))
                        }
                    }))
            })
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let git = self.git.read(cx);
        let (ahead, behind) = (git.head.ahead, git.head.behind);
        let busy = git.busy.clone();
        let theme = cx.theme();
        let counted = |label: &str, count: u32| {
            if count > 0 {
                format!("{label} ({count})")
            } else {
                label.to_string()
            }
        };
        h_flex()
            .flex_none()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("graph-fetch")
                    .label("Fetch")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git.update(cx, |git, cx| git.fetch(cx));
                    })),
            )
            .child(
                Button::new("graph-pull")
                    .label(counted("Pull", behind))
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git.update(cx, |git, cx| git.pull(cx));
                    })),
            )
            .child(
                Button::new("graph-push")
                    .label(counted("Push", ahead))
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git.update(cx, |git, cx| git.push(cx));
                    })),
            )
            .child(
                Button::new("graph-branch")
                    .label("Branch…")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| {
                        create_branch_dialog(this.git.clone(), None, window, cx);
                    })),
            )
            .child(
                Button::new("graph-stash")
                    .label("Stash")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git
                            .update(cx, |git, cx| git.stash_push(None, true, cx));
                    })),
            )
            .child(div().flex_1())
            .when_some(busy, |this, label| {
                this.child(Spinner::new().small()).child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
            })
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let Some(selected) = &self.selected else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child("커밋을 선택하세요")
                .into_any_element();
        };
        let git = self.git.read(cx);
        let Some(commit) = git.commits.iter().find(|commit| &commit.id == selected) else {
            return div().into_any_element();
        };
        let detail = self
            .detail
            .as_ref()
            .filter(|detail| &detail.sha == selected);
        let message =
            detail.map_or_else(|| commit.subject.clone(), |detail| detail.message.clone());

        let parents = commit.parents.iter().enumerate().map(|(ix, parent)| {
            let parent = parent.clone();
            div()
                .id(("parent", ix))
                .text_color(theme.blue)
                .cursor_pointer()
                .child(short(&parent).to_string())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_sha(&parent, window, cx);
                }))
        });

        let files = detail.into_iter().flat_map(|detail| {
            detail.files.iter().enumerate().map(|(ix, (state, path))| {
                let sha = detail.sha.clone();
                let path = path.clone();
                h_flex()
                    .id(("detail-file", ix))
                    .h(px(ROW_HEIGHT))
                    .gap_2()
                    .px_1()
                    .hover(|style| style.bg(theme.muted))
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .text_color(muted)
                            .child(state.letter()),
                    )
                    .child(path.to_string_lossy().into_owned())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file_diff(sha.clone(), path.clone(), window, cx);
                    }))
            })
        });

        v_flex()
            .id("commit-detail")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .text_sm()
            .child(v_flex().children(message.lines().map(|line| {
                div().child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                })
            })))
            .child(
                v_flex()
                    .text_color(muted)
                    .child(format!("{} <{}>", commit.author, commit.email))
                    .child(format_time(commit.time))
                    .child(commit.id.to_string())
                    .child(h_flex().gap_2().child("Parents:").children(parents)),
            )
            .child(v_flex().children(files))
            .into_any_element()
    }
}

impl gpui_kit::base::dock::Panel for GitGraphPanel {
    fn panel_name(&self) -> &'static str {
        "GitGraph"
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, _: &mut Context<Self>) {
        self.tab.set(group);
    }
}

impl gpui_kit::component::dock::Panel for GitGraphPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        "Git Graph"
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for GitGraphPanel {}

impl Focusable for GitGraphPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitGraphPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let git = self.git.read(cx);
        let count = git.commits.len();
        let uncommitted = git.status.len();
        let theme = cx.theme();

        let uncommitted_bar = (uncommitted > 0).then(|| {
            div()
                .id("uncommitted")
                .flex_none()
                .h(px(ROW_HEIGHT))
                .px_3()
                .flex()
                .items_center()
                .text_sm()
                .text_color(theme.warning)
                .border_b_1()
                .border_color(theme.border)
                .cursor_pointer()
                .child(format!("● Uncommitted changes ({uncommitted})"))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(FocusGitChanges), cx);
                })
        });

        let list = uniform_list(
            "git-graph",
            count,
            cx.processor(|this, range: Range<usize>, _, cx| {
                range
                    .map(|ix| this.render_row(ix, cx).into_any_element())
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();

        v_flex()
            .key_context("GitGraph")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .child(self.render_toolbar(cx))
            .children(uncommitted_bar)
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("git-graph-split")
                        .child(resizable_panel().child(list))
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .child(self.render_detail(cx)),
                        ),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_grouped_by_target_commit() {
        let git_ref = |name: &str, target: &str| GitRef {
            kind: RefKind::LocalBranch,
            name: name.to_string().into(),
            target: Sha::from(target),
            upstream: None,
            is_head: false,
        };
        let map = refs_by_commit(&[
            git_ref("main", "a"),
            git_ref("dev", "b"),
            git_ref("topic", "a"),
        ]);
        let names = |sha: &str| -> Vec<String> {
            map[&Sha::from(sha)]
                .iter()
                .map(|r| r.name.to_string())
                .collect()
        };
        assert_eq!(names("a"), vec!["main", "topic"]);
        assert_eq!(names("b"), vec!["dev"]);
    }
}
