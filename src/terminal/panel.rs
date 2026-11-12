use std::borrow::Cow;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Direction, Side};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::search::{Match, RegexSearch};
use gpui::{
    App, AppContext as _, AsyncApp, Bounds, ClipboardItem, Context, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, ScrollEvent, Styled as _, Subscription, Task, UTF16Selection,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{PanelEvent, PanelId, TabGroup};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex};

use super::element::{Geometry, TerminalElement};
use super::{Terminal, TerminalEvent, keys, mouse};
use crate::actions::terminal::{
    Clear, Copy, Find, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, SelectAll,
    SendKeystroke,
};
use crate::settings;
use crate::workspace::{TabHandle, Workspace};

pub(super) struct SearchBar {
    input: Entity<InputState>,
    regex: Option<RegexSearch>,
    current: Option<Match>,
    not_found: bool,
    _sub: Subscription,
}

/// A drag that left the viewport vertically keeps scrolling one line per frame.
#[derive(Clone, Copy)]
pub(super) struct Autoscroll {
    pub delta: i32,
    pub row: usize,
    pub col: usize,
    pub side: Side,
}

#[derive(Default)]
struct MouseState {
    selecting: bool,
    moved: bool,
    /// Base button code of a press that was reported to the application.
    reported_button: Option<u8>,
    last_cell: Option<(usize, usize)>,
    autoscroll: Option<Autoscroll>,
}

pub struct TerminalPanel {
    pub(super) terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
    pub(super) marked_text: Option<String>,
    /// Caret inside the composition (UTF-16), as the IME last set it.
    marked_selection: Range<usize>,
    /// Recorded by the element during paint; the IME candidate window anchors here.
    pub(super) cursor_bounds: Option<Bounds<Pixels>>,
    pub(super) blink_on: bool,
    blink_task: Option<Task<()>>,
    search: Option<SearchBar>,
    mouse: MouseState,
    tab: TabHandle,
    workspace: WeakEntity<Workspace>,
    _subs: Vec<Subscription>,
}

