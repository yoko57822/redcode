pub mod colors;
pub mod element;
pub mod keys;
pub mod mouse;
pub mod panel;

use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, RawFd};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::{
    Event as AlacEvent, EventListener, Notify as _, OnResize as _, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{self, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use gpui::{
    App, AppContext as _, AsyncApp, ClipboardItem, Context, Entity, EventEmitter, Hsla, Pixels,
    SharedString, Task, WeakEntity, px,
};
use smallvec::SmallVec;

use crate::settings;
use colors::Palette;

#[derive(Clone, Copy, PartialEq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: Pixels,
    pub line_h: Pixels,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

impl TermSize {
    pub fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: f32::from(self.cell_w) as u16,
            cell_height: f32::from(self.line_h) as u16,
        }
    }
}

#[derive(Clone)]
pub struct Listener(smol::channel::Sender<AlacEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.0.try_send(event);
    }
}

pub enum TerminalEvent {
    Wakeup,
    TitleChanged,
    Exited,
}

pub struct SnapCell {
    pub row: usize,
    pub col: usize,
    pub c: char,
    pub zerowidth: SmallVec<[char; 2]>,
    pub fg: Hsla,
    /// `None` = default background.
    pub bg: Option<Hsla>,
    pub flags: Flags,
}

pub struct Snapshot {
    pub cells: Vec<SnapCell>,
    pub cols: usize,
    pub rows: usize,
    /// `(row, col, shape)`; `None` when outside the viewport or hidden.
    pub cursor: Option<(usize, usize, CursorShape)>,
    pub display_offset: usize,
    pub selection: Option<SelectionRange>,
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    size: TermSize,
    pub title: Option<SharedString>,
    pub exit_status: Option<ExitStatus>,
    shell_name: SharedString,
    child_pid: u32,
    master_fd: RawFd,
    scroll_accum: f32,
    _event_task: Task<()>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

const DECORATIONS: Flags = Flags::UNDERLINE
    .union(Flags::DOUBLE_UNDERLINE)
    .union(Flags::UNDERCURL)
    .union(Flags::DOTTED_UNDERLINE)
    .union(Flags::DASHED_UNDERLINE)
    .union(Flags::STRIKEOUT);

impl Terminal {
    /// Starts a shell on a new PTY in `cwd`. Runs on the foreground thread.
    pub fn spawn(cwd: PathBuf, cx: &mut App) -> anyhow::Result<Entity<Self>> {
        let config = settings::get(cx).terminal.clone();
        let program = if !config.shell.is_empty() {
            config.shell.clone()
        } else {
            std::env::var("SHELL")
                .ok()
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(|| "/bin/zsh".to_string())
        };
        let shell_name: SharedString = Path::new(&program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.clone())
            .into();

        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        env.insert("TERM_PROGRAM".to_string(), "redcode".to_string());
        env.insert(
            "TERM_PROGRAM_VERSION".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        if std::env::var_os("LANG").is_none() {
            env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
        }

        let options = tty::Options {
            shell: Some(tty::Shell::new(program, config.shell_args.clone())),
            working_directory: Some(cwd),
            drain_on_exit: true,
            env,
        };
        let size = TermSize {
            cols: 80,
            rows: 24,
            cell_w: px(8.),
            line_h: px(16.),
        };
        let pty = tty::new(&options, size.window_size(), 0)?;
        let child_pid = pty.child().id();
        let master_fd = pty.file().as_raw_fd();

        let (tx, rx) = smol::channel::unbounded();
        let term = Arc::new(FairMutex::new(Term::new(
            term::Config {
                scrolling_history: config.scrollback_lines,
                ..Default::default()
            },
            &size,
            Listener(tx.clone()),
        )));
        let event_loop = EventLoop::new(term.clone(), Listener(tx), pty, true, false)?;
        let notifier = Notifier(event_loop.channel());
        // The PTY is dropped on the reader thread when the loop ends.
        let _ = event_loop.spawn();

        Ok(cx.new(|cx| {
            let event_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                while let Ok(first) = rx.recv().await {
                    let mut batch = vec![first];
                    cx.background_executor()
                        .timer(Duration::from_millis(4))
                        .await;
                    while let Ok(event) = rx.try_recv() {
                        batch.push(event);
                    }
                    if this
                        .update(cx, |this, cx| this.process_events(batch, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            Self {
                term,
                notifier,
                size,
                title: None,
                exit_status: None,
                shell_name,
                child_pid,
                master_fd,
                scroll_accum: 0.,
                _event_task: event_task,
            }
        }))
    }

    fn process_events(&mut self, events: Vec<AlacEvent>, cx: &mut Context<Self>) {
        let mut wakeup = false;
        for event in events {
            match event {
                AlacEvent::Wakeup
                | AlacEvent::CursorBlinkingChange
                | AlacEvent::MouseCursorDirty => wakeup = true,
                AlacEvent::PtyWrite(text) => self.write(text.into_bytes()),
                AlacEvent::Title(title) => {
                    self.title = Some(title.into());
                    cx.emit(TerminalEvent::TitleChanged);
                }
                AlacEvent::ResetTitle => {
                    self.title = None;
                    cx.emit(TerminalEvent::TitleChanged);
                }
                AlacEvent::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                AlacEvent::ColorRequest(index, format) => {
                    let color = self.term.lock().colors()[index];
                    let rgb = color.unwrap_or_else(|| Palette::from_theme(cx).rgb(index));
                    self.write(format(rgb).into_bytes());
                }
                AlacEvent::TextAreaSizeRequest(format) => {
                    self.write(format(self.size.window_size()).into_bytes());
                }
                AlacEvent::ChildExit(status) => {
                    self.exit_status = Some(status);
                    cx.emit(TerminalEvent::Exited);
                }
                AlacEvent::ClipboardLoad(..) | AlacEvent::Bell | AlacEvent::Exit => {}
            }
        }
        if wakeup {
            cx.emit(TerminalEvent::Wakeup);
        }
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    pub fn set_size(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        self.notifier.on_resize(size.window_size());
    }

    pub fn scroll(&self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    pub fn select_all(&self) {
        let mut term = self.term.lock();
        let start = GridPoint::new(term.topmost_line(), Column(0));
        let end = GridPoint::new(term.bottommost_line(), term.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
    }

    /// Starts a selection at a viewport cell.
    pub fn start_selection(&self, ty: SelectionType, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let point = term::viewport_to_point(
            term.grid().display_offset(),
            GridPoint::new(row, Column(col)),
        );
        term.selection = Some(Selection::new(ty, point, side));
    }

    /// Extends the active selection to a viewport cell.
    pub fn update_selection(&self, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let point = term::viewport_to_point(
            term.grid().display_offset(),
            GridPoint::new(row, Column(col)),
        );
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
    }

    /// Drops the scrollback and asks the shell to redraw.
    pub fn clear(&self) {
        self.term.lock().grid_mut().clear_history();
        self.write(b"\x0c".as_slice());
    }

    pub fn paste(&self, text: &str) {
        if self.mode().contains(TermMode::BRACKETED_PASTE) {
            let body = text.replace('\x1b', "");
            self.write(format!("\x1b[200~{body}\x1b[201~").into_bytes());
        } else {
            self.write(text.replace("\r\n", "\r").replace('\n', "\r").into_bytes());
        }
    }

    pub fn set_focused(&self, focused: bool) {
        let report = {
            let mut term = self.term.lock();
            term.is_focused = focused;
            term.mode().contains(TermMode::FOCUS_IN_OUT)
        };
        if report {
            self.write(if focused {
                b"\x1b[I".as_slice()
            } else {
                b"\x1b[O".as_slice()
            });
        }
    }

    /// Accumulates a pixel scroll delta and returns the whole lines it now covers.
    pub fn accumulate_scroll(&mut self, delta_y: f32) -> i32 {
        let line_h = f32::from(self.size.line_h);
        self.scroll_accum += delta_y;
        let lines = (self.scroll_accum / line_h).trunc();
        self.scroll_accum -= lines * line_h;
        lines as i32
    }

    /// Finds the match after (`Right`) or before (`Left`) `current` and scrolls it into view.
    /// Without a current match the search starts at the top-left of the viewport.
    pub fn search_next(
        &self,
        regex: &mut RegexSearch,
        current: Option<&Match>,
        direction: Direction,
    ) -> Option<Match> {
        let mut term = self.term.lock();
        let origin = match (current, direction) {
            (Some(current), Direction::Right) => current.end().add(&*term, Boundary::None, 1),
            (Some(current), Direction::Left) => current.start().sub(&*term, Boundary::None, 1),
            (None, _) => {
                term::viewport_to_point(term.grid().display_offset(), GridPoint::new(0, Column(0)))
            }
        };
        let found = term.search_next(regex, origin, direction, Side::Left, None)?;
        term.scroll_to_point(*found.start());
        Some(found)
    }

    /// All matches intersecting the viewport.
    pub fn visible_matches(&self, regex: &mut RegexSearch) -> Vec<Match> {
        let term = self.term.lock();
        let offset = term.grid().display_offset();
        let start = term::viewport_to_point(offset, GridPoint::new(0, Column(0)));
        let end = term::viewport_to_point(
            offset,
            GridPoint::new(term.screen_lines() - 1, term.last_column()),
        );
        RegexIter::new(start, end, Direction::Right, &*term, regex).collect()
    }

    /// The working directory of the foreground process, for new tabs to inherit.
    pub fn working_directory(&self) -> Option<PathBuf> {
        if self.exit_status.is_some() {
            return None;
        }
        // SAFETY: `tcgetpgrp` only reads the fd; `proc_pidinfo` fills at most `size` bytes of the
        // zero-initialized struct, and the path is NUL-terminated within it on success.
        unsafe {
            let mut pid = libc::tcgetpgrp(self.master_fd);
            if pid <= 0 {
                pid = self.child_pid as libc::pid_t;
            }
            let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
            let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
            let written = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut libc::proc_vnodepathinfo as *mut libc::c_void,
                size,
            );
            if written != size {
                return None;
            }
            let path = std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast());
            let path = PathBuf::from(
                <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path.to_bytes()),
            );
            (!path.as_os_str().is_empty()).then_some(path)
        }
    }

    pub fn display_title(&self) -> SharedString {
        self.title
            .clone()
            .unwrap_or_else(|| self.shell_name.clone())
    }

    pub fn snapshot(&self, palette: &Palette) -> Snapshot {
        let term = self.term.lock();
        let content = term.renderable_content();
        let display_offset = content.display_offset;
        let overrides = content.colors;
        let rows = term.screen_lines();
        let cols = term.columns();

        let mut cells = Vec::with_capacity(rows * cols / 4);
        for indexed in content.display_iter {
            let cell = indexed.cell;
            let row = indexed.point.line.0 + display_offset as i32;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let flags = cell.flags;
            let default_bg = cell.bg == Color::Named(NamedColor::Background);
            let inverse = flags.contains(Flags::INVERSE);
            let c = if flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };
            if (c == ' ' || c == '\0') && default_bg && !inverse && !flags.intersects(DECORATIONS) {
                continue;
            }
            let mut fg = palette.resolve(cell.fg, overrides);
            let mut bg = (!default_bg).then(|| palette.resolve(cell.bg, overrides));
            if inverse {
                let swapped = bg.unwrap_or(palette.background);
                bg = Some(fg);
                fg = swapped;
            }
            if flags.contains(Flags::DIM) {
                fg = fg.opacity(0.66);
            }
            cells.push(SnapCell {
                row: row as usize,
                col: indexed.point.column.0,
                c: if c == '\0' { ' ' } else { c },
                zerowidth: cell
                    .zerowidth()
                    .map(SmallVec::from_slice)
                    .unwrap_or_default(),
                fg,
                bg,
                flags,
            });
        }

        let cursor = (content.cursor.shape != CursorShape::Hidden)
            .then(|| term::point_to_viewport(display_offset, content.cursor.point))
            .flatten()
            .filter(|point| point.line < rows)
            .map(|point| (point.line, point.column.0, content.cursor.shape));

        Snapshot {
            cells,
            cols,
            rows,
            cursor,
            display_offset,
            selection: content.selection,
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}
