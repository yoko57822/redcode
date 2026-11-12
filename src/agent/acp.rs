//! ACP backend: runs an Agent Client Protocol agent as a child process over stdio.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ClientCapabilities, ContentBlock, Implementation, InitializeRequest,
    NewSessionRequest, PermissionOption as AcpPermissionOption, PermissionOptionKind,
    PlanEntryStatus, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionId, SessionNotification,
    SessionUpdate, SetSessionModeRequest, StopReason, TextContent, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind as AcpToolKind,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client};
use futures::channel::oneshot;
use futures::future::{Either, select};
use gpui::{App, AppContext as _};

use super::thread::{
    AgentEvent, BackendCommand, BackendHandle, Location, PermissionKind, PermissionOption,
    ToolContent, ToolKind, ToolStatus, TurnEnd,
};
use crate::settings::AcpSettings;

type EventTx = smol::channel::Sender<AgentEvent>;
type CommandRx = smol::channel::Receiver<BackendCommand>;

pub fn spawn(root: PathBuf, cfg: AcpSettings, cx: &mut App) -> BackendHandle {
    let (commands, cmd_rx) = smol::channel::unbounded();
    let (ev_tx, events) = smol::channel::unbounded();
    let task = cx.background_spawn(async move {
        if let Err(err) = run(root, cfg, cmd_rx, ev_tx.clone()).await {
            let _ = ev_tx.send(AgentEvent::Fatal(format!("{err}"))).await;
        }
    });
    BackendHandle {
        commands,
        events,
        _task: task,
    }
}

/// The badge label: the last whitespace-separated token of the command.
fn label(command: &str) -> String {
    command
        .split_whitespace()
        .last()
        .unwrap_or(command)
        .to_string()
}

