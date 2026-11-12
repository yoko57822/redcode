//! The agent console tab: one thread, its transcript and the prompt input.

use std::cell::Cell;
use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::Diff;
use gpui_kit::component::dock::{PanelEvent, TabGroup};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};

use super::thread::{
    AgentThread, BackendHandle, Entry, ThreadEvent, ToolContent, ToolKind, ToolStatus,
};
use super::{acp, native};
use crate::actions::agent::Cancel;
use crate::settings::{self, Backend};
use crate::workspace::{TabHandle, Workspace};

/// Keychain entry holding the Anthropic API key.
const KEYCHAIN_URL: &str = "https://api.anthropic.com";
const TEXT_MAX_HEIGHT: f32 = 240.;
const DIFF_MAX_HEIGHT: f32 = 320.;

pub struct AgentPanel {
    backend: Backend,
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    /// `None` while the native backend's API key is being looked up.
    thread: Option<Entity<AgentThread>>,
    scroller: Entity<MessageScrollerState>,
    input: Entity<TextareaState>,
    tab: TabHandle,
    start_task: Option<Task<()>>,
    thread_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
}

impl AgentPanel {
    pub fn new(
        backend: Backend,
        root: PathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("메시지 입력 (Enter 전송, Shift+Enter 줄바꿈)")
                .submit_on_enter(true)
                .auto_grow(2, 8)
        });
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let subs = vec![cx.subscribe_in(&input, window, Self::on_input_event)];
        let mut this = Self {
            backend,
            root,
            workspace,
            thread: None,
            scroller,
            input,
            tab: TabHandle::default(),
            start_task: None,
            thread_sub: None,
            _subs: subs,
        };
        this.start(cx);
        this
    }

    /// Starts a fresh backend and thread of this panel's kind.
    fn start(&mut self, cx: &mut Context<Self>) {
        let agent = settings::get(cx).agent.clone();
        match self.backend {
            Backend::Acp => {
                let handle = acp::spawn(self.root.clone(), agent.acp, cx);
                self.attach(handle, cx);
            }
            Backend::Native => {
                let cfg = agent.native;
                let from_env = std::env::var(&cfg.api_key_env)
                    .ok()
                    .filter(|key| !key.is_empty());
                if from_env.is_some() {
                    let handle = native::session::spawn(self.root.clone(), cfg, from_env, cx);
                    self.attach(handle, cx);
                    return;
                }
                let credentials = cx.read_credentials(KEYCHAIN_URL);
                self.start_task = Some(cx.spawn(
                    async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                        let key = credentials
                            .await
                            .ok()
                            .flatten()
                            .and_then(|(_, secret)| String::from_utf8(secret).ok())
                            .filter(|key| !key.is_empty());
                        this.update(cx, |this, cx| {
                            let handle = native::session::spawn(this.root.clone(), cfg, key, cx);
                            this.attach(handle, cx);
                        })
                        .ok();
                    },
                ));
            }
        }
    }

    fn attach(&mut self, handle: BackendHandle, cx: &mut Context<Self>) {
        let thread = cx.new(|cx| AgentThread::new(handle, cx));
        self.thread_sub = Some(cx.subscribe(&thread, Self::on_thread_event));
        self.thread = Some(thread);
        self.scroller
            .update(cx, |scroller, cx| scroller.reset(0, cx));
        self.input
            .update(cx, |input, cx| input.set_disabled(false, cx));
        self.tab.refresh(cx);
        cx.notify();
    }

    fn on_thread_event(
        &mut self,
        thread: Entity<AgentThread>,
        event: &ThreadEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ThreadEvent::EntriesAppended(count) => {
                self.scroller.update(cx, |scroller, cx| {
                    scroller.append(*count, cx);
                });
            }
            ThreadEvent::EntryChanged(ix) => {
                self.scroller.update(cx, |scroller, cx| {
                    scroller.remeasure_items(*ix..*ix + 1, cx);
                });
            }
            ThreadEvent::StateChanged => {
                let dead = thread.read(cx).dead;
                self.input
                    .update(cx, |input, cx| input.set_disabled(dead, cx));
                self.tab.refresh(cx);
            }
        }
        cx.notify();
    }

    fn on_input_event(
        &mut self,
        _: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
            self.send(window, cx);
        }
    }

    fn is_running(&self, cx: &App) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).running)
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.thread.clone() else {
            return;
        };
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || thread.read(cx).running || thread.read(cx).dead {
            return;
        }
        thread.update(cx, |thread, cx| thread.send(text, cx));
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(thread) = &self.thread {
            thread.update(cx, |thread, cx| thread.cancel(cx));
        }
    }

    fn on_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_running(cx) {
            self.stop(cx);
        } else {
            cx.propagate();
        }
    }

    fn toggle_tool(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(thread) = &self.thread else {
            return;
        };
        thread.update(cx, |thread, cx| {
            if let Some(Entry::Tool { expanded, .. }) = thread.entries.get_mut(ix) {
                *expanded = !*expanded;
                cx.emit(ThreadEvent::EntryChanged(ix));
            }
        });
    }

    fn open_location(&self, path: PathBuf, line: Option<u32>, window: &mut Window, cx: &mut App) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path(path, line, window, cx)
                })
                .ok();
        });
    }

    fn render_tool(&self, ix: usize, entry: &Entry, cx: &Context<Self>) -> AnyElement {
        let Entry::Tool {
            title,
            kind,
            status,
            content,
            diffs,
            locations,
            permission,
            expanded,
            ..
        } = entry
        else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let icon = match kind {
            ToolKind::Read => IconName::File,
            ToolKind::Edit => IconName::Pencil,
            ToolKind::Search => IconName::Search,
            ToolKind::Execute => IconName::SquareTerminal,
            _ => IconName::Wrench,
        };
        let (badge, badge_color) = match status {
            ToolStatus::Pending => ("Pending", theme.muted_foreground),
            ToolStatus::InProgress => ("Running", theme.blue),
            ToolStatus::Completed => ("Done", theme.success),
            ToolStatus::Failed => ("Failed", theme.danger),
        };
        let appearance = &settings::get(cx).appearance;
        let mono = SharedString::from(appearance.font_family.clone());

        let header = h_flex()
            .id(("tool-header", ix))
            .px_2()
            .py_1()
            .gap_2()
            .text_sm()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(ix, cx)))
            .child(Icon::new(icon).small())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(badge_color)
                    .child(badge),
            );

        let mut body = v_flex().gap_2().px_2().pb_2();
        for (item_ix, item) in content.iter().enumerate() {
            let diff = diffs.get(item_ix).and_then(Option::as_ref);
            body = body.child(match (item, diff) {
                (ToolContent::Diff { .. }, Some(diff)) => {
                    // Room for the file header plus the patch lines, up to the cap.
                    let height = (diff.lines as f32 * 20. + 48.).min(DIFF_MAX_HEIGHT);
                    div()
                        .h(px(height))
                        .border_1()
                        .border_color(theme.border)
                        .child(Diff::new(&diff.state))
                        .into_any_element()
                }
                (ToolContent::Text(text), _) | (ToolContent::Diff { new: text, .. }, None) => div()
                    .id(("tool-text", ix * 1000 + item_ix))
                    .max_h(px(TEXT_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .p_2()
                    .bg(theme.muted)
                    .rounded(theme.radius)
                    .font_family(mono.clone())
                    .text_xs()
                    .children(text.lines().map(|line| {
                        div().child(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line.to_string()
                        })
                    }))
                    .into_any_element(),
            });
        }
        for (location_ix, (path, line)) in locations.iter().enumerate() {
            let label = match line {
                Some(line) => format!("{}:{line}", path.display()),
                None => path.display().to_string(),
            };
            let (path, line) = (path.clone(), *line);
            body = body.child(
                div()
                    .id(("tool-location", ix * 1000 + location_ix))
                    .text_xs()
                    .text_color(theme.blue)
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_location(path.clone(), line, window, cx);
                    })),
            );
        }
        if let Some((options, _)) = permission {
            let mut buttons = h_flex().gap_2();
            for (option_ix, option) in options.iter().enumerate() {
                let id = option.id.clone();
                let button = Button::new(("permission", ix * 1000 + option_ix))
                    .label(option.label.clone())
                    .small();
                let button = if option.kind.is_allow() {
                    button.primary()
                } else {
                    button.ghost()
                };
                buttons =
                    buttons.child(button.on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(thread) = &this.thread {
                            thread.update(cx, |thread, cx| {
                                thread.answer_permission(ix, id.clone(), cx);
                            });
                        }
                        // The buttons disappear with the request; keep typing possible.
                        this.input.update(cx, |input, cx| input.focus(window, cx));
                    })));
            }
            body = body.child(buttons);
        }

        v_flex()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .child(header)
            .when(*expanded, |this| this.child(body))
            .into_any_element()
    }

    fn render_entry(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let Some(thread) = &self.thread else {
            return div().into_any_element();
        };
        let Some(entry) = thread.read(cx).entries.get(ix) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let content = match entry {
            Entry::User(text) => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.8))
                        .px_3()
                        .py_2()
                        .rounded(theme.radius_lg)
                        .bg(theme.muted)
                        .child(text.clone()),
                )
                .into_any_element(),
            Entry::Assistant(state) => TextView::new(state).selectable(true).into_any_element(),
            Entry::Thought(state) => div()
                .pl_3()
                .border_l_2()
                .border_color(theme.border)
                .text_color(theme.muted_foreground)
                .text_sm()
                .child(TextView::new(state).selectable(true))
                .into_any_element(),
            Entry::Plan(items) => v_flex()
                .gap_1()
                .text_sm()
                .children(items.iter().map(|(content, status)| {
                    let (mark, color) = match status.as_str() {
                        "completed" => ("☑", theme.muted_foreground),
                        "in_progress" => ("◐", theme.blue),
                        _ => ("☐", theme.foreground),
                    };
                    h_flex()
                        .gap_2()
                        .items_start()
                        .text_color(color)
                        .child(div().flex_none().child(mark))
                        .child(
                            div()
                                .when(status == "completed", |this| this.line_through())
                                .child(content.clone()),
                        )
                }))
                .into_any_element(),
            Entry::Notice { text, is_error } => div()
                .text_sm()
                .text_color(if *is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text.clone())
                .into_any_element(),
            Entry::Tool { .. } => self.render_tool(ix, entry, cx),
        };
        div().px_4().py_1p5().child(content).into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let thread = self.thread.as_ref().map(|thread| thread.read(cx));
        let label: SharedString = thread
            .map(|thread| thread.label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "Agent".into());
        let running = thread.is_some_and(|thread| thread.running);
        let modes = thread
            .map(|thread| thread.modes.clone())
            .unwrap_or_default();
        let current = thread.and_then(|thread| thread.current_mode.clone());
        let mode_name: SharedString = thread
            .and_then(|thread| thread.mode_name())
            .unwrap_or("Mode")
            .to_string()
            .into();
        let thread_entity = self.thread.clone();

        h_flex()
            .flex_none()
            .h(px(32.))
            .px_3()
            .gap_2()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(IconName::Bot).small())
            .child(div().flex_1().min_w_0().overflow_hidden().child(label))
            .when(running, |this| this.child(Spinner::new().small()))
            .when(!modes.is_empty(), |this| {
                this.child(
                    Button::new("agent-mode")
                        .label(mode_name)
                        .ghost()
                        .xsmall()
                        .dropdown_menu(move |menu, _, _| {
                            let mut menu = menu;
                            for (id, name) in &modes {
                                let thread = thread_entity.clone();
                                let mode = id.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(name.clone())
                                        .checked(current.as_ref() == Some(id))
                                        .on_click(move |_, _, cx| {
                                            if let Some(thread) = &thread {
                                                thread.update(cx, |thread, cx| {
                                                    thread.set_mode(mode.clone(), cx);
                                                });
                                            }
                                        }),
                                );
                            }
                            menu
                        }),
                )
            })
    }
}

