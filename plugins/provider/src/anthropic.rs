//! Native Anthropic Messages API adapter. Converts the neutral `Message`
//! history at the boundary: system messages become the `system` parameter,
//! tool results merge into single user messages, and this API's own thinking
//! blocks are replayed unchanged (it requires that on thinking-enabled
//! models); another vendor's reasoning items are not blocks it accepts and
//! are skipped.

use std::ops::ControlFlow;

use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::{Error, Result};
use elmee_core::message::{FunctionCall, Message, Role, ToolCall};

use super::{AssistantTurn, Chunk, Provider};
use crate::OrHttp;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: usize = 32_000;

pub struct Anthropic {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    /// Cap on thinking + response text per turn. Derived from the model's
    /// window so a small model is not asked for a reply that cannot fit
    /// beside its own context, and shared with the compaction threshold.
    max_tokens: usize,
}

impl Anthropic {
    pub fn new(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self { client: super::http_client(), base_url, api_key, model, max_tokens: DEFAULT_MAX_TOKENS }
    }

    /// Size the reply budget to the model's context window; an unknown window
    /// falls back to the constructor default.
    pub fn set_context_window(&mut self, tokens: Option<usize>) {
        self.max_tokens = tokens.map_or(DEFAULT_MAX_TOKENS, elmee_core::output_budget);
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    async fn list_models_authorized(&self) -> Result<Vec<crate::Model>> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;
        let mut cursors = HashSet::new();

        loop {
            let mut request = self
                .client
                .get(format!("{}/v1/models", self.base_url))
                .header("anthropic-version", ANTHROPIC_VERSION)
                .query(&[("limit", "1000")]);
            if let Some(cursor) = &after_id {
                request = request.query(&[("after_id", cursor)]);
            }
            let response = self.authorize(request).send().await.or_http()?;
            let status = response.status();
            let headers = response.headers().clone();
            let text = response.text().await.or_http()?;
            if status != StatusCode::OK {
                return Err(super::refused(status, &headers, &text));
            }
            let body: Value = serde_json::from_str(&text)?;
            for model in super::model_ids(&body) {
                if !models.iter().any(|known: &crate::Model| known.id == model.id) {
                    models.push(model);
                }
            }

            if !body.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(models);
            }
            let next =
                body.get("last_id").and_then(Value::as_str).filter(|id| !id.is_empty()).ok_or_else(|| {
                    Error::Provider("Anthropic model catalogue has more pages but no last_id cursor".into())
                })?;
            if !cursors.insert(next.to_owned()) {
                return Err(Error::Provider(
                    "Anthropic model catalogue repeated its pagination cursor".into(),
                ));
            }
            after_id = Some(next.to_owned());
        }
    }
}

#[elmee_core::async_trait(?Send)]
impl Provider for Anthropic {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        let body = request_body(&self.model, messages, tools, self.max_tokens)?;
        let request = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body);
        let (response, is_stream) = super::accepted(self.authorize(request).send().await.or_http()?).await?;
        let mut blocks = Blocks::default();
        if is_stream {
            read_stream(response, &mut blocks, on_text).await?;
        } else {
            let body: Value = serde_json::from_str(&response.text().await.or_http()?)?;
            blocks.stop_reason = body.get("stop_reason").and_then(Value::as_str).map(str::to_owned);
            blocks.tokens = (
                input_tokens(body.pointer("/usage")),
                body.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0),
            );
            for block in body.get("content").and_then(Value::as_array).into_iter().flatten() {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    on_text(Chunk::Text(text));
                }
                blocks.blocks.push(block.clone());
                blocks.arguments.push(String::new());
            }
        }
        blocks.finish()
    }

    fn set_model(&mut self, model: String) {
        self.model = model;
    }

    async fn list_models(&self) -> Result<Vec<crate::Model>> {
        self.list_models_authorized().await
    }
}

