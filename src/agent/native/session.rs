//! The native backend's command loop: one Messages API conversation with a local tool loop.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::oneshot;
use futures::future::{Either, select};
use gpui::{App, AppContext as _};
use serde_json::{Value, json};

use super::api::{self, StreamEvent};
use super::tools::{self, ToolResult};
use crate::agent::thread::{
    AgentEvent, BackendCommand, BackendHandle, PermissionKind, PermissionOption, ToolContent,
    ToolKind, ToolStatus, TurnEnd,
};
use crate::settings::NativeSettings;

const MISSING_KEY: &str =
    "API 키가 설정되지 않았습니다. 명령 팔레트에서 Agent: Set Anthropic API Key를 실행하세요.";
const REJECTED: &str = "User rejected this action.";
const CANCELLED: &str = "Cancelled by user.";

type EventTx = smol::channel::Sender<AgentEvent>;
type CommandRx = smol::channel::Receiver<BackendCommand>;

pub fn spawn(
    root: PathBuf,
    cfg: NativeSettings,
    api_key: Option<String>,
    cx: &mut App,
) -> BackendHandle {
    let (commands, cmd_rx) = smol::channel::unbounded();
    let (events_tx, events) = smol::channel::unbounded();
    let _ = events_tx.try_send(AgentEvent::Ready {
        label: cfg.model.clone(),
        modes: Vec::new(),
        current_mode: None,
    });
    let task = cx.background_spawn(run(Session::new(root, cfg, events_tx), api_key, cmd_rx));
    BackendHandle {
        commands,
        events,
        _task: task,
    }
}

fn system_prompt(root: &std::path::Path) -> String {
    format!(
        "You are a coding agent running inside redcode, a desktop terminal and git workbench. \
         The workspace root is {}. Use the provided tools to read, search, edit files and run \
         shell commands. Prefer edit_file over write_file for existing files. Keep answers \
         concise and reference files as relative paths.",
        root.display()
    )
}

/// How a turn ends for a `stop_reason`, or `None` when the loop continues (`tool_use`,
/// `pause_turn`).
fn turn_end(stop_reason: &str) -> Option<TurnEnd> {
    match stop_reason {
        "tool_use" | "pause_turn" => None,
        "max_tokens" => Some(TurnEnd::MaxTokens),
        "refusal" => Some(TurnEnd::Refusal),
        _ => Some(TurnEnd::Done),
    }
}

fn tool_result(tool_use_id: &str, result: &ToolResult) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": result.text,
        "is_error": result.is_error,
    })
}

