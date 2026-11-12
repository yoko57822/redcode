use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
    relative,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{PanelEvent, PanelId, TabGroup};
use gpui_kit::component::highlighter::Language;
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, Position, RopeExt as _, TabSize,
};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};

use crate::actions::editor::Save;
use crate::fs_watch::{FsChanged, FsWatcher};
use crate::settings;
use crate::workspace::{AddTarget, TabHandle, Workspace};

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
/// Change events this soon after our own save are the save itself.
const SAVE_ECHO_WINDOW: Duration = Duration::from_secs(1);

enum ReadError {
    Missing,
    TooLarge,
    NotUtf8,
    Io(std::io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("파일이 없습니다"),
            Self::TooLarge => f.write_str("10MB를 초과합니다"),
            Self::NotUtf8 => f.write_str("UTF-8 텍스트가 아닙니다"),
            Self::Io(err) => err.fmt(f),
        }
    }
}

impl From<std::io::Error> for ReadError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io(err)
        }
    }
}

/// Reads a file the editor can open: at most 10MB of UTF-8.
fn read_text(path: &Path) -> Result<String, ReadError> {
    if std::fs::metadata(path)?.len() > MAX_FILE_SIZE {
        return Err(ReadError::TooLarge);
    }
    String::from_utf8(std::fs::read(path)?).map_err(|_| ReadError::NotUtf8)
}

/// What happened to the file on disk while the buffer had unsaved edits.
#[derive(Clone, Copy, PartialEq)]
enum DiskChange {
    Modified,
    Deleted,
}

pub struct EditorPanel {
    pub path: PathBuf,
    editor: Entity<EditorState>,
    dirty: bool,
    /// Bumped on every edit, so a save only clears `dirty` if nothing was typed meanwhile.
    generation: u64,
    disk_changed: Option<DiskChange>,
    saved_at: Option<Instant>,
    save_task: Option<Task<()>>,
    disk_task: Option<Task<()>>,
    tab: TabHandle,
    _subs: Vec<Subscription>,
}

impl EditorPanel {
    fn new(
        path: PathBuf,
        text: String,
        fs: &Entity<FsWatcher>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor_settings = &settings::get(cx).editor;
        let tab_size = TabSize {
            tab_size: editor_settings.tab_size,
            hard_tabs: false,
        };
        let soft_wrap = editor_settings.soft_wrap;
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| "text".into());
        let language = Language::from_str(&extension).name();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .line_number(true)
                .soft_wrap(soft_wrap)
                .tab_size(tab_size)
        });
        editor.update(cx, |state, cx| state.set_value(text, window, cx));

        let subs = vec![
            cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.generation += 1;
                    if !this.dirty {
                        this.dirty = true;
                        this.tab.refresh(cx);
                        cx.notify();
                    }
                }
            }),
            cx.subscribe_in(fs, window, Self::on_fs_changed),
        ];
        Self {
            path,
            editor,
            dirty: false,
            generation: 0,
            disk_changed: None,
            saved_at: None,
            save_task: None,
            disk_task: None,
            tab: TabHandle::default(),
            _subs: subs,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn file_name(&self) -> SharedString {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
            .into()
    }

    /// Moves the cursor to the start of a 1-based line and focuses the editor.
    fn go_to_line(&mut self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.set_cursor_position(Position::new(line.saturating_sub(1), 0), window, cx)
        });
    }

    /// Replaces the buffer, keeping the cursor line/column and scroll position without
    /// taking focus.
    fn replace_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            let mut position = state.cursor_position();
            let scroll = state.scroll_offset();
            state.set_value(text, window, cx);
            let last_line = state.text().lines_len().saturating_sub(1) as u32;
            position.line = position.line.min(last_line);
            let offset = state.text().position_to_offset(&position);
            state.set_selected_range(offset..offset, cx);
            state.set_scroll_offset(scroll, cx);
        });
    }

    fn mark_clean(&mut self, cx: &mut Context<Self>) {
        self.dirty = false;
        self.disk_changed = None;
        self.tab.refresh(cx);
        cx.notify();
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let path = self.path.clone();
        let generation = self.generation;
        self.save_task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let result = cx
                    .background_executor()
                    .spawn(async move { std::fs::write(&path, text) })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    this.finish_save(result, generation, window, cx)
                })
                .ok();
            },
        ));
    }

    fn finish_save(
        &mut self,
        result: std::io::Result<()>,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = result {
            window.push_notification(Notification::error(format!("저장 실패: {err}")), cx);
            return;
        }
        self.saved_at = Some(Instant::now());
        if generation == self.generation {
            self.mark_clean(cx);
        } else {
            self.disk_changed = None;
            cx.notify();
        }
        if self.path == settings::path() {
            // Outside this window's update, so a parse error notification can reach it.
            cx.defer(settings::reload);
        }
    }

    fn on_fs_changed(
        &mut self,
        _: &Entity<FsWatcher>,
        event: &FsChanged,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.0.contains(&self.path)
            || self
                .saved_at
                .is_some_and(|saved| saved.elapsed() < SAVE_ECHO_WINDOW)
        {
            return;
        }
        let path = self.path.clone();
        self.disk_task = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let read = cx
                    .background_executor()
                    .spawn(async move { read_text(&path) })
                    .await;
                this.update_in(cx, |this, window, cx| this.on_disk_read(read, window, cx))
                    .ok();
            },
        ));
    }

    fn on_disk_read(
        &mut self,
        read: Result<String, ReadError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match read {
            Err(ReadError::Missing) => self.disk_changed = Some(DiskChange::Deleted),
            _ if self.dirty => self.disk_changed = Some(DiskChange::Modified),
            Ok(text) => {
                if *self.editor.read(cx).text() != text {
                    self.replace_text(text, window, cx);
                }
                self.disk_changed = None;
            }
            // Unreadable now (binary, too large): keep the clean buffer as it is.
            Err(_) => {}
        }
        cx.notify();
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match read_text(&self.path) {
            Ok(text) => {
                self.replace_text(text, window, cx);
                self.mark_clean(cx);
            }
            Err(err) => window.push_notification(
                Notification::error(format!(
                    "열 수 없는 파일입니다: {} ({err})",
                    self.path.display()
                )),
                cx,
            ),
        }
    }

    fn keep(&mut self, cx: &mut Context<Self>) {
        self.disk_changed = None;
        cx.notify();
    }

    fn render_banner(&self, change: DiskChange, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let message = match change {
            DiskChange::Modified => "디스크의 파일이 변경되었습니다",
            DiskChange::Deleted => "파일이 삭제되었습니다",
        };
        h_flex()
            .gap_2()
            .px_3()
            .py_1()
            .bg(theme.muted)
            .border_b_1()
            .border_color(theme.border)
            .text_sm()
            .child(div().flex_1().text_color(theme.warning).child(message))
            .when(change == DiskChange::Modified, |this| {
                this.child(
                    Button::new("editor-reload")
                        .label("Reload")
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                )
            })
            .child(
                Button::new("editor-keep")
                    .label("Keep")
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _, _, cx| this.keep(cx))),
            )
    }
}