/// Asks for the Anthropic API key and stores it in the keychain.
pub fn set_api_key_dialog(window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder("sk-ant-…")
    });
    let focus_on_mount = Cell::new(true);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        if focus_on_mount.replace(false) {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        let key_input = input.clone();
        alert
            .title("Anthropic API Key")
            .child(Input::new(&input))
            .confirm()
            .ok_text("Save")
            .on_ok(move |_, window, cx| {
                let key = key_input.read(cx).value().trim().to_string();
                if key.is_empty() {
                    return false;
                }
                let write = cx.write_credentials(KEYCHAIN_URL, "api-key", key.as_bytes());
                window
                    .spawn(cx, async move |cx: &mut AsyncWindowContext| {
                        let note = match write.await {
                            Ok(()) => Notification::success(
                                "API 키를 키체인에 저장했습니다. 새 스레드부터 적용됩니다.",
                            ),
                            Err(err) => Notification::error(format!("키체인 저장 실패: {err}")),
                        };
                        cx.update(|window, cx| window.push_notification(note, cx))
                            .ok();
                    })
                    .detach();
                true
            })
    });
}

impl gpui_kit::base::dock::Panel for AgentPanel {
    fn panel_name(&self) -> &'static str {
        "Agent"
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, _: &mut Context<Self>) {
        self.tab.set(group);
    }
}