fn bytes(text: Cow<'static, str>) -> Cow<'static, [u8]> {
    match text {
        Cow::Borrowed(text) => Cow::Borrowed(text.as_bytes()),
        Cow::Owned(text) => Cow::Owned(text.into_bytes()),
    }
}

impl TerminalPanel {
    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let subs = vec![
            cx.subscribe_in(&terminal, window, Self::on_terminal_event),
            cx.on_focus_in(&focus_handle, window, |this, _, cx| {
                this.terminal.read(cx).set_focused(true);
                this.blink_on = true;
                this.start_blink(cx);
                cx.notify();
            }),
            cx.on_focus_out(&focus_handle, window, |this, _, _, cx| {
                this.terminal.read(cx).set_focused(false);
                this.blink_task = None;
                this.blink_on = true;
                cx.notify();
            }),
        ];
        Self {
            terminal,
            focus_handle,
            marked_text: None,
            marked_selection: 0..0,
            cursor_bounds: None,
            blink_on: true,
            blink_task: None,
            search: None,
            mouse: MouseState::default(),
            tab: TabHandle::default(),
            workspace,
            _subs: subs,
        }
    }

    pub fn exited(&self, cx: &App) -> bool {
        self.terminal.read(cx).exit_status.is_some()
    }

    pub fn working_directory(&self, cx: &App) -> Option<PathBuf> {
        self.terminal.read(cx).working_directory()
    }

    pub(super) fn autoscroll(&self) -> Option<Autoscroll> {
        self.mouse.autoscroll
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        if !settings::get(cx).terminal.cursor_blink {
            self.blink_task = None;
            return;
        }
        self.blink_task = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(530))
                        .await;
                    let alive = this.update(cx, |this, cx| {
                        this.blink_on = !this.blink_on;
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }),
        );
    }

    fn on_terminal_event(
        &mut self,
        _terminal: &Entity<Terminal>,
        event: &TerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::Wakeup => {
                self.blink_on = true;
                cx.notify();
            }
            TerminalEvent::TitleChanged => {
                self.tab.refresh(cx);
                cx.notify();
            }
            TerminalEvent::Exited => {
                let clean = self
                    .terminal
                    .read(cx)
                    .exit_status
                    .is_some_and(|status| status.success());
                if clean {
                    let id = PanelId::from(cx.entity_id());
                    let workspace = self.workspace.clone();
                    window.defer(cx, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| workspace.close_panel(id, window, cx))
                            .ok();
                    });
                }
                cx.notify();
            }
        }
    }

    /// Writes user input to the PTY and snaps the viewport back to the bottom.
    fn send_input(&mut self, bytes: impl Into<Cow<'static, [u8]>>, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        terminal.write(bytes);
        terminal.scroll(Scroll::Bottom);
        self.blink_on = true;
        cx.notify();
    }

    fn send_keystroke_sequence(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let option_as_meta = settings::get(cx).terminal.option_as_meta;
        let mode = self.terminal.read(cx).mode();
        let Some(sequence) = keys::to_esc(keystroke, mode, option_as_meta) else {
            return false;
        };
        self.send_input(bytes(sequence), cx);
        true
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys typed into the search bar bubble up to here; they are not terminal input.
        if !self.focus_handle.is_focused(window) {
            return;
        }
        // Leave the keystroke to the IME while it composes or prefers character input.
        if (event.prefer_character_input && event.keystroke.key_char.is_some())
            || self.marked_text.is_some()
        {
            return;
        }
        if self.send_keystroke_sequence(&event.keystroke, cx) {
            cx.stop_propagation();
        }
    }

    fn send_keystroke(
        &mut self,
        action: &SendKeystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus_handle.is_focused(window) {
            cx.propagate();
            return;
        }
        if let Ok(keystroke) = Keystroke::parse(action.0) {
            self.send_keystroke_sequence(&keystroke, cx);
        }
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.terminal.read(cx).selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let terminal = self.terminal.read(cx);
        terminal.paste(&text);
        terminal.scroll(Scroll::Bottom);
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.read(cx).select_all();
        cx.notify();
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.read(cx).clear();
        cx.notify();
    }

    fn scroll_to(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        self.terminal.read(cx).scroll(scroll);
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to(Scroll::PageUp, cx);
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to(Scroll::PageDown, cx);
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to(Scroll::Top, cx);
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to(Scroll::Bottom, cx);
    }

    // --- Mouse -------------------------------------------------------------------------------

    fn reports_mouse(&self, shift: bool, cx: &App) -> Option<TermMode> {
        let mode = self.terminal.read(cx).mode();
        (mode.intersects(TermMode::MOUSE_MODE) && !shift).then_some(mode)
    }

    pub(super) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        geometry: Geometry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let (col, row, side) = geometry.cell(event.position);
        if let Some(mode) = self.reports_mouse(event.modifiers.shift, cx) {
            let button = match event.button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            if let Some(report) = mouse::mouse_report(button, true, col, row, event.modifiers, mode)
            {
                self.terminal.read(cx).write(report);
            }
            self.mouse.reported_button = Some(button);
            self.mouse.last_cell = Some((col, row));
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let ty = match event.click_count {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        self.terminal.read(cx).start_selection(ty, row, col, side);
        self.mouse.selecting = true;
        self.mouse.moved = event.click_count > 1;
        self.mouse.autoscroll = None;
        cx.notify();
    }

    pub(super) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        geometry: Geometry,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let (col, row, side) = geometry.cell(event.position);
        if self.mouse.selecting {
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            self.mouse.moved = true;
            let top = geometry.origin.y;
            let bottom = top + geometry.line_h * geometry.rows;
            let delta = if event.position.y < top {
                1
            } else if event.position.y > bottom {
                -1
            } else {
                0
            };
            self.mouse.autoscroll = (delta != 0).then_some(Autoscroll {
                delta,
                row,
                col,
                side,
            });
            self.terminal.read(cx).update_selection(row, col, side);
            cx.notify();
            return;
        }
        let Some(mode) = self.reports_mouse(event.modifiers.shift, cx) else {
            return;
        };
        let pressed = event.pressed_button.is_some() && self.mouse.reported_button.is_some();
        if !pressed && !hovered {
            return;
        }
        let motion = mode.contains(TermMode::MOUSE_MOTION)
            || (mode.contains(TermMode::MOUSE_DRAG) && pressed);
        if !motion || self.mouse.last_cell == Some((col, row)) {
            return;
        }
        let button = match self.mouse.reported_button.filter(|_| pressed) {
            Some(button) => 32 + button,
            None => 35,
        };
        if let Some(report) = mouse::mouse_report(button, true, col, row, event.modifiers, mode) {
            self.terminal.read(cx).write(report);
        }
        self.mouse.last_cell = Some((col, row));
    }

    pub(super) fn mouse_up(
        &mut self,
        event: &MouseUpEvent,
        geometry: Geometry,
        cx: &mut Context<Self>,
    ) {
        if let Some(button) = self.mouse.reported_button.take() {
            let mode = self.terminal.read(cx).mode();
            if mode.intersects(TermMode::MOUSE_MODE) {
                let (col, row, _) = geometry.cell(event.position);
                if let Some(report) =
                    mouse::mouse_report(button, false, col, row, event.modifiers, mode)
                {
                    self.terminal.read(cx).write(report);
                }
            }
            return;
        }
        if !self.mouse.selecting || event.button != MouseButton::Left {
            return;
        }
        self.mouse.selecting = false;
        self.mouse.autoscroll = None;
        if !self.mouse.moved {
            self.terminal.read(cx).clear_selection();
        } else if settings::get(cx).terminal.copy_on_select {
            self.copy_selection(cx);
        }
        cx.notify();
    }

    pub(super) fn scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        geometry: Geometry,
        cx: &mut Context<Self>,
    ) {
        let delta_y = f32::from(event.delta.pixel_delta(geometry.line_h).y);
        let lines = self
            .terminal
            .update(cx, |terminal, _| terminal.accumulate_scroll(delta_y));
        if lines == 0 {
            return;
        }
        let terminal = self.terminal.read(cx);
        let mode = terminal.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            let (col, row, _) = geometry.cell(event.position);
            let button = if lines > 0 { 64 } else { 65 };
            if let Some(report) = mouse::mouse_report(button, true, col, row, event.modifiers, mode)
            {
                for _ in 0..lines.unsigned_abs() {
                    terminal.write(report.clone());
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let sequence: &'static [u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
            for _ in 0..lines.unsigned_abs() {
                terminal.write(sequence);
            }
        } else {
            terminal.scroll(Scroll::Delta(lines));
        }
        cx.notify();
    }

    // --- Search ------------------------------------------------------------------------------

    fn open_search(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = &self.search {
            bar.input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("찾기"));
        let sub = cx.subscribe_in(&input, window, Self::on_search_event);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.search = Some(SearchBar {
            input,
            regex: None,
            current: None,
            not_found: false,
            _sub: sub,
        });
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value();
                if let Some(bar) = self.search.as_mut() {
                    // Literal search; alacritty ignores case unless the query has an uppercase letter.
                    bar.regex = (!query.is_empty())
                        .then(|| RegexSearch::new(&regex::escape(&query)).ok())
                        .flatten();
                    bar.current = None;
                }
                self.search_step(Direction::Right, cx);
            }
            InputEvent::PressEnter { shift, .. } => {
                let direction = if *shift {
                    Direction::Left
                } else {
                    Direction::Right
                };
                self.search_step(direction, cx);
            }
            _ => {}
        }
    }

    fn search_step(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(bar) = self.search.as_mut() else {
            return;
        };
        match bar.regex.as_mut() {
            Some(regex) => {
                bar.current =
                    self.terminal
                        .read(cx)
                        .search_next(regex, bar.current.as_ref(), direction);
                bar.not_found = bar.current.is_none();
            }
            None => {
                bar.current = None;
                bar.not_found = false;
            }
        }
        cx.notify();
    }

    /// Matches inside the viewport, flagged when they are the current match.
    pub(super) fn search_highlights(&mut self, cx: &App) -> Vec<(Match, bool)> {
        let Some(bar) = self.search.as_mut() else {
            return Vec::new();
        };
        let Some(regex) = bar.regex.as_mut() else {
            return Vec::new();
        };
        let current = bar.current.clone();
        self.terminal
            .read(cx)
            .visible_matches(regex)
            .into_iter()
            .map(|found| {
                let is_current = current.as_ref() == Some(&found);
                (found, is_current)
            })
            .collect()
    }

    fn render_search(&self, bar: &SearchBar, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let border = if bar.not_found {
            theme.danger
        } else {
            theme.border
        };
        h_flex()
            .absolute()
            .top(px(6.))
            .right(px(14.))
            .gap_1()
            .p_1()
            .bg(theme.background)
            .border_1()
            .border_color(border)
            .rounded(theme.radius)
            .shadow_md()
            .occlude()
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.close_search(window, cx)))
            .child(
                div()
                    .w(px(220.))
                    .child(Input::new(&bar.input).appearance(false).small()),
            )
            .child(
                Button::new("terminal-search-prev")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::ChevronUp))
                    .on_click(cx.listener(|this, _, _, cx| this.search_step(Direction::Left, cx))),
            )
            .child(
                Button::new("terminal-search-next")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::ChevronDown))
                    .on_click(cx.listener(|this, _, _, cx| this.search_step(Direction::Right, cx))),
            )
            .child(
                Button::new("terminal-search-close")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::X))
                    .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
            )
    }
}

