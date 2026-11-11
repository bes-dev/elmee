use std::ops::ControlFlow;

use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::{Error, Result};
use elmee_core::message::{FunctionCall, Message, ToolCall};

use super::{AssistantTurn, Chunk, Provider};
use crate::OrHttp;
use elmee_core::provider::ReasoningEffort;

pub struct OpenAiCompatible {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
}

impl OpenAiCompatible {
    pub fn new(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self { client: super::http_client(), base_url, api_key, model, reasoning_effort: None }
    }
}

#[elmee_core::async_trait(?Send)]
impl Provider for OpenAiCompatible {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        let wire: Vec<serde_json::Value> = messages.iter().map(wire_message).collect();
        let mut body = json!({
            "model": self.model,
            "messages": wire,
            "tools": tools,
            "tool_choice": "auto",
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        if let Some(effort) = self.reasoning_effort {
            body["reasoning_effort"] = json!(effort.as_str());
        }
        let mut request = self.client.post(format!("{}/chat/completions", self.base_url)).json(&body);

        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let (response, is_stream) = super::accepted(request.send().await.or_http()?).await?;
        if is_stream {
            return read_stream(response, on_text).await;
        }

        let body = response.text().await.or_http()?;
        let response: ApiResponse<ChatChoice> = serde_json::from_str(&body)?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Provider("provider returned no choices".into()))?;
        let mut message = choice.message;
        if choice.finish_reason.as_deref() == Some("length") {
            drop_truncated_calls(&mut message.tool_calls);
        }
        if let Some(content) = message.content.as_deref() {
            on_text(Chunk::Text(content));
        }
        message.tokens =
            response.usage.map_or((0, 0), |usage| (usage.prompt_tokens, usage.completion_tokens));
        Ok(message)
    }

    fn set_model(&mut self, model: String) {
        self.model = model;
    }

    fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.reasoning_effort = effort;
    }

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }

    async fn list_models(&self) -> Result<Vec<crate::Model>> {
        let mut request = self.client.get(format!("{}/models", self.base_url));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let (response, _) = super::accepted(request.send().await.or_http()?).await?;
        let body: serde_json::Value = serde_json::from_str(&response.text().await.or_http()?)?;
        Ok(super::model_ids(&body))
    }
}

/// The neutral `Message` restricted to what the Chat Completions wire accepts:
/// role, content, tool calls and the result id. `thinking` belongs to another
/// vendor's protocol, and `origin`/`event_id` are internal bookkeeping.
fn wire_message(message: &Message) -> serde_json::Value {
    let mut value = json!({"role": message.role});
    if message.images.is_empty() {
        if let Some(content) = &message.content {
            value["content"] = json!(content);
        }
    } else {
        let mut parts = vec![json!({"type": "text", "text": message.content.clone().unwrap_or_default()})];
        parts.extend(
            message
                .images
                .iter()
                .map(|image| json!({"type": "image_url", "image_url": {"url": image.data_url()}})),
        );
        value["content"] = json!(parts);
    }
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = json!(message.tool_calls);
    }
    if let Some(id) = &message.tool_call_id {
        value["tool_call_id"] = json!(id);
    }
    value
}

/// A call whose arguments the token cap cut mid-JSON is a call nobody meant to
/// make; the text of the turn survives.
fn drop_truncated_calls(calls: &mut Vec<ToolCall>) {
    calls.retain(|call| serde_json::from_str::<serde_json::Value>(&call.function.arguments).is_ok());
}