/// After a cancelled turn the last assistant message may hold `tool_use` blocks without results,
/// which the API rejects. Answers each of them with an error result.
fn close_pending_tool_uses(messages: &mut Vec<Value>) {
    let Some(last) = messages.last() else {
        return;
    };
    if last["role"] != "assistant" {
        return;
    }
    let cancelled = ToolResult {
        text: CANCELLED.to_string(),
        is_error: true,
    };
    let results: Vec<Value> = last["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "tool_use")
        .filter_map(|block| block["id"].as_str())
        .map(|id| tool_result(id, &cancelled))
        .collect();
    if !results.is_empty() {
        messages.push(json!({ "role": "user", "content": results }));
    }
}

struct Session {
    root: PathBuf,
    cfg: NativeSettings,
    system: String,
    messages: Vec<Value>,
    /// Tool names the user chose "Always Allow" for in this thread.
    always_allowed: HashSet<String>,
    events: EventTx,
}

impl Session {
    fn new(root: PathBuf, cfg: NativeSettings, events: EventTx) -> Self {
        Self {
            system: system_prompt(&root),
            root,
            cfg,
            messages: Vec::new(),
            always_allowed: HashSet::new(),
            events,
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.try_send(event);
    }

    /// Requests until the model stops asking for tools.
    async fn turn(&mut self, api_key: &str, cancel: Arc<AtomicBool>) -> TurnEnd {
        loop {
            let body =
                api::request_body(&self.cfg, &self.system, &tools::schemas(), &self.messages);
            let stream = api::stream(
                self.cfg.base_url.clone(),
                api_key.to_string(),
                body,
                cancel.clone(),
            );
            let mut done = None;
            while let Ok(event) = stream.recv().await {
                match event {
                    StreamEvent::TextDelta(text) => self.emit(AgentEvent::MessageChunk(text)),
                    StreamEvent::ThinkingDelta(text) => self.emit(AgentEvent::ThoughtChunk(text)),
                    StreamEvent::ToolUseStart { .. } | StreamEvent::BlockStop => {}
                    StreamEvent::MessageDone {
                        stop_reason,
                        content,
                    } => {
                        done = Some((stop_reason, content));
                        break;
                    }
                    StreamEvent::Error(message) => {
                        self.emit(AgentEvent::Error(message));
                        return TurnEnd::Done;
                    }
                }
            }
            let Some((stop_reason, content)) = done else {
                self.emit(AgentEvent::Error(
                    "네트워크 오류: 응답이 완료되기 전에 끊어졌습니다".to_string(),
                ));
                return TurnEnd::Done;
            };
            // Thinking blocks and their signatures go back exactly as received.
            if !content.is_empty() {
                self.messages
                    .push(json!({ "role": "assistant", "content": content }));
            }
            if let Some(end) = turn_end(&stop_reason) {
                return end;
            }
            if stop_reason == "tool_use" {
                let results = self.run_tools(&content).await;
                self.messages
                    .push(json!({ "role": "user", "content": results }));
            }
        }
    }

    async fn run_tools(&mut self, content: &[Value]) -> Vec<Value> {
        let mut results = Vec::new();
        for block in content.iter().filter(|block| block["type"] == "tool_use") {
            let id = block["id"].as_str().unwrap_or_default().to_string();
            let name = block["name"].as_str().unwrap_or_default();
            let input = &block["input"];
            let preview = tools::preview(&self.root, name, input);
            self.emit(AgentEvent::ToolCall {
                id: id.clone(),
                title: preview.title,
                kind: preview.kind,
                status: ToolStatus::Pending,
                content: preview.content.clone(),
                locations: preview.locations,
            });

            let result = if self.approve(&id, name).await {
                self.emit(AgentEvent::ToolCallUpdate {
                    id: id.clone(),
                    title: None,
                    status: Some(ToolStatus::InProgress),
                    content: None,
                    locations: None,
                });
                tools::execute(&self.root, name, input).await
            } else {
                ToolResult {
                    text: REJECTED.to_string(),
                    is_error: true,
                }
            };

            // Edit tools keep their diff above the result text.
            let mut shown = if preview.kind == ToolKind::Edit {
                preview.content
            } else {
                Vec::new()
            };
            shown.push(ToolContent::Text(result.text.clone()));
            self.emit(AgentEvent::ToolCallUpdate {
                id: id.clone(),
                title: None,
                status: Some(if result.is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                }),
                content: Some(shown),
                locations: None,
            });
            results.push(tool_result(&id, &result));
        }
        results
    }

    /// Whether the tool may run: read-only tools always, others after the user allows them.
    async fn approve(&mut self, tool_call_id: &str, name: &str) -> bool {
        if !tools::requires_approval(name) || self.always_allowed.contains(name) {
            return true;
        }
        let (reply, answer) = oneshot::channel();
        let option = |id: &str, label: &str, kind| PermissionOption {
            id: id.to_string(),
            label: label.to_string(),
            kind,
        };
        self.emit(AgentEvent::Permission {
            tool_call_id: tool_call_id.to_string(),
            options: vec![
                option("allow", "Allow", PermissionKind::AllowOnce),
                option("allow_always", "Always Allow", PermissionKind::AllowAlways),
                option("reject", "Reject", PermissionKind::RejectOnce),
            ],
            reply,
        });
        match answer.await.ok().flatten().as_deref() {
            Some("allow") => true,
            Some("allow_always") => {
                self.always_allowed.insert(name.to_string());
                true
            }
            _ => false,
        }
    }
}

async fn run(mut session: Session, api_key: Option<String>, commands: CommandRx) {
    while let Ok(command) = commands.recv().await {
        let BackendCommand::Prompt(text) = command else {
            // Nothing is running, and the native backend has no modes.
            continue;
        };
        let Some(api_key) = api_key.as_deref() else {
            session.emit(AgentEvent::Error(MISSING_KEY.to_string()));
            session.emit(AgentEvent::TurnEnded(TurnEnd::Done));
            continue;
        };
        session.messages.push(json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        }));

        let cancel = Arc::new(AtomicBool::new(false));
        let finished = {
            let turn = session.turn(api_key, cancel.clone());
            futures::pin_mut!(turn);
            loop {
                match select(turn.as_mut(), Box::pin(commands.recv())).await {
                    Either::Left((end, _)) => break Some(end),
                    Either::Right((Ok(BackendCommand::Cancel), _)) => break None,
                    Either::Right((Ok(_), _)) => {}
                    // The panel is gone.
                    Either::Right((Err(_), _)) => return,
                }
            }
        };
        let end = finished.unwrap_or_else(|| {
            // Dropping the turn dropped the response channel and any running tool process.
            cancel.store(true, Ordering::Relaxed);
            close_pending_tool_uses(&mut session.messages);
            TurnEnd::Cancelled
        });
        session.emit(AgentEvent::TurnEnded(end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_map_to_turn_ends() {
        assert_eq!(turn_end("tool_use"), None);
        assert_eq!(turn_end("pause_turn"), None);
        assert_eq!(turn_end("end_turn"), Some(TurnEnd::Done));
        assert_eq!(turn_end("stop_sequence"), Some(TurnEnd::Done));
        assert_eq!(turn_end("max_tokens"), Some(TurnEnd::MaxTokens));
        assert_eq!(turn_end("refusal"), Some(TurnEnd::Refusal));
        assert_eq!(turn_end("something_new"), Some(TurnEnd::Done));
    }

    #[test]
    fn cancelled_turn_answers_pending_tool_uses() {
        let mut messages = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
            json!({ "role": "assistant", "content": [
                { "type": "thinking", "thinking": "…", "signature": "sig" },
                { "type": "tool_use", "id": "a", "name": "read_file", "input": {} },
                { "type": "tool_use", "id": "b", "name": "run_command", "input": {} },
            ] }),
        ];
        close_pending_tool_uses(&mut messages);
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[2],
            json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "a", "content": CANCELLED, "is_error": true },
                { "type": "tool_result", "tool_use_id": "b", "content": CANCELLED, "is_error": true },
            ] })
        );
    }

    #[test]
    fn cancelled_turn_without_tool_uses_adds_nothing() {
        let mut after_text = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "hello" }] }),
        ];
        let mut mid_stream = after_text[..1].to_vec();
        close_pending_tool_uses(&mut after_text);
        close_pending_tool_uses(&mut mid_stream);
        assert_eq!(after_text.len(), 2);
        assert_eq!(mid_stream.len(), 1);
    }
}