impl EntityInputHandler for TerminalPanel {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // Only the composition is addressable; the grid is not a text document.
        let units: Vec<u16> = self.marked_text.as_ref()?.encode_utf16().collect();
        let end = range_utf16.end.min(units.len());
        let start = range_utf16.start.min(end);
        *adjusted = Some(start..end);
        String::from_utf16(&units[start..end]).ok()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // While composing, the IME expects the caret where it put it; a caret elsewhere makes
        // it commit the composition after every keystroke.
        let range = match &self.marked_text {
            Some(_) => self.marked_selection.clone(),
            None => 0..0,
        };
        Some(UTF16Selection {
            range,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        self.send_input(text.as_bytes().to_vec(), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = new_text.encode_utf16().count();
        self.marked_selection = new_selected_range
            .map(|range| range.start.min(len)..range.end.min(len))
            .unwrap_or(len..len);
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl gpui_kit::base::dock::Panel for TerminalPanel {
    fn panel_name(&self) -> &'static str {
        "Terminal"
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, _: &mut Context<Self>) {
        self.tab.set(group);
    }
}

impl gpui_kit::component::dock::Panel for TerminalPanel {
    fn title(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.terminal.read(cx).display_title()
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let exit_status = self.terminal.read(cx).exit_status;
        let search = self
            .search
            .as_ref()
            .map(|bar| self.render_search(bar, cx).into_any_element());
        div()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::send_keystroke))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_action(cx.listener(Self::open_search))
            .child(div().flex_1().min_h_0().child(TerminalElement {
                terminal: self.terminal.clone(),
                panel: cx.entity(),
                focus: self.focus_handle.clone(),
            }))
            .when_some(exit_status, |this, status| {
                this.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("[프로세스가 종료되었습니다: {status}]")),
                )
            })
            .children(search)
    }
}
