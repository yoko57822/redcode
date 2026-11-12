//! Anthropic Messages API streaming client: SSE parsing, content block assembly, and the
//! blocking `ureq` request thread.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::settings::NativeSettings;

const API_VERSION: &str = "2026-10-21";
const READ_BUFFER: usize = 8 * 1024;
const ERROR_BODY_LIMIT: u64 = 64 * 1024;

/// Incremental Server-Sent Events parser. Lines may be split across chunks arbitrarily.
#[derive(Default)]
pub struct SseParser {
    /// Bytes of the current, not yet terminated line.
    line: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds a chunk and returns every event completed by it as `(event, data)`.
    /// Events without an `event:` field are reported as `"message"`.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..pos]);
            rest = &rest[pos + 1..];
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = String::from_utf8_lossy(&self.line).into_owned();
            self.line.clear();
            self.process_line(&line, &mut out);
        }
        self.line.extend_from_slice(rest);
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<(String, String)>) {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            if !self.data.is_empty() {
                let data = std::mem::take(&mut self.data).join("\n");
                let event = if event.is_empty() {
                    "message".to_string()
                } else {
                    event
                };
                out.push((event, data));
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    BlockStop,
    /// `content` holds the assembled content blocks, valid to send back verbatim.
    MessageDone {
        stop_reason: String,
        content: Vec<Value>,
    },
    Error(String),
}

struct Block {
    value: Map<String, Value>,
    partial_json: String,
}

/// Assembles Messages API stream events into deltas and the final content blocks.
#[derive(Default)]
pub struct Assembler {
    blocks: BTreeMap<u64, Block>,
    stop_reason: Option<String>,
}

impl Assembler {
    pub fn on_event(&mut self, event: &str, data: &str) -> Vec<StreamEvent> {
        let Ok(data) = serde_json::from_str::<Value>(data) else {
            if event == "error" {
                return vec![StreamEvent::Error(data.trim().to_string())];
            }
            return Vec::new();
        };
        let kind = if event.is_empty() || event == "message" {
            data.get("type").and_then(Value::as_str).unwrap_or("")
        } else {
            event
        };
        match kind {
            "message_start" => {
                self.blocks.clear();
                self.stop_reason = None;
                Vec::new()
            }
            "content_block_start" => self.block_start(&data),
            "content_block_delta" => self.block_delta(&data),
            "content_block_stop" => self.block_stop(&data),
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                Vec::new()
            }
            "message_stop" => {
                let content = std::mem::take(&mut self.blocks)
                    .into_values()
                    .map(|block| Value::Object(block.value))
                    .collect();
                vec![StreamEvent::MessageDone {
                    stop_reason: self.stop_reason.take().unwrap_or_default(),
                    content,
                }]
            }
            "error" => {
                let ty = data
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let message = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                vec![StreamEvent::Error(format!("{ty}: {message}"))]
            }
            _ => Vec::new(),
        }
    }

    fn block_start(&mut self, data: &Value) -> Vec<StreamEvent> {
        let Some(index) = data.get("index").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(Value::Object(value)) = data.get("content_block").cloned() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if value.get("type").and_then(Value::as_str) == Some("tool_use") {
            let field = |key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            events.push(StreamEvent::ToolUseStart {
                id: field("id"),
                name: field("name"),
            });
        }
        self.blocks.insert(
            index,
            Block {
                value,
                partial_json: String::new(),
            },
        );
        events
    }

    fn block_delta(&mut self, data: &Value) -> Vec<StreamEvent> {
        let Some(index) = data.get("index").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(block) = self.blocks.get_mut(&index) else {
            return Vec::new();
        };
        let Some(delta) = data.get("delta") else {
            return Vec::new();
        };
        let text = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or("");
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => {
                let chunk = text("text");
                append_field(&mut block.value, "text", chunk);
                vec![StreamEvent::TextDelta(chunk.to_string())]
            }
            "thinking_delta" => {
                let chunk = text("thinking");
                append_field(&mut block.value, "thinking", chunk);
                vec![StreamEvent::ThinkingDelta(chunk.to_string())]
            }
            "signature_delta" => {
                append_field(&mut block.value, "signature", text("signature"));
                Vec::new()
            }
            "input_json_delta" => {
                block.partial_json.push_str(text("partial_json"));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn block_stop(&mut self, data: &Value) -> Vec<StreamEvent> {
        let Some(index) = data.get("index").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(block) = self.blocks.get_mut(&index) else {
            return Vec::new();
        };
        let is_tool = block.value.get("type").and_then(Value::as_str) == Some("tool_use");
        if !block.partial_json.is_empty() {
            match serde_json::from_str::<Value>(&block.partial_json) {
                Ok(input) => {
                    block.value.insert("input".to_string(), input);
                }
                Err(err) => {
                    return vec![StreamEvent::Error(format!(
                        "invalid tool input JSON: {err}"
                    ))];
                }
            }
            block.partial_json.clear();
        } else if is_tool && !block.value.get("input").is_some_and(Value::is_object) {
            block.value.insert("input".to_string(), json!({}));
        }
        vec![StreamEvent::BlockStop]
    }
}

fn append_field(block: &mut Map<String, Value>, key: &str, chunk: &str) {
    match block.get_mut(key) {
        Some(Value::String(existing)) => existing.push_str(chunk),
        _ => {
            block.insert(key.to_string(), Value::String(chunk.to_string()));
        }
    }
}

/// Builds the streaming `POST /v1/messages` request body.
pub fn request_body(
    cfg: &NativeSettings,
    system: &str,
    tools: &Value,
    messages: &[Value],
) -> Value {
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "stream": true,
        "system": system,
        "tools": tools,
        "messages": messages,
    });
    if cfg.thinking == "adaptive" {
        body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
    }
    if !cfg.effort.is_empty() {
        body["output_config"] = json!({ "effort": cfg.effort });
    }
    body
}

