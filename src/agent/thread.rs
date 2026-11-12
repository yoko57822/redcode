//! The backend-neutral contract (`AgentEvent` / `BackendCommand`) and the thread model the
//! agent panel renders.

use std::path::{Path, PathBuf};

use futures::channel::oneshot;
use gpui::{
    AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Task, WeakEntity,
};
use gpui_kit::component::diff::{DiffFile, DiffMode, DiffState};
use gpui_kit::component::text::TextViewState;

use crate::diff_panel::unified_patch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolContent {
    Text(String),
    Diff {
        path: PathBuf,
        old: Option<String>,
        new: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionKind {
    pub fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    pub kind: PermissionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Done,
    MaxTokens,
    MaxTurns,
    Refusal,
    Cancelled,
}

pub type Location = (PathBuf, Option<u32>);

/// Backend → UI.
#[derive(Debug)]
pub enum AgentEvent {
    Ready {
        label: String,
        modes: Vec<(String, String)>,
        current_mode: Option<String>,
    },
    MessageChunk(String),
    ThoughtChunk(String),
    ToolCall {
        id: String,
        title: String,
        kind: ToolKind,
        status: ToolStatus,
        content: Vec<ToolContent>,
        locations: Vec<Location>,
    },
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
        content: Option<Vec<ToolContent>>,
        locations: Option<Vec<Location>>,
    },
    /// `(content, status)` with status `pending` | `in_progress` | `completed`.
    Plan(Vec<(String, String)>),
    ModeChanged(String),
    /// The reply carries the chosen option id, or `None` when cancelled.
    Permission {
        tool_call_id: String,
        options: Vec<PermissionOption>,
        reply: oneshot::Sender<Option<String>>,
    },
    TurnEnded(TurnEnd),
    /// The backend is gone (process died, initialization failed).
    Fatal(String),
    /// A per-turn error; a `TurnEnded(Done)` follows.
    Error(String),
}

/// UI → backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendCommand {
    Prompt(String),
    Cancel,
    SetMode(String),
}

pub struct BackendHandle {
    pub commands: smol::channel::Sender<BackendCommand>,
    pub events: smol::channel::Receiver<AgentEvent>,
    pub _task: Task<()>,
}

/// A rendered diff of one `ToolContent::Diff` item.
pub struct ToolDiff {
    pub state: Entity<DiffState>,
    /// Number of patch lines, used to size the viewer.
    pub lines: usize,
}

pub type PendingPermission = (Vec<PermissionOption>, oneshot::Sender<Option<String>>);

pub enum Entry {
    User(SharedString),
    Assistant(Entity<TextViewState>),
    Thought(Entity<TextViewState>),
    Tool {
        id: String,
        title: SharedString,
        kind: ToolKind,
        status: ToolStatus,
        content: Vec<ToolContent>,
        /// Parallel to `content`: the parsed viewer for `Diff` items; `None` for text items and
        /// for diffs the viewer rejected (those show their new text instead).
        diffs: Vec<Option<ToolDiff>>,
        locations: Vec<Location>,
        permission: Option<PendingPermission>,
        expanded: bool,
    },
    Plan(Vec<(String, String)>),
    Notice {
        text: SharedString,
        is_error: bool,
    },
}

pub enum ThreadEvent {
    EntriesAppended(usize),
    EntryChanged(usize),
    StateChanged,
}

pub struct AgentThread {
    pub entries: Vec<Entry>,
    pub running: bool,
    pub dead: bool,
    pub label: SharedString,
    pub modes: Vec<(String, String)>,
    pub current_mode: Option<String>,
    commands: smol::channel::Sender<BackendCommand>,
    _backend: Task<()>,
    _pump: Task<()>,
}

impl EventEmitter<ThreadEvent> for AgentThread {}

const CONNECTION_CLOSED: &str = "에이전트 연결이 종료되었습니다";