impl Workspace {
    /// Opens `path` (relative paths resolve against the root) in an editor tab, or reveals the
    /// tab that already shows it. `line` is 1-based.
    pub fn open_path(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = if path.is_relative() {
            self.root.join(path)
        } else {
            path
        };
        let panel = match self.find_panel::<EditorPanel>(cx, |panel| panel.path == path) {
            Some(panel) => {
                self.reveal_panel(PanelId::from(panel.entity_id()), window, cx);
                panel
            }
            None => {
                let text = match read_text(&path) {
                    Ok(text) => text,
                    Err(err) => {
                        window.push_notification(
                            Notification::error(format!(
                                "열 수 없는 파일입니다: {} ({err})",
                                path.display()
                            )),
                            cx,
                        );
                        return;
                    }
                };
                let fs = self.fs().clone();
                let panel = cx.new(|cx| EditorPanel::new(path, text, &fs, window, cx));
                self.add_center_panel(panel.clone(), AddTarget::ActiveGroup, window, cx);
                panel
            }
        };
        if let Some(line) = line {
            panel.update(cx, |panel, cx| panel.go_to_line(line, window, cx));
        }
    }
}

impl gpui_kit::base::dock::Panel for EditorPanel {
    fn panel_name(&self) -> &'static str {
        "Editor"
    }

    /// A dirty tab hides its close button, so it cannot be closed without confirmation.
    fn closable(&self, _cx: &App) -> bool {
        !self.dirty
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, _: &mut Context<Self>) {
        self.tab.set(group);
    }
}

impl gpui_kit::component::dock::Panel for EditorPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.file_name();
        if self.dirty {
            SharedString::from(format!("● {name}"))
        } else {
            name
        }
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for EditorPanel {}

impl Focusable for EditorPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for EditorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = &settings::get(cx).appearance;
        let font_family = SharedString::from(appearance.font_family.clone());
        let font_size = px(appearance.font_size);
        let banner = self
            .disk_changed
            .map(|change| self.render_banner(change, cx).into_any_element());
        v_flex()
            .key_context("Editor")
            .size_full()
            .on_action(cx.listener(Self::save))
            .children(banner)
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(&self.editor)
                        .bordered(false)
                        .h(relative(1.))
                        .font_family(font_family)
                        .text_size(font_size),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_text_rejects_missing_and_non_utf8_files() {
        let dir = std::env::temp_dir().join(format!("redcode-editor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        let binary = dir.join("b.bin");
        std::fs::write(&text, "가나다\n").unwrap();
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();

        let read = (
            read_text(&text),
            read_text(&binary),
            read_text(&dir.join("missing")),
        );
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(read.0.ok().as_deref(), Some("가나다\n"));
        assert!(matches!(read.1, Err(ReadError::NotUtf8)));
        assert!(matches!(read.2, Err(ReadError::Missing)));
    }
}
