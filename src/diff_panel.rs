use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, div,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};
use gpui_kit::component::dock::{PanelEvent, PanelId, TabGroup};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use similar::TextDiff;

use crate::settings;
use crate::workspace::{AddTarget, TabHandle, Workspace};

/// A unified patch (3 lines of context) between two versions of one file, in the
/// `--- a/path` / `+++ b/path` form the diff viewer parses.
pub fn unified_patch(path: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

enum Content {
    /// The patch was empty.
    Empty,
    Parsed,
    /// The viewer could not parse the patch; it is shown verbatim.
    Raw(SharedString),
}

pub struct DiffPanel {
    title: SharedString,
    state: Entity<DiffState>,
    content: Content,
    focus_handle: FocusHandle,
    tab: TabHandle,
}

impl DiffPanel {
    fn new(title: SharedString, patch: &str, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| DiffState::new(Vec::new(), cx).with_mode(DiffMode::Unified));
        let mut this = Self {
            title,
            state,
            content: Content::Empty,
            focus_handle: cx.focus_handle(),
            tab: TabHandle::default(),
        };
        this.set_patch(patch, cx);
        this
    }

    fn set_patch(&mut self, patch: &str, cx: &mut Context<Self>) {
        self.content = if patch.trim().is_empty() {
            Content::Empty
        } else {
            match DiffFile::parse(patch) {
                Ok(files) => {
                    self.state
                        .update(cx, |state, cx| state.set_files(files, cx));
                    Content::Parsed
                }
                Err(_) => Content::Raw(patch.to_string().into()),
            }
        };
        cx.notify();
    }

    fn mode_button(&self, label: &'static str, mode: DiffMode, cx: &mut Context<Self>) -> Button {
        let button = Button::new(label).label(label).xsmall();
        let button = if self.state.read(cx).mode() == mode {
            button.primary()
        } else {
            button.ghost()
        };
        button.on_click(cx.listener(move |this, _, _, cx| {
            this.state.update(cx, |state, cx| state.set_mode(mode, cx));
            cx.notify();
        }))
    }
}

impl Workspace {
    /// Shows `patch` in the diff tab titled `title`, replacing that tab's content if it exists.
    pub fn open_diff(
        &mut self,
        title: SharedString,
        patch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.find_panel::<DiffPanel>(cx, |panel| panel.title == title) {
            panel.update(cx, |panel, cx| panel.set_patch(&patch, cx));
            self.reveal_panel(PanelId::from(panel.entity_id()), window, cx);
            return;
        }
        let panel = cx.new(|cx| DiffPanel::new(title, &patch, cx));
        self.add_center_panel(panel, AddTarget::ActiveGroup, window, cx);
    }
}

impl gpui_kit::base::dock::Panel for DiffPanel {
    fn panel_name(&self) -> &'static str {
        "Diff"
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, _: &mut Context<Self>) {
        self.tab.set(group);
    }
}

impl gpui_kit::component::dock::Panel for DiffPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.title.clone()
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for DiffPanel {}

impl Focusable for DiffPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body = match &self.content {
            Content::Empty => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("변경 사항 없음")
                .into_any_element(),
            Content::Parsed => div()
                .flex_1()
                .min_h_0()
                .child(Diff::new(&self.state))
                .into_any_element(),
            Content::Raw(patch) => {
                let appearance = &settings::get(cx).appearance;
                div()
                    .id("diff-raw")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .font_family(SharedString::from(appearance.font_family.clone()))
                    .text_size(gpui::px(appearance.font_size))
                    .children(patch.lines().map(|line| {
                        // An empty div has no height; keep blank lines visible.
                        div().whitespace_nowrap().child(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line.to_string()
                        })
                    }))
                    .into_any_element()
            }
        };
        let toolbar = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(self.title.clone()),
            )
            .child(self.mode_button("Unified", DiffMode::Unified, cx))
            .child(self.mode_button("Split", DiffMode::Split, cx));
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(toolbar)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_patch_is_accepted_by_the_diff_viewer() {
        let patch = unified_patch("src/a.txt", "one\ntwo\nthree\n", "one\n2\nthree\n");
        assert!(patch.starts_with("--- a/src/a.txt\n+++ b/src/a.txt\n"));
        let files = DiffFile::parse(&patch).expect("viewer parses similar's unified diff");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path().as_ref(), "src/a.txt");
    }

    #[test]
    fn unified_patch_of_a_new_file_parses() {
        let patch = unified_patch("new.txt", "", "hello\n");
        assert_eq!(
            DiffFile::parse(&patch).map(|files| files.len()).ok(),
            Some(1)
        );
    }
}