/// Connects to the agent and serves commands until the command channel closes.
pub(crate) async fn run(
    root: PathBuf,
    cfg: AcpSettings,
    cmd_rx: CommandRx,
    ev_tx: EventTx,
) -> anyhow::Result<()> {
    let label = label(&cfg.command);
    // Through a login shell so `npx` resolves on PATH even when launched from Finder.
    let config = AcpAgentConfig::new("/bin/zsh")
        .args(["-lc".to_string(), format!("exec {}", cfg.command)])
        .envs(cfg.env);
    let started = Arc::new(AtomicBool::new(false));
    let notify_tx = ev_tx.clone();
    let permission_tx = ev_tx.clone();
    let session_started = started.clone();

    let result = Client
        .v1()
        .name("redcode")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let Some(event) = map_update(notification.update) {
                    let _ = notify_tx.try_send(event);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, cx: ConnectionTo<Agent>| {
                let (reply, answer) = oneshot::channel();
                let options = request.options.iter().map(map_permission_option).collect();
                let _ = permission_tx.try_send(AgentEvent::Permission {
                    tool_call_id: request.tool_call.tool_call_id.to_string(),
                    options,
                    reply,
                });
                // Waiting for the user inside the handler would block the dispatch loop.
                cx.spawn(async move {
                    let outcome = match answer.await {
                        Ok(Some(id)) => {
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                        }
                        _ => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            AcpAgent::new(config),
            async move |cx: ConnectionTo<Agent>| {
                serve(cx, root, label, session_started, cmd_rx, ev_tx).await
            },
        )
        .await;

    result.map_err(|err| {
        if started.load(Ordering::SeqCst) {
            anyhow::anyhow!("{err}")
        } else {
            anyhow::anyhow!(
                "에이전트를 시작할 수 없습니다: {err}. 터미널에서 해당 에이전트 CLI에 로그인되어 있는지 확인하세요."
            )
        }
    })
}

async fn serve(
    cx: ConnectionTo<Agent>,
    root: PathBuf,
    label: String,
    started: Arc<AtomicBool>,
    cmd_rx: CommandRx,
    ev_tx: EventTx,
) -> Result<(), agent_client_protocol::Error> {
    let initialize = InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(ClientCapabilities::new())
        .client_info(Implementation::new("redcode", env!("CARGO_PKG_VERSION")));
    cx.send_request(initialize).block_task().await?;
    let session = cx
        .send_request(NewSessionRequest::new(root))
        .block_task()
        .await?;
    let session_id = session.session_id;
    let (modes, current_mode) = match session.modes {
        Some(state) => (
            state
                .available_modes
                .into_iter()
                .map(|mode| (mode.id.to_string(), mode.name))
                .collect(),
            Some(state.current_mode_id.to_string()),
        ),
        None => (Vec::new(), None),
    };
    started.store(true, Ordering::SeqCst);
    let _ = ev_tx.try_send(AgentEvent::Ready {
        label,
        modes,
        current_mode,
    });

    // A closed command channel means the panel is gone: returning drops the connection,
    // which kills the agent's process group.
    while let Ok(command) = cmd_rx.recv().await {
        match command {
            BackendCommand::Prompt(text) => {
                let request = PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(text))],
                );
                let mut prompt = Box::pin(cx.send_request(request).block_task());
                let response = loop {
                    match select(prompt, Box::pin(cmd_rx.recv())).await {
                        Either::Left((response, _)) => break response,
                        Either::Right((command, pending)) => {
                            prompt = pending;
                            match command {
                                // The agent answers the pending prompt with `Cancelled`.
                                Ok(BackendCommand::Cancel) => {
                                    cx.send_notification(CancelNotification::new(
                                        session_id.clone(),
                                    ))?;
                                }
                                Ok(BackendCommand::SetMode(mode)) => {
                                    set_mode(&cx, &session_id, mode, &ev_tx)?
                                }
                                Ok(BackendCommand::Prompt(_)) => {}
                                Err(_) => return Ok(()),
                            }
                        }
                    }
                };
                match response {
                    Ok(response) => {
                        let _ = ev_tx
                            .try_send(AgentEvent::TurnEnded(map_stop_reason(response.stop_reason)));
                    }
                    Err(err) => {
                        let _ = ev_tx.try_send(AgentEvent::Error(err.to_string()));
                        let _ = ev_tx.try_send(AgentEvent::TurnEnded(TurnEnd::Done));
                    }
                }
            }
            BackendCommand::SetMode(mode) => set_mode(&cx, &session_id, mode, &ev_tx)?,
            BackendCommand::Cancel => {}
        }
    }
    Ok(())
}

/// Switches the session mode without blocking the command loop.
fn set_mode(
    cx: &ConnectionTo<Agent>,
    session_id: &SessionId,
    mode: String,
    ev_tx: &EventTx,
) -> Result<(), agent_client_protocol::Error> {
    let request = cx
        .send_request(SetSessionModeRequest::new(session_id.clone(), mode.clone()))
        .block_task();
    let ev_tx = ev_tx.clone();
    cx.spawn(async move {
        let event = match request.await {
            Ok(_) => AgentEvent::ModeChanged(mode),
            Err(err) => AgentEvent::Error(err.to_string()),
        };
        let _ = ev_tx.try_send(event);
        Ok(())
    })
}

fn map_stop_reason(reason: StopReason) -> TurnEnd {
    match reason {
        StopReason::MaxTokens => TurnEnd::MaxTokens,
        StopReason::MaxTurnRequests => TurnEnd::MaxTurns,
        StopReason::Refusal => TurnEnd::Refusal,
        StopReason::Cancelled => TurnEnd::Cancelled,
        _ => TurnEnd::Done,
    }
}

fn map_permission_option(option: &AcpPermissionOption) -> PermissionOption {
    let kind = match option.kind {
        PermissionOptionKind::AllowOnce => PermissionKind::AllowOnce,
        PermissionOptionKind::AllowAlways => PermissionKind::AllowAlways,
        PermissionOptionKind::RejectAlways => PermissionKind::RejectAlways,
        _ => PermissionKind::RejectOnce,
    };
    PermissionOption {
        id: option.option_id.to_string(),
        label: option.name.clone(),
        kind,
    }
}

fn map_kind(kind: AcpToolKind) -> ToolKind {
    match kind {
        AcpToolKind::Read => ToolKind::Read,
        AcpToolKind::Edit => ToolKind::Edit,
        AcpToolKind::Delete => ToolKind::Delete,
        AcpToolKind::Move => ToolKind::Move,
        AcpToolKind::Search => ToolKind::Search,
        AcpToolKind::Execute => ToolKind::Execute,
        AcpToolKind::Think => ToolKind::Think,
        AcpToolKind::Fetch => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn map_status(status: ToolCallStatus) -> ToolStatus {
    match status {
        ToolCallStatus::InProgress => ToolStatus::InProgress,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

/// Text content and diffs; terminals and non-text blocks are dropped.
fn map_content(content: Vec<ToolCallContent>) -> Vec<ToolContent> {
    content
        .into_iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(content) => match content.content {
                ContentBlock::Text(text) => Some(ToolContent::Text(text.text)),
                _ => None,
            },
            ToolCallContent::Diff(diff) => Some(ToolContent::Diff {
                path: diff.path,
                old: diff.old_text,
                new: diff.new_text,
            }),
            _ => None,
        })
        .collect()
}

fn map_locations(locations: Vec<ToolCallLocation>) -> Vec<Location> {
    locations
        .into_iter()
        .map(|location| (location.path, location.line))
        .collect()
}

fn plan_status(status: &PlanEntryStatus) -> &'static str {
    match status {
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "pending",
    }
}

fn text_of(content: ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text),
        _ => None,
    }
}