fn request_body(model: &str, messages: &[Message], tools: &[Value], max_tokens: usize) -> Result<Value> {
    let mut system = String::new();
    let mut converted: Vec<Value> = Vec::new();
    let mut outstanding = HashSet::new();
    for message in messages {
        let content = message.content.clone().unwrap_or_default();
        match message.role {
            Role::System => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&content);
            }
            Role::User => {
                require_no_outstanding(&outstanding)?;
                let mut blocks = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                blocks.extend(message.images.iter().map(|image| {
                    json!({"type": "image", "source": {
                        "type": "base64", "media_type": image.media_type, "data": image.data,
                    }})
                }));
                push_blocks(&mut converted, "user", blocks);
            }
            Role::Assistant => {
                require_no_outstanding(&outstanding)?;
                // Only blocks this API produced; a mid-session vendor switch
                // leaves foreign reasoning items this wire would refuse.
                let mut blocks: Vec<Value> = message
                    .thinking
                    .iter()
                    .filter(|block| {
                        matches!(
                            block.get("type").and_then(Value::as_str),
                            Some("thinking" | "redacted_thinking")
                        )
                    })
                    .cloned()
                    .collect();
                if !content.is_empty() {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                for call in &message.tool_calls {
                    if call.id.is_empty() || !outstanding.insert(call.id.clone()) {
                        return Err(Error::Provider(format!(
                            "invalid or duplicate Anthropic tool call id {:?}",
                            call.id
                        )));
                    }
                    let input: Value =
                        serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use", "id": call.id, "name": call.function.name, "input": input
                    }));
                }
                if !blocks.is_empty() {
                    push_blocks(&mut converted, "assistant", blocks);
                }
            }
            Role::Tool => {
                let id = message.tool_call_id.as_deref().filter(|id| outstanding.remove(*id)).ok_or_else(
                    || Error::Provider("Anthropic history contains an orphan tool result".into()),
                )?;
                let block = json!({
                    "type": "tool_result", "tool_use_id": id, "content": content
                });
                // The API requires all results of one assistant turn in a single user message.
                push_blocks(&mut converted, "user", vec![block]);
            }
        }
    }
    require_no_outstanding(&outstanding)?;

    // Two cache breakpoints, which is the difference between paying full price
    // for the whole history on every step and paying a tenth. Nothing is cached
    // unless asked, and reuse is by byte-identical prefix. Rendering order is
    // tools, system, messages: one mark on the system block covers the tools
    // with it and holds for the session, and one at the end of the newest turn
    // lets each step reuse every step before it. A write costs a quarter more
    // than an uncached read, so two requests break even — and a turn that calls
    // one tool is already two.
    let ephemeral = || json!({"type": "ephemeral"});
    // Only on a user turn: text, images and tool results carry a breakpoint but
    // a thinking block does not, and that is what an assistant turn can end
    // with. The history always ends on a user turn here; the guard is so that
    // staying true stops being an assumption.
    if let Some(last) = converted.last_mut().filter(|last| last["role"] == "user")
        && let Some(block) = last["content"].as_array_mut().and_then(|blocks| blocks.last_mut())
    {
        block["cache_control"] = ephemeral();
    }
    let mut body = json!({
        "model": model, "max_tokens": max_tokens, "messages": converted, "stream": true
    });
    if !system.is_empty() {
        // The block form rather than a bare string: only a block can carry a
        // breakpoint, and a bare string is what this sent before.
        body["system"] = json!([{"type": "text", "text": system, "cache_control": ephemeral()}]);
    }
    if !tools.is_empty() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool["function"]["name"],
                    "description": tool["function"]["description"],
                    "input_schema": tool["function"]["parameters"],
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    Ok(body)
}

fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    match messages.last_mut() {
        Some(last) if last["role"] == role => {
            last["content"].as_array_mut().expect("Anthropic content is always an array").extend(blocks);
        }
        _ => messages.push(json!({"role": role, "content": blocks})),
    }
}

fn require_no_outstanding(outstanding: &HashSet<String>) -> Result<()> {
    if outstanding.is_empty() {
        Ok(())
    } else {
        Err(Error::Provider(format!(
            "Anthropic history is missing tool results for: {}",
            outstanding.iter().cloned().collect::<Vec<_>>().join(", ")
        )))
    }
}

/// Accumulates streamed content blocks; `arguments` holds the concatenated
/// `input_json_delta` fragments per block index.
#[derive(Default)]
struct Blocks {
    blocks: Vec<Value>,
    arguments: Vec<String>,
    stop_reason: Option<String>,
    tokens: (u64, u64),
}