impl gpui_kit::component::dock::Panel for AgentPanel {
    fn title(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .thread
            .as_ref()
            .map(|thread| thread.read(cx).label.clone())
            .filter(|label| !label.is_empty());
        match label {
            Some(label) => SharedString::from(format!("Agent · {label}")),
            None => "Agent".into(),
        }
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for AgentPanel {}

impl Focusable for AgentPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for AgentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running(cx);
        let dead = self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).dead);
        let panel = cx.entity();
        let transcript = MessageScroller::new(
            "agent-thread",
            self.scroller.clone(),
            move |ix, _window, cx| panel.update(cx, |panel, cx| panel.render_entry(ix, cx)),
        )
        // Entries bring their own vertical padding; the default 2rem row gap is too airy.
        .with_row_style(StyleRefinement::default().pb_0());

        let action = if dead {
            Button::new("agent-restart")
                .label("Restart")
                .small()
                .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
        } else if running {
            Button::new("agent-stop")
                .label("Stop")
                .small()
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("agent-send")
                .label("Send")
                .primary()
                .small()
                .disabled(self.thread.is_none())
                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
        };
        let header = self.render_header(cx);
        let theme = cx.theme();

        v_flex()
            .key_context("Agent")
            .size_full()
            .on_action(cx.listener(Self::on_cancel))
            .child(header)
            .child(div().flex_1().min_h_0().child(transcript))
            .child(
                h_flex()
                    .flex_none()
                    .items_end()
                    .gap_2()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().flex_1().min_w_0().child(Textarea::new(&self.input)))
                    .child(action),
            )
    }
}