impl AgentThread {
    pub fn new(backend: BackendHandle, cx: &mut Context<Self>) -> Self {
        let BackendHandle {
            commands,
            events,
            _task,
        } = backend;
        let pump = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                let event = events.recv().await;
                let closed = event.is_err();
                let event = event.unwrap_or_else(|_| AgentEvent::Fatal(CONNECTION_CLOSED.into()));
                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() || closed {
                    break;
                }
            }
        });
        Self {
            entries: Vec::new(),
            running: false,
            dead: false,
            label: SharedString::default(),
            modes: Vec::new(),
            current_mode: None,
            commands,
            _backend: _task,
            _pump: pump,
        }
    }

    /// Sends a prompt. Ignored while a turn is running or after the backend died.
    pub fn send(&mut self, text: String, cx: &mut Context<Self>) {
        if self.running || self.dead {
            return;
        }
        self.push(Entry::User(text.clone().into()), cx);
        self.running = true;
        let _ = self.commands.try_send(BackendCommand::Prompt(text));
        cx.emit(ThreadEvent::StateChanged);
        cx.notify();
    }

    /// Answers every pending permission request with `None` and asks the backend to cancel.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.reject_permissions(cx);
        let _ = self.commands.try_send(BackendCommand::Cancel);
        cx.notify();
    }

    pub fn answer_permission(
        &mut self,
        entry_ix: usize,
        option_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(Entry::Tool { permission, .. }) = self.entries.get_mut(entry_ix) else {
            return;
        };
        if let Some((_, reply)) = permission.take() {
            let _ = reply.send(Some(option_id));
            cx.emit(ThreadEvent::EntryChanged(entry_ix));
            cx.notify();
        }
    }

    pub fn set_mode(&mut self, id: String, _cx: &mut Context<Self>) {
        let _ = self.commands.try_send(BackendCommand::SetMode(id));
    }

    pub fn mode_name(&self) -> Option<&str> {
        let current = self.current_mode.as_deref()?;
        self.modes
            .iter()
            .find(|(id, _)| id == current)
            .map(|(_, name)| name.as_str())
    }

    fn push(&mut self, entry: Entry, cx: &mut Context<Self>) {
        self.entries.push(entry);
        cx.emit(ThreadEvent::EntriesAppended(1));
    }

    fn notice(&mut self, text: impl Into<SharedString>, is_error: bool, cx: &mut Context<Self>) {
        self.push(
            Entry::Notice {
                text: text.into(),
                is_error,
            },
            cx,
        );
    }

    fn reject_permissions(&mut self, cx: &mut Context<Self>) {
        for (ix, entry) in self.entries.iter_mut().enumerate() {
            if let Entry::Tool { permission, .. } = entry
                && let Some((_, reply)) = permission.take()
            {
                let _ = reply.send(None);
                cx.emit(ThreadEvent::EntryChanged(ix));
            }
        }
    }

    fn find_tool(&self, tool_id: &str) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::Tool { id, .. } if id == tool_id))
    }

    fn append_text(&mut self, text: String, thought: bool, cx: &mut Context<Self>) {
        let last = match (self.entries.last(), thought) {
            (Some(Entry::Assistant(state)), false) | (Some(Entry::Thought(state)), true) => {
                Some(state.clone())
            }
            _ => None,
        };
        match last {
            Some(state) => {
                state.update(cx, |state, cx| state.push_str(&text, cx));
                cx.emit(ThreadEvent::EntryChanged(self.entries.len() - 1));
            }
            // Agents stream blank chunks between blocks; those must not open an empty entry.
            None if text.trim().is_empty() => {}
            None => {
                let state = cx.new(|cx| TextViewState::markdown("", cx).selectable(true));
                state.update(cx, |state, cx| state.push_str(&text, cx));
                let entry = if thought {
                    Entry::Thought(state)
                } else {
                    Entry::Assistant(state)
                };
                self.push(entry, cx);
            }
        }
    }

    fn apply(&mut self, event: AgentEvent, cx: &mut Context<Self>) {
        match event {
            AgentEvent::Ready {
                label,
                modes,
                current_mode,
            } => {
                self.label = label.into();
                self.modes = modes;
                self.current_mode = current_mode;
                cx.emit(ThreadEvent::StateChanged);
            }
            AgentEvent::MessageChunk(text) => self.append_text(text, false, cx),
            AgentEvent::ThoughtChunk(text) => self.append_text(text, true, cx),
            AgentEvent::ToolCall {
                id,
                title,
                kind,
                status,
                content,
                locations,
            } => {
                let diffs = build_diffs(&content, cx);
                match self.find_tool(&id) {
                    Some(ix) => {
                        let Entry::Tool {
                            title: t,
                            kind: k,
                            status: s,
                            content: c,
                            diffs: d,
                            locations: l,
                            permission,
                            expanded,
                            ..
                        } = &mut self.entries[ix]
                        else {
                            unreachable!("find_tool returns tool entries");
                        };
                        *t = title.into();
                        *k = kind;
                        *s = status;
                        *c = content;
                        *d = diffs;
                        *l = locations;
                        if is_active(status) || permission.is_some() {
                            *expanded = true;
                        }
                        cx.emit(ThreadEvent::EntryChanged(ix));
                    }
                    None => {
                        let entry = Entry::Tool {
                            id,
                            title: title.into(),
                            kind,
                            status,
                            content,
                            diffs,
                            locations,
                            permission: None,
                            expanded: is_active(status),
                        };
                        self.push(entry, cx);
                    }
                }
            }
            AgentEvent::ToolCallUpdate {
                id,
                title,
                status,
                content,
                locations,
            } => {
                let Some(ix) = self.find_tool(&id) else {
                    return;
                };
                let diffs = content.as_ref().map(|content| build_diffs(content, cx));
                let Entry::Tool {
                    title: t,
                    status: s,
                    content: c,
                    diffs: d,
                    locations: l,
                    expanded,
                    ..
                } = &mut self.entries[ix]
                else {
                    unreachable!("find_tool returns tool entries");
                };
                if let Some(title) = title {
                    *t = title.into();
                }
                if let Some(status) = status {
                    *s = status;
                    if is_active(status) {
                        *expanded = true;
                    }
                }
                if let (Some(content), Some(diffs)) = (content, diffs) {
                    *c = content;
                    *d = diffs;
                }
                if let Some(locations) = locations {
                    *l = locations;
                }
                cx.emit(ThreadEvent::EntryChanged(ix));
            }
            AgentEvent::Plan(items) => {
                match self
                    .entries
                    .iter()
                    .rposition(|entry| matches!(entry, Entry::Plan(_)))
                {
                    Some(ix) => {
                        self.entries[ix] = Entry::Plan(items);
                        cx.emit(ThreadEvent::EntryChanged(ix));
                    }
                    None => self.push(Entry::Plan(items), cx),
                }
            }
            AgentEvent::ModeChanged(mode) => {
                self.current_mode = Some(mode);
                cx.emit(ThreadEvent::StateChanged);
            }
            AgentEvent::Permission {
                tool_call_id,
                options,
                reply,
            } => match self.find_tool(&tool_call_id) {
                Some(ix) => {
                    if let Entry::Tool {
                        permission,
                        expanded,
                        ..
                    } = &mut self.entries[ix]
                    {
                        *permission = Some((options, reply));
                        *expanded = true;
                    }
                    cx.emit(ThreadEvent::EntryChanged(ix));
                }
                None => {
                    let entry = Entry::Tool {
                        id: tool_call_id,
                        title: "Permission required".into(),
                        kind: ToolKind::Other,
                        status: ToolStatus::Pending,
                        content: Vec::new(),
                        diffs: Vec::new(),
                        locations: Vec::new(),
                        permission: Some((options, reply)),
                        expanded: true,
                    };
                    self.push(entry, cx);
                }
            },
            AgentEvent::TurnEnded(end) => {
                self.running = false;
                if let Some(text) = turn_end_notice(end) {
                    self.notice(text, false, cx);
                }
                cx.emit(ThreadEvent::StateChanged);
            }
            AgentEvent::Error(message) => self.notice(message, true, cx),
            AgentEvent::Fatal(message) => {
                // A backend that already reported its death also closes its channel afterwards.
                if self.dead {
                    return;
                }
                self.notice(message, true, cx);
                self.dead = true;
                self.running = false;
                self.reject_permissions(cx);
                cx.emit(ThreadEvent::StateChanged);
            }
        }
        cx.notify();
    }
}