impl Blocks {
    fn apply<F>(&mut self, event: &Value, on_text: &mut F) -> Result<ControlFlow<()>>
    where
        F: FnMut(Chunk<'_>) + ?Sized,
    {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "content_block_start" => {
                while self.blocks.len() <= index {
                    self.blocks.push(json!({}));
                    self.arguments.push(String::new());
                }
                self.blocks[index] = event.get("content_block").cloned().unwrap_or_default();
            }
            "content_block_delta" if index < self.blocks.len() => {
                let delta = event.get("delta").cloned().unwrap_or_default();
                let block = &mut self.blocks[index];
                let kind = delta.get("type").and_then(Value::as_str).unwrap_or_default();
                let field = |name: &str| delta.get(name).and_then(Value::as_str).unwrap_or_default();
                match kind {
                    "text_delta" => {
                        on_text(Chunk::Text(field("text")));
                        append(block, "text", field("text"));
                    }
                    "input_json_delta" => self.arguments[index].push_str(field("partial_json")),
                    "thinking_delta" => {
                        on_text(Chunk::Reasoning(field("thinking")));
                        append(block, "thinking", field("thinking"));
                    }
                    "signature_delta" => {
                        block["signature"] = delta.get("signature").cloned().unwrap_or_default()
                    }
                    _ => {}
                }
            }
            "message_start" => self.tokens.0 = input_tokens(event.pointer("/message/usage")),
            "message_delta" => {
                self.stop_reason =
                    event.pointer("/delta/stop_reason").and_then(Value::as_str).map(str::to_owned);
                if let Some(output) = event.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.tokens.1 = output;
                }
            }
            "error" => {
                return Err(crate::stream_refusal(format!(
                    "Anthropic stream error: {}",
                    event.get("error").cloned().unwrap_or_default()
                )));
            }
            // This API's last word; the connection's own ending is not part
            // of the answer.
            "message_stop" => return Ok(ControlFlow::Break(())),
            _ => {} // message_start, content_block_stop, ping
        }
        Ok(ControlFlow::Continue(()))
    }

    fn finish(self) -> Result<AssistantTurn> {
        let tokens = self.tokens;
        let truncated = self.stop_reason.as_deref() == Some("max_tokens");
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut thinking = Vec::new();
        for (block, arguments) in self.blocks.into_iter().zip(self.arguments) {
            match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                "text" => content.push_str(block.get("text").and_then(Value::as_str).unwrap_or_default()),
                "tool_use" => {
                    let arguments = if arguments.is_empty() {
                        block.get("input").map_or_else(|| "{}".into(), Value::to_string)
                    } else {
                        arguments
                    };
                    // A call whose arguments the token cap cut mid-JSON is a
                    // call nobody meant to make; the text before it survives.
                    if truncated && serde_json::from_str::<Value>(&arguments).is_err() {
                        continue;
                    }
                    tool_calls.push(ToolCall {
                        id: block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                            arguments,
                        },
                    })
                }
                "thinking" | "redacted_thinking" => thinking.push(block),
                _ => {}
            }
        }
        if self.stop_reason.as_deref() == Some("refusal") && content.is_empty() && tool_calls.is_empty() {
            return Err(Error::Provider("Anthropic declined the request (stop_reason: refusal)".into()));
        }
        Ok(AssistantTurn {
            content: (!content.is_empty()).then_some(content),
            tool_calls,
            thinking,
            tokens,
            metrics: None,
        })
    }
}

/// Total input for a request. Cache traffic is reported in its own counters,
/// so `input_tokens` alone undercounts a cached session several-fold — the
/// figure users see in `/usage` and orchestrators bill against.
fn input_tokens(usage: Option<&Value>) -> u64 {
    ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
        .iter()
        .filter_map(|key| usage?.get(key)?.as_u64())
        .sum()
}

fn append(block: &mut Value, key: &str, text: &str) {
    let existing = block.get(key).and_then(Value::as_str).unwrap_or_default();
    block[key] = json!(format!("{existing}{text}"));
}