async fn read_stream<F>(response: reqwest::Response, on_text: &mut F) -> Result<AssistantTurn>
where
    F: FnMut(Chunk<'_>) + Send + ?Sized,
{
    let mut content = String::new();
    let mut calls: Vec<ToolCallBuilder> = Vec::new();
    let mut tokens = (0, 0);
    let mut finish = None;
    super::read_events(response, |line| {
        process_sse_line(line, &mut content, &mut calls, &mut tokens, &mut finish, on_text)
    })
    .await?;

    let mut tool_calls: Vec<ToolCall> = calls
        .into_iter()
        .enumerate()
        .filter(|(_, call)| !call.name.is_empty())
        .map(|(index, call)| ToolCall {
            id: call.id.unwrap_or_else(|| format!("call_{index}")),
            kind: call.kind.unwrap_or_else(|| "function".into()),
            function: FunctionCall { name: call.name, arguments: call.arguments },
        })
        .collect();
    if finish.as_deref() == Some("length") {
        drop_truncated_calls(&mut tool_calls);
    }
    Ok(AssistantTurn {
        content: (!content.is_empty()).then_some(content),
        tool_calls,
        thinking: Vec::new(),
        tokens,
        metrics: None,
    })
}

fn process_sse_line<F>(
    line: &[u8],
    content: &mut String,
    calls: &mut Vec<ToolCallBuilder>,
    tokens: &mut (u64, u64),
    finish: &mut Option<String>,
    on_text: &mut F,
) -> Result<ControlFlow<()>>
where
    F: FnMut(Chunk<'_>) + ?Sized,
{
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(data) = line.strip_prefix(b"data:") else {
        return Ok(ControlFlow::Continue(()));
    };
    let data = data.strip_prefix(b" ").unwrap_or(data);
    // `[DONE]` is this API's last word; whatever the connection does after it
    // is not part of the answer.
    if data == b"[DONE]" {
        return Ok(ControlFlow::Break(()));
    }
    if data.is_empty() {
        return Ok(ControlFlow::Continue(()));
    }
    let value: serde_json::Value = serde_json::from_slice(data)?;
    if let Some(error) = value.get("error") {
        let detail =
            error.get("message").and_then(serde_json::Value::as_str).unwrap_or("provider stream error");
        return Err(crate::stream_refusal(detail.to_owned()));
    }
    let event: ApiResponse<StreamChoice> = serde_json::from_value(value)?;
    if let Some(usage) = event.usage {
        *tokens = (usage.prompt_tokens, usage.completion_tokens);
    }
    for choice in event.choices {
        if let Some(reason) = choice.finish_reason {
            *finish = Some(reason);
        }
        if let Some(thought) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
            on_text(Chunk::Reasoning(&thought));
        }
        if let Some(text) = choice.delta.content {
            on_text(Chunk::Text(&text));
            content.push_str(&text);
        }
        for fragment in choice.delta.tool_calls {
            while calls.len() <= fragment.index {
                calls.push(ToolCallBuilder::default());
            }
            let call = &mut calls[fragment.index];
            if let Some(id) = fragment.id {
                call.id = Some(id);
            }
            if let Some(kind) = fragment.kind {
                call.kind = Some(kind);
            }
            if let Some(function) = fragment.function {
                if let Some(name) = function.name {
                    call.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    match arguments {
                        serde_json::Value::String(value) => call.arguments.push_str(&value),
                        value => call.arguments.push_str(&serde_json::to_string(&value)?),
                    }
                }
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

#[derive(Default)]
struct ToolCallBuilder {
    id: Option<String>,
    kind: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    #[serde(default = "Vec::new")]
    choices: Vec<T>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    /// What the model was working out before it answered.
    ///
    /// Not part of the OpenAI schema, and read anyway because every
    /// OpenAI-compatible endpoint that has thinking models put it here:
    /// `reasoning_content` in DeepSeek, vLLM, SGLang, Ollama and most
    /// gateways, `reasoning` in a few others. Without it a local reasoning
    /// model is a run of tool calls with nothing saying why — which is
    /// exactly what it looked like.
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallFragment>,
}

#[derive(Deserialize)]
struct ToolCallFragment {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<FunctionFragment>,
}

#[derive(Deserialize)]
struct FunctionFragment {
    name: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: AssistantTurn,
    finish_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    /// Thinking models on an OpenAI-compatible endpoint put their working in a
    /// field OpenAI never specified, and every one of them picked a different
    /// one of two names. Reading neither made a local reasoning model a run of
    /// tool calls with nothing saying why.
    #[test]
    fn reasoning_is_read_under_either_name_the_ecosystem_uses() {
        for field in ["reasoning_content", "reasoning"] {
            let delta: StreamDelta =
                serde_json::from_str(&format!(r#"{{"{field}": "weighing two options"}}"#)).unwrap();
            let thought = delta.reasoning_content.or(delta.reasoning);
            assert_eq!(thought.as_deref(), Some("weighing two options"), "{field}");
        }
        // And a delta with neither is still an ordinary delta.
        let plain: StreamDelta = serde_json::from_str(r#"{"content": "hello"}"#).unwrap();
        assert!(plain.reasoning_content.is_none() && plain.reasoning.is_none());
    }

    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    #[tokio::test]
    async fn calls_openai_compatible_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("POST /v1/chat/completions"));
            assert!(request.contains(r#""reasoning_effort":"high""#), "{request}");

            let body = r#"{"choices":[{"message":{"content":"hello","tool_calls":[]}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let mut provider = OpenAiCompatible::new(format!("http://{address}/v1"), None, "test-model".into());
        provider.set_reasoning_effort(Some(ReasoningEffort::High));
        let mut streamed = String::new();
        let turn = provider
            .complete(&[Message::user("hi")], &[], &mut |chunk| {
                if let Chunk::Text(text) = chunk {
                    streamed.push_str(text)
                }
            })
            .await
            .unwrap();
        assert_eq!(turn.content.as_deref(), Some("hello"));
        assert_eq!(streamed, "hello");
        server.join().unwrap();
    }

    /// The neutral `Message` used to be serialized verbatim, so another
    /// vendor's thinking blocks and internal bookkeeping rode along on a wire
    /// that never defined them.
    #[test]
    fn wire_messages_carry_no_foreign_thinking_or_internal_metadata() {
        let mut message = Message::assistant_text("hi");
        message.thinking = vec![json!({"type":"thinking","thinking":"hm","signature":"sig"})];
        message.origin = elmee_core::message::MessageOrigin::Background;
        message.event_id = Some("e1".into());
        message.tool_calls = vec![ToolCall::new("c1", "read", "{}")];
        let value = wire_message(&message);
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"], "hi");
        assert_eq!(value["tool_calls"][0]["type"], "function");
        for foreign in ["thinking", "origin", "event_id", "images"] {
            assert!(value.get(foreign).is_none(), "{foreign} is not chat-completions wire: {value}");
        }
    }

    /// `finish_reason: "length"` means the token cap spoke last: a call whose
    /// arguments it cut mid-JSON is dropped, the text and any call that closed
    /// its JSON are kept, and the turn is not an error.
    #[tokio::test]
    async fn a_call_cut_by_the_length_cap_is_dropped_and_the_text_kept() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"so far\",\"tool_calls\":[",
                "{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}},",
                "{\"index\":1,\"id\":\"c2\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let provider = OpenAiCompatible::new(format!("http://{address}/v1"), None, "test-model".into());
        let turn = provider.complete(&[Message::user("hi")], &[], &mut |_| {}).await.unwrap();
        assert_eq!(turn.content.as_deref(), Some("so far"), "text truncation is not an error");
        let names: Vec<&str> = turn.tool_calls.iter().map(|call| call.function.name.as_str()).collect();
        assert_eq!(names, ["ls"], "the cut call goes, the complete one stays");
        server.join().unwrap();
    }

    /// A 401 used to be parsed as a catalogue with no models in it.
    #[tokio::test]
    async fn an_unauthorized_catalogue_is_an_error_not_an_empty_list() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let body = r#"{"error":{"message":"bad key"}}"#;
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let provider = OpenAiCompatible::new(format!("http://{address}/v1"), None, "m".into());
        let error = provider.list_models().await.unwrap_err();
        assert!(
            matches!(error, Error::Refused { status: 401, ref body, .. } if body.contains("bad key")),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn assembles_streamed_text_and_tool_calls() {
        let mut content = String::new();
        let mut calls = Vec::new();
        let mut streamed = String::new();
        let mut tokens = (0, 0);
        let mut finish = None;
        let _ = process_sse_line(
            br#"data: {"choices":[{"delta":{"content":"Hi ","tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"re","arguments":"{\"pa"}}]}}]}"#,
            &mut content,
            &mut calls,
            &mut tokens,
            &mut finish,
            &mut |chunk| if let Chunk::Text(text) = chunk { streamed.push_str(text) },
        )
        .unwrap();
        let _ = process_sse_line(
            br#"data: {"choices":[{"delta":{"content":"there","tool_calls":[{"index":0,"function":{"name":"ad","arguments":"th\":\"x\"}"}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
            &mut content,
            &mut calls,
            &mut tokens,
            &mut finish,
            &mut |chunk| if let Chunk::Text(text) = chunk { streamed.push_str(text) },
        )
        .unwrap();
        assert_eq!(tokens, (7, 3));

        assert_eq!(content, "Hi there");
        assert_eq!(streamed, content);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, r#"{"path":"x"}"#);
    }
}