/// Sends the request on a dedicated thread and streams assembled events back. The thread ends
/// when `cancel` is set, the receiver is dropped, or after `MessageDone`/`Error`.
pub fn stream(
    base_url: String,
    api_key: String,
    body: Value,
    cancel: Arc<AtomicBool>,
) -> smol::channel::Receiver<StreamEvent> {
    let (tx, rx) = smol::channel::unbounded::<StreamEvent>();
    std::thread::spawn(move || run_stream(&base_url, &api_key, &body, &cancel, &tx));
    rx
}

fn run_stream(
    base_url: &str,
    api_key: &str,
    body: &Value,
    cancel: &AtomicBool,
    tx: &smol::channel::Sender<StreamEvent>,
) {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_per_read(Some(Duration::from_secs(120)))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let result = agent
        .post(url.as_str())
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
        .header("content-type", "application/json")
        .send_json(body);
    let mut response = match result {
        Ok(response) => response,
        Err(err) => {
            let _ = tx.try_send(StreamEvent::Error(format!("네트워크 오류: {err}")));
            return;
        }
    };

    let status = response.status().as_u16();
    if status != 200 {
        let mut raw = Vec::new();
        let read = response
            .body_mut()
            .as_reader()
            .take(ERROR_BODY_LIMIT)
            .read_to_end(&mut raw);
        if let Err(err) = read {
            let _ = tx.try_send(StreamEvent::Error(format!("네트워크 오류: {err}")));
            return;
        }
        let _ = tx.try_send(StreamEvent::Error(http_error_message(status, &raw)));
        return;
    }

    let mut reader = response.body_mut().as_reader();
    let mut buf = [0u8; READ_BUFFER];
    let mut parser = SseParser::default();
    let mut assembler = Assembler::default();
    loop {
        if cancel.load(Ordering::Relaxed) || tx.is_closed() {
            return;
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = tx.try_send(StreamEvent::Error(format!("네트워크 오류: {err}")));
                return;
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        for (event, data) in parser.feed(&buf[..n]) {
            for ev in assembler.on_event(&event, &data) {
                let last = matches!(ev, StreamEvent::MessageDone { .. } | StreamEvent::Error(_));
                if tx.try_send(ev).is_err() || last {
                    return;
                }
            }
        }
    }
    let _ = tx.try_send(StreamEvent::Error(
        "네트워크 오류: 응답 스트림이 message_stop 전에 끝났습니다".to_string(),
    ));
}

fn http_error_message(status: u16, raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        let ty = value.pointer("/error/type").and_then(Value::as_str);
        let message = value.pointer("/error/message").and_then(Value::as_str);
        if let (Some(ty), Some(message)) = (ty, message) {
            return format!("{status} {ty}: {message}");
        }
    }
    let text = text.trim();
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{status} {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(events: &[(&str, &str)]) -> Vec<StreamEvent> {
        let mut assembler = Assembler::default();
        events
            .iter()
            .flat_map(|(event, data)| assembler.on_event(event, data))
            .collect()
    }

    #[test]
    fn sse_split_anywhere_is_equivalent() {
        let stream: &[u8] = "event: content_block_delta\r\ndata: {\"a\":\"한글\"}\r\n\r\n: comment\nevent: ping\ndata: one\ndata: two\n\n"
            .as_bytes();
        let mut whole = SseParser::default();
        let expected = whole.feed(stream);
        assert_eq!(
            expected,
            vec![
                (
                    "content_block_delta".to_string(),
                    "{\"a\":\"한글\"}".to_string()
                ),
                ("ping".to_string(), "one\ntwo".to_string()),
            ]
        );
        for split in 0..=stream.len() {
            let mut parser = SseParser::default();
            let mut got = parser.feed(&stream[..split]);
            got.extend(parser.feed(&stream[split..]));
            assert_eq!(got, expected, "split at {split}");
        }
    }

    #[test]
    fn text_deltas_and_message_done() {
        let events = run(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"m","content":[]}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hel".into()),
                StreamEvent::TextDelta("lo".into()),
                StreamEvent::BlockStop,
                StreamEvent::MessageDone {
                    stop_reason: "end_turn".into(),
                    content: vec![json!({"type":"text","text":"Hello"})],
                },
            ]
        );
    }

    #[test]
    fn tool_use_input_is_assembled() {
        let events = run(&[
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"sr"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"c/main.rs\", \"limit\": 5}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: "toolu_1".into(),
                    name: "read_file".into()
                },
                StreamEvent::BlockStop,
                StreamEvent::MessageDone {
                    stop_reason: "tool_use".into(),
                    content: vec![json!({
                        "type": "tool_use", "id": "toolu_1", "name": "read_file",
                        "input": {"path": "src/main.rs", "limit": 5}
                    })],
                },
            ]
        );
    }

    #[test]
    fn tool_use_without_input_gets_empty_object() {
        let events = run(&[
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"list_files"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let Some(StreamEvent::MessageDone { content, .. }) = events.last() else {
            panic!("missing MessageDone");
        };
        assert_eq!(content[0]["input"], json!({}));
    }

    #[test]
    fn thinking_signature_is_preserved() {
        let events = run(&[
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        assert_eq!(events[0], StreamEvent::ThinkingDelta("Let me think".into()));
        let Some(StreamEvent::MessageDone { content, .. }) = events.last() else {
            panic!("missing MessageDone");
        };
        assert_eq!(
            content,
            &vec![
                json!({"type":"thinking","thinking":"Let me think","signature":"EqQBCgIYAhIM"}),
                json!({"type":"redacted_thinking","data":"opaque"}),
            ]
        );
    }

    #[test]
    fn ping_and_unknown_events_are_ignored() {
        let events = run(&[
            ("ping", r#"{"type":"ping"}"#),
            ("future_event", r#"{"type":"future_event","x":1}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"future_delta"}}"#,
            ),
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn error_event_and_http_error() {
        let events = run(&[(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )]);
        assert_eq!(
            events,
            vec![StreamEvent::Error("overloaded_error: Overloaded".into())]
        );
        assert_eq!(
            http_error_message(401, br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
            "401 authentication_error: invalid x-api-key"
        );
        assert_eq!(http_error_message(502, b"Bad Gateway\n"), "502 Bad Gateway");
    }
}