fn is_active(status: ToolStatus) -> bool {
    matches!(status, ToolStatus::Pending | ToolStatus::InProgress)
}

pub fn turn_end_notice(end: TurnEnd) -> Option<&'static str> {
    match end {
        TurnEnd::Done => None,
        TurnEnd::MaxTokens => Some("출력 토큰 한도에 도달했습니다."),
        TurnEnd::MaxTurns => Some("최대 턴 수에 도달했습니다."),
        TurnEnd::Refusal => Some("모델이 요청을 거부했습니다."),
        TurnEnd::Cancelled => Some("취소됨"),
    }
}

/// The patch header path for a diff: absolute paths lose their leading `/` so the
/// `a/` / `b/` prefixes stay well-formed.
fn patch_path(path: &Path) -> String {
    path.to_string_lossy().trim_start_matches('/').to_string()
}

fn build_diffs<T>(content: &[ToolContent], cx: &mut Context<T>) -> Vec<Option<ToolDiff>> {
    content
        .iter()
        .map(|item| {
            let ToolContent::Diff { path, old, new } = item else {
                return None;
            };
            let patch = unified_patch(&patch_path(path), old.as_deref().unwrap_or(""), new);
            let files = DiffFile::parse(&patch)
                .ok()
                .filter(|files| !files.is_empty())?;
            let state = cx.new(|cx| DiffState::new(files, cx).with_mode(DiffMode::Unified));
            Some(ToolDiff {
                state,
                lines: patch.lines().count(),
            })
        })
        .collect()
}