pub(crate) fn map_update(update: SessionUpdate) -> Option<AgentEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            text_of(chunk.content).map(AgentEvent::MessageChunk)
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            text_of(chunk.content).map(AgentEvent::ThoughtChunk)
        }
        SessionUpdate::ToolCall(call) => Some(AgentEvent::ToolCall {
            id: call.tool_call_id.to_string(),
            title: call.title,
            kind: map_kind(call.kind),
            status: map_status(call.status),
            content: map_content(call.content),
            locations: map_locations(call.locations),
        }),
        SessionUpdate::ToolCallUpdate(update) => {
            let fields = update.fields;
            Some(AgentEvent::ToolCallUpdate {
                id: update.tool_call_id.to_string(),
                title: fields.title,
                status: fields.status.map(map_status),
                content: fields.content.map(map_content),
                locations: fields.locations.map(map_locations),
            })
        }
        SessionUpdate::Plan(plan) => Some(AgentEvent::Plan(
            plan.entries
                .into_iter()
                .map(|entry| {
                    let status = plan_status(&entry.status).to_string();
                    (entry.content, status)
                })
                .collect(),
        )),
        SessionUpdate::CurrentModeUpdate(update) => {
            Some(AgentEvent::ModeChanged(update.current_mode_id.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_client_protocol::v1::schema::{
        Content, ContentChunk, CurrentModeUpdate, Diff, Plan, PlanEntry, PlanEntryPriority,
        ToolCall, ToolCallUpdate, ToolCallUpdateFields,
    };

    use super::*;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    #[test]
    fn message_and_thought_chunks() {
        let message = map_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            text_block("hi"),
        )));
        assert!(matches!(message, Some(AgentEvent::MessageChunk(text)) if text == "hi"));
        let thought = map_update(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            text_block("hmm"),
        )));
        assert!(matches!(thought, Some(AgentEvent::ThoughtChunk(text)) if text == "hmm"));
        let user = map_update(SessionUpdate::UserMessageChunk(ContentChunk::new(
            text_block("me"),
        )));
        assert!(user.is_none());
    }

    #[test]
    fn non_text_chunks_are_ignored() {
        let update: SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "image", "data": "", "mimeType": "image/png" }
        }))
        .unwrap();
        assert!(map_update(update).is_none());
    }

    #[test]
    fn tool_call_maps_kind_content_and_locations() {
        let call = ToolCall::new("call-1", "Edit a.txt")
            .kind(AcpToolKind::Edit)
            .status(ToolCallStatus::InProgress)
            .content(vec![
                ToolCallContent::Content(Content::new(text_block("note"))),
                ToolCallContent::Diff(Diff::new("/w/a.txt", "new\n").old_text("old\n")),
            ])
            .locations(vec![ToolCallLocation::new("/w/a.txt").line(3)]);
        let Some(AgentEvent::ToolCall {
            id,
            title,
            kind,
            status,
            content,
            locations,
        }) = map_update(SessionUpdate::ToolCall(call))
        else {
            panic!("expected ToolCall");
        };
        assert_eq!(id, "call-1");
        assert_eq!(title, "Edit a.txt");
        assert_eq!(kind, ToolKind::Edit);
        assert_eq!(status, ToolStatus::InProgress);
        assert_eq!(
            content,
            vec![
                ToolContent::Text("note".into()),
                ToolContent::Diff {
                    path: "/w/a.txt".into(),
                    old: Some("old\n".into()),
                    new: "new\n".into()
                },
            ]
        );
        assert_eq!(locations, vec![(PathBuf::from("/w/a.txt"), Some(3))]);

        let switch = ToolCall::new("call-2", "Plan mode").kind(AcpToolKind::SwitchMode);
        assert!(matches!(
            map_update(SessionUpdate::ToolCall(switch)),
            Some(AgentEvent::ToolCall {
                kind: ToolKind::Other,
                status: ToolStatus::Pending,
                ..
            })
        ));
    }

    #[test]
    fn tool_call_update_keeps_only_present_fields() {
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        let Some(AgentEvent::ToolCallUpdate {
            id,
            title,
            status,
            content,
            locations,
        }) = map_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "call-1", fields,
        )))
        else {
            panic!("expected ToolCallUpdate");
        };
        assert_eq!(id, "call-1");
        assert_eq!(title, None);
        assert_eq!(status, Some(ToolStatus::Completed));
        assert_eq!(content, None);
        assert_eq!(locations, None);
    }

    #[test]
    fn plan_and_mode_updates() {
        let plan = Plan::new(vec![
            PlanEntry::new("read", PlanEntryPriority::High, PlanEntryStatus::Completed),
            PlanEntry::new("edit", PlanEntryPriority::Low, PlanEntryStatus::InProgress),
            PlanEntry::new("test", PlanEntryPriority::Medium, PlanEntryStatus::Pending),
        ]);
        let Some(AgentEvent::Plan(entries)) = map_update(SessionUpdate::Plan(plan)) else {
            panic!("expected Plan");
        };
        assert_eq!(
            entries,
            vec![
                ("read".to_string(), "completed".to_string()),
                ("edit".to_string(), "in_progress".to_string()),
                ("test".to_string(), "pending".to_string()),
            ]
        );
        let mode = map_update(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            "plan",
        )));
        assert!(matches!(mode, Some(AgentEvent::ModeChanged(id)) if id == "plan"));
    }

    #[test]
    fn stop_reasons_and_permission_kinds() {
        assert_eq!(map_stop_reason(StopReason::EndTurn), TurnEnd::Done);
        assert_eq!(map_stop_reason(StopReason::MaxTokens), TurnEnd::MaxTokens);
        assert_eq!(
            map_stop_reason(StopReason::MaxTurnRequests),
            TurnEnd::MaxTurns
        );
        assert_eq!(map_stop_reason(StopReason::Refusal), TurnEnd::Refusal);
        assert_eq!(map_stop_reason(StopReason::Cancelled), TurnEnd::Cancelled);
        let option = AcpPermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce);
        assert_eq!(
            map_permission_option(&option),
            PermissionOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: PermissionKind::AllowOnce
            }
        );
        assert_eq!(
            label("npx -y @agentclientprotocol/claude-agent-acp@0.106.0"),
            "@agentclientprotocol/claude-agent-acp@0.106.0"
        );
    }

    /// Needs network and `npx`: `cargo test -- --ignored acp_smoke_ready`.
    #[test]
    #[ignore]
    fn acp_smoke_ready() {
        let dir = std::env::temp_dir().join(format!("redcode-acp-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = AcpSettings::default();
        let (cmd_tx, cmd_rx) = smol::channel::unbounded();
        let (ev_tx, ev_rx) = smol::channel::unbounded();
        let task = smol::spawn(run(dir.clone(), cfg, cmd_rx, ev_tx));
        let ready = smol::block_on(async {
            let wait = async {
                loop {
                    match ev_rx.recv().await {
                        Ok(AgentEvent::Ready {
                            label,
                            modes,
                            current_mode,
                        }) => {
                            break Ok(format!(
                                "Ready label={label} modes={modes:?} current={current_mode:?}"
                            ));
                        }
                        Ok(AgentEvent::Fatal(err)) => break Err(format!("Fatal: {err}")),
                        Ok(other) => eprintln!("event before Ready: {other:?}"),
                        Err(_) => break Err("event channel closed".to_string()),
                    }
                }
            };
            let timeout = async {
                smol::Timer::after(Duration::from_secs(90)).await;
                Err("timed out after 90s".to_string())
            };
            smol::future::or(wait, timeout).await
        });
        drop(cmd_tx);
        let finished = smol::block_on(smol::future::or(
            async {
                task.await.map_err(|err| err.to_string())?;
                Ok::<_, String>("connection closed cleanly")
            },
            async {
                smol::Timer::after(Duration::from_secs(10)).await;
                Err("shutdown timed out".to_string())
            },
        ));
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("acp smoke: {ready:?}; shutdown: {finished:?}");
        assert!(ready.is_ok(), "{ready:?}");
    }
}