async fn read_stream<F>(response: reqwest::Response, blocks: &mut Blocks, on_text: &mut F) -> Result<()>
where
    F: FnMut(Chunk<'_>) + Send + ?Sized,
{
    super::read_events(response, |line| process_line(line, blocks, on_text)).await
}

fn process_line<F>(line: &[u8], blocks: &mut Blocks, on_text: &mut F) -> Result<ControlFlow<()>>
where
    F: FnMut(Chunk<'_>) + ?Sized,
{
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(data) = line.strip_prefix(b"data:") else {
        // "event:" lines and blanks; data carries its own "type"
        return Ok(ControlFlow::Continue(()));
    };
    let data = data.strip_prefix(b" ").unwrap_or(data);
    if data.is_empty() {
        return Ok(ControlFlow::Continue(()));
    }
    blocks.apply(&serde_json::from_slice(data)?, on_text)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    fn serve(listener: &TcpListener, body: &str, status: &str) -> String {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 16_384];
        let size = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..size]).into_owned();
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        request
    }

    #[test]
    fn converts_neutral_history_to_messages_api_shape() {
        let assistant = Message {
            role: Role::Assistant,
            content: Some("checking".into()),
            tool_calls: vec![ToolCall::new("t1", "read", r#"{"path":"a"}"#), ToolCall::new("t2", "ls", "{}")],
            tool_call_id: None,
            thinking: vec![json!({"type":"thinking","thinking":"hm","signature":"sig"})],
            origin: elmee_core::message::MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        };
        let messages = [
            Message::system("base"),
            Message::system("summary"),
            Message::user("go"),
            assistant,
            Message::tool("t1", "contents"),
            Message::tool("t2", "listing"),
        ];
        let tools = [json!({"type":"function","function":{
            "name":"read","description":"d","parameters":{"type":"object"}}})];

        let body = request_body("claude-opus-5", &messages, &tools, 32_000).unwrap();
        assert_eq!(body["system"][0]["text"], json!("base\n\nsummary"));
        assert_eq!(body["max_tokens"], json!(32_000));
        assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
        let converted = body["messages"].as_array().unwrap();
        assert_eq!(converted.len(), 3); // user, assistant, merged tool results
        let assistant_blocks = converted[1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks[0]["type"], "thinking");
        assert_eq!(assistant_blocks[1]["text"], "checking");
        assert_eq!(assistant_blocks[2]["input"], json!({"path":"a"}));
        let results = converted[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1]["tool_use_id"], "t2");
    }

    /// After a mid-session vendor switch the history carries another API's
    /// reasoning items; replaying them here is a 400 on every request until
    /// the session ends.
    #[test]
    fn foreign_reasoning_items_are_not_replayed_on_this_wire() {
        let mut assistant = Message::assistant_text("checking");
        assistant.thinking = vec![
            json!({"type":"reasoning","id":"rs_1","encrypted_content":"..."}),
            json!({"type":"thinking","thinking":"hm","signature":"sig"}),
            json!({"type":"redacted_thinking","data":"..."}),
        ];
        let body = request_body("claude", &[Message::user("go"), assistant], &[], 1_024).unwrap();
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        let kinds: Vec<&str> = blocks.iter().map(|block| block["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["thinking", "redacted_thinking", "text"], "{body}");
    }

    /// A tool call whose arguments the token cap cut mid-JSON is a call nobody
    /// meant to make — but mere text truncation is not an error, and a call
    /// that closed its JSON before the cap is still real.
    #[test]
    fn a_call_cut_by_the_token_cap_is_dropped_and_the_text_kept() {
        let stream = |arguments: &str| {
            let mut blocks = Blocks::default();
            let lines = [
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_owned(),
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an answer"}}"#.to_owned(),
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c1","name":"read","input":{}}}"#.to_owned(),
                format!(r#"data: {{"type":"content_block_delta","index":1,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#, json!(arguments)),
                r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#.to_owned(),
            ];
            for line in lines {
                let _ = process_line(line.as_bytes(), &mut blocks, &mut |_| {}).unwrap();
            }
            blocks.finish().unwrap()
        };

        let truncated = stream(r#"{"path":"x"#);
        assert_eq!(truncated.content.as_deref(), Some("half an answer"));
        assert!(truncated.tool_calls.is_empty(), "a cut call must not be executed");

        let complete = stream(r#"{"path":"x"}"#);
        assert_eq!(complete.tool_calls.len(), 1, "a call that finished its JSON stays");
    }

    /// Nothing is cached unless the request asks, and the request is matched by
    /// prefix — so the two marks have to land on the two things that are worth
    /// reusing: the session-long head (tools and system together) and the end of
    /// the newest turn. Without them every step re-reads the whole history at
    /// full price, which is what this used to do.
    #[test]
    fn both_cache_breakpoints_are_placed_where_the_prefix_is_reusable() {
        let messages = [Message::system("rules"), Message::user("first"), Message::user("second")];
        let body = request_body("claude-opus-5", &messages, &[], 1_000).unwrap();

        // Tools render before system, so one mark here covers both.
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");

        // The newest turn's last block, so the next step reuses this one.
        let turns = body["messages"].as_array().unwrap();
        let newest = turns.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(newest.last().unwrap()["cache_control"]["type"], "ephemeral");

        // And nowhere else: four are allowed in total, and each one spent here
        // is one unavailable to whatever needs it later.
        let marks = body.to_string().matches("cache_control").count();
        assert_eq!(marks, 2, "exactly two breakpoints: {body}");

        // A thinking block cannot carry one, so a history ending on an
        // assistant turn takes the system mark alone rather than risking a
        // refused request.
        let ends_assistant =
            [Message::system("rules"), Message::user("go"), Message::assistant_text("thought about it")];
        let body = request_body("claude-opus-5", &ends_assistant, &[], 1_000).unwrap();
        assert_eq!(body.to_string().matches("cache_control").count(), 1, "{body}");
    }

    #[test]
    fn normalizes_same_role_runs_and_refuses_broken_tool_pairs() {
        let messages = [
            Message::user("one"),
            Message::background("event", "two"),
            Message::assistant_text("three"),
            Message::assistant_text("four"),
        ];
        let body = request_body("claude", &messages, &[], 1_024).unwrap();
        let converted = body["messages"].as_array().unwrap();
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(converted[1]["content"].as_array().unwrap().len(), 2);

        let orphan = [Message::tool("missing", "result")];
        assert!(request_body("claude", &orphan, &[], 1_024).unwrap_err().to_string().contains("orphan"));

        let pending = [Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall::new("call", "read", "{}")],
            tool_call_id: None,
            thinking: Vec::new(),
            origin: elmee_core::message::MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        }];
        assert!(request_body("claude", &pending, &[], 1_024).unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn input_accounting_includes_cache_traffic() {
        let usage = json!({
            "input_tokens": 42, "cache_creation_input_tokens": 1_200, "cache_read_input_tokens": 18_000
        });
        assert_eq!(input_tokens(Some(&usage)), 19_242);
        // A provider that reports no cache counters is unaffected.
        assert_eq!(input_tokens(Some(&json!({"input_tokens": 42}))), 42);
        assert_eq!(input_tokens(None), 0);
    }

    #[tokio::test]
    async fn api_key_catalogue_is_authorized_and_reads_every_page() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let first = serve(
                &listener,
                r#"{"data":[{"id":"claude-opus"},{"id":"shared"}],"has_more":true,"last_id":"page one"}"#,
                "200 OK",
            );
            let second = serve(
                &listener,
                r#"{"data":[{"id":"shared"},{"id":"claude-sonnet"}],"has_more":false,"last_id":"done"}"#,
                "200 OK",
            );
            (first, second)
        });

        let provider = Anthropic::new(format!("http://{address}"), Some("api-secret".into()), String::new());
        let models = provider.list_models().await.unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["claude-opus", "shared", "claude-sonnet"]);

        let (first, second) = server.join().unwrap();
        for request in [&first, &second] {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("x-api-key: api-secret\r\n"), "{request}");
            assert!(lower.contains("anthropic-version: 2023-06-01\r\n"));
        }
        assert!(first.starts_with("GET /v1/models?limit=1000 "), "{first}");
        assert!(second.starts_with("GET /v1/models?limit=1000&after_id=page+one "), "{second}");
    }

    #[tokio::test]
    async fn catalogue_refusals_keep_the_status_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve(&listener, r#"{"error":{"message":"wrong scope"}}"#, "403 Forbidden")
        });
        let provider = Anthropic::new(format!("http://{address}"), Some("api-secret".into()), String::new());
        let error = provider.list_models().await.unwrap_err();
        assert!(
            matches!(error, Error::Refused { status: 403, ref body, .. } if body.contains("wrong scope"))
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn malformed_pagination_stops_instead_of_looping_forever() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve(&listener, r#"{"data":[],"has_more":true,"last_id":null}"#, "200 OK")
        });
        let provider = Anthropic::new(format!("http://{address}"), Some("key".into()), String::new());
        let error = provider.list_models().await.unwrap_err();
        assert!(error.to_string().contains("no last_id cursor"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn assembles_streamed_text_thinking_and_tool_use() {
        let mut blocks = Blocks::default();
        let mut streamed = String::new();
        let mut reasoned = String::new();
        let lines = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing it up"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi "}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"there"}}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"c1","name":"read","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"th\":\"x\"}"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ];
        for line in lines {
            let _ = process_line(line.as_bytes(), &mut blocks, &mut |chunk| match chunk {
                Chunk::Text(text) => streamed.push_str(text),
                Chunk::Reasoning(text) => reasoned.push_str(text),
            })
            .unwrap();
        }

        let turn = blocks.finish().unwrap();
        assert_eq!(streamed, "Hi there");
        assert_eq!(turn.content.as_deref(), Some("Hi there"));
        assert_eq!(turn.tool_calls[0].function.name, "read");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"path":"x"}"#);
        assert_eq!(turn.thinking[0]["signature"], "s1");
        // The thinking is replayed to the provider *and* shown to the person:
        // capturing it for replay while streaming none of it left a spinner
        // and an empty screen for as long as the model reasoned.
        assert_eq!(reasoned, turn.thinking[0]["thinking"].as_str().unwrap_or_default());
        assert!(!reasoned.is_empty(), "reasoning must reach whoever is watching");
    }
}
