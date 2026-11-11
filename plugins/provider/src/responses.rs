//! OpenAI's Responses API.
//!
//! A second OpenAI wire rather than a variant of the first: the request is not
//! a list of messages but a list of *items*, the system prompt is a field of
//! its own, and a tool call is an item in the same stream as the text rather
//! than a fragment attached to a delta. Trying to bend the chat adapter into
//! this shape would have left one function branching on which OpenAI it was
//! talking to at every step.
//!
//! It exists here for one reason: a ChatGPT subscription speaks only this. To
//! be exact about what that means — a subscription is one OpenAI account, and
//! the surface it reaches programmatically is the coding one, so the models on
//! offer are the ones that endpoint serves and not everything chatgpt.com can
//! show a person in a browser. The same adapter also serves `api.openai.com/v1`
//! with an ordinary key, which is how it can be exercised without a
//! subscription at all.

use elmee_core::provider::RequestMetrics;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Instant;

use crate::Result;
use elmee_core::message::{FunctionCall, Message, Role, ToolCall};
use elmee_core::provider::ReasoningEffort;

use super::{AssistantTurn, Chunk, Provider};
use crate::OrHttp;

/// Where the bearer token comes from.
pub enum Credential {
    Key(Option<String>),
    /// A signed-in ChatGPT account: the token is short-lived and renewed as
    /// needed, so it is fetched per request rather than held here.
    Subscription,
}

pub struct Responses {
    client: Client,
    base_url: String,
    model: String,
    credential: Credential,
    client_version: String,
    reasoning_effort: Option<ReasoningEffort>,
    context: std::rc::Rc<crate::SessionContext>,
}

impl Responses {
    pub fn new(
        base_url: String,
        credential: Credential,
        model: String,
        client_version: Option<String>,
    ) -> Self {
        let client_version = client_version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
        Self {
            client: super::http_client(),
            base_url,
            model,
            credential,
            client_version,
            reasoning_effort: None,
            context: std::rc::Rc::default(),
        }
    }

    pub(super) fn with_context(mut self, context: std::rc::Rc<crate::SessionContext>) -> Self {
        self.context = context;
        self
    }

    async fn authorize(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        Ok(match &self.credential {
            Credential::Key(None) => request,
            Credential::Key(Some(key)) => request.bearer_auth(key),
            Credential::Subscription => {
                let account = crate::auth::current().await?;
                request
                    .bearer_auth(&account.access_token)
                    .header("ChatGPT-Account-ID", &account.account_id)
                    // Truthfully this client, not the one whose OAuth app was
                    // used to sign in. If the endpoint ever refuses on that
                    // basis, the refusal is the answer.
                    .header("originator", "elmee")
                    .header("session_id", self.context.routing_id())
            }
        })
    }
}

#[elmee_core::async_trait(?Send)]
impl Provider for Responses {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        let started = Instant::now();
        let mut body = request_body(&self.model, messages, tools, self.reasoning_effort);
        // One key per conversation routes its requests to the cache that
        // already holds its prefix; independent calls may come back together,
        // which is a round trip saved for every file read beside another.
        body["prompt_cache_key"] = json!(self.context.key());
        body["parallel_tool_calls"] = json!(true);
        let encoding = Instant::now();
        let encoded = serde_json::to_vec(&body)?;
        let mut metrics =
            RequestMetrics { attempts: 1, serialize_ms: Some(elapsed(encoding)), ..Default::default() };
        let auth_started = Instant::now();
        let request = self.authorize(self.client.post(format!("{}/responses", self.base_url))).await?;
        metrics.auth_ms = Some(elapsed(auth_started));
        let (response, _) = super::accepted(
            request.header("content-type", "application/json").body(encoded).send().await.or_http()?,
        )
        .await?;
        metrics.headers_ms = Some(elapsed(started));
        read_stream_at(response, on_text, started, metrics).await
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

    /// Ask the endpoint what it serves, rather than believing a list compiled
    /// into the binary.
    ///
    /// `client_version` is required — without it the request is a 400 and the
    /// catalogue silently read as empty, which is why the built-in list looked
    /// like the only answer there was. Each model in the reply declares a
    /// `minimal_client_version` of its own, so a client below it is told about
    /// fewer models: the field is a statement about which protocol features the
    /// caller implements, and elmee sends its own version because that is what
    /// it implements. `clientVersion` in models.json is there for whoever wants
    /// to say otherwise about their own account.
    async fn list_models(&self) -> Result<Vec<crate::Model>> {
        let url = format!("{}/models?client_version={}", self.base_url, self.client_version);
        let request = self.authorize(self.client.get(url)).await?;
        let (response, _) = super::accepted(request.send().await.or_http()?).await?;
        let body: Value = serde_json::from_str(&response.text().await.or_http()?)?;
        Ok(model_ids(&body))
    }
}

fn request_body(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    reasoning_effort: Option<ReasoningEffort>,
) -> Value {
    let (instructions, input) = split_instructions(messages);
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tools": tools.iter().map(flatten_tool).collect::<Vec<_>>(),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        // Nothing is left on the server: the session file is the record,
        // and a stored response would be a second one nobody prunes.
        "store": false,
        // We manage history ourselves. With stateless Responses requests,
        // the encrypted item is what lets the next request faithfully
        // replay reasoning instead of sending only its visible summary.
        "include": ["reasoning.encrypted_content"],
        "stream": true,
        // Asking for the summary is what makes reasoning visible: the model
        // thinks either way, but without this the API streams none of it
        // and a person watching a thinking model sees a spinner and an
        // empty screen for however long it takes.
        "reasoning": {"summary": "auto"},
    });
    if let Some(effort) = reasoning_effort {
        body["reasoning"]["effort"] = json!(effort.as_str());
    }
    body
}

/// The system prompt is a field here, not a message. Everything else becomes an
/// item, and the order of the conversation is preserved exactly.
fn split_instructions(messages: &[Message]) -> (String, Vec<Value>) {
    let mut instructions = String::new();
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(message.content.as_deref().unwrap_or_default());
            }
            Role::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content.clone().unwrap_or_default(),
            })),
            Role::User => {
                let mut content = Vec::new();
                if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
                    content.push(json!({"type": "input_text", "text": text}));
                }
                content.extend(
                    message
                        .images
                        .iter()
                        .map(|image| json!({"type": "input_image", "image_url": image.data_url()})),
                );
                input.push(json!({"type": "message", "role": "user", "content": content}));
            }
            Role::Assistant => {
                // Reasoning first: the model produced it before it spoke or
                // called anything, and replaying it out of order breaks the
                // chain it is meant to continue. Only this API's own items —
                // an Anthropic thinking block left by a mid-session vendor
                // switch is not an item this wire accepts.
                input.extend(
                    message
                        .thinking
                        .iter()
                        .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                        .cloned(),
                );
                if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    }));
                }
            }
        }
    }
    (instructions, input)
}

/// The catalogue comes back in one of two shapes depending on which endpoint
/// answered: `{"data":[{"id":..}]}` from the public API, `{"models":[{"slug":..}]}`
/// from the one a subscription talks to. The second carries each model's
/// context window, which is the only place that number is authoritative.
fn model_ids(body: &Value) -> Vec<crate::Model> {
    let slugs: Vec<crate::Model> = body
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let slug = model.get("slug").and_then(Value::as_str)?;
            Some(crate::Model::new(slug, super::declared_window(model)))
        })
        .collect();
    match slugs.is_empty() {
        true => super::model_ids(body),
        false => slugs,
    }
}

/// Chat-style tools nest the declaration under `function`; here it is flat.
fn flatten_tool(tool: &Value) -> Value {
    let Some(function) = tool.get("function") else { return tool.clone() };
    json!({
        "type": "function",
        "name": function.get("name").cloned().unwrap_or(Value::Null),
        "description": function.get("description").cloned().unwrap_or(Value::Null),
        "parameters": function.get("parameters").cloned().unwrap_or(Value::Null),
    })
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

async fn read_stream_at<F>(
    response: reqwest::Response,
    on_text: &mut F,
    started: Instant,
    metrics: RequestMetrics,
) -> Result<AssistantTurn>
where
    F: FnMut(Chunk<'_>) + Send + ?Sized,
{
    let mut turn = AssistantTurn::text("");
    turn.content = None;
    turn.metrics = Some(metrics);
    super::read_events(response, |line| process_sse_line_at(line, &mut turn, on_text, started)).await?;
    turn.metrics.as_mut().unwrap().total_ms = Some(elapsed(started));
    Ok(turn)
}

#[cfg(test)]
fn process_sse_line<F>(
    line: &[u8],
    turn: &mut AssistantTurn,
    on_text: &mut F,
) -> Result<std::ops::ControlFlow<()>>
where
    F: FnMut(Chunk<'_>) + ?Sized,
{
    process_sse_line_at(line, turn, on_text, Instant::now())
}

fn process_sse_line_at<F>(
    line: &[u8],
    turn: &mut AssistantTurn,
    on_text: &mut F,
    started: Instant,
) -> Result<std::ops::ControlFlow<()>>
where
    F: FnMut(Chunk<'_>) + ?Sized,
{
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(data) = line.strip_prefix(b"data:") else { return Ok(std::ops::ControlFlow::Continue(())) };
    let data = data.strip_prefix(b" ").unwrap_or(data);
    if data == b"[DONE]" || data.is_empty() {
        return Ok(std::ops::ControlFlow::Continue(()));
    }
    let event: Event = serde_json::from_slice(data)?;
    let metrics = turn.metrics.get_or_insert_with(RequestMetrics::default);
    if event.kind == "response.output_text.delta" && event.delta.as_deref().is_some_and(|s| !s.is_empty()) {
        metrics.first_text_ms.get_or_insert_with(|| elapsed(started));
    }
    if event.kind == "response.function_call_arguments.delta"
        || (event.kind == "response.output_item.added"
            && event.item.as_ref().and_then(|item| item.get("type")).and_then(Value::as_str)
                == Some("function_call"))
    {
        metrics.first_tool_ms.get_or_insert_with(|| elapsed(started));
    }
    match event.kind.as_str() {
        // The model's own summary of what it is working through. Only sent
        // when summaries are on, which is why its absence is not an error.
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = &event.delta {
                on_text(Chunk::Reasoning(delta));
            }
        }
        "response.output_text.delta" => {
            let delta = event.delta.unwrap_or_default();
            on_text(Chunk::Text(&delta));
            turn.content.get_or_insert_default().push_str(&delta);
        }
        // Every completed item arrives whole on this one event, whatever it
        // is: the text already streamed, a call to make, or reasoning to
        // replay. Only the last two are news.
        "response.output_item.done" => match event.item {
            Some(item) if item.get("type").and_then(Value::as_str) == Some("function_call") => {
                turn.tool_calls.push(ToolCall {
                    id: string(&item, "call_id"),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: string(&item, "name"),
                        arguments: string(&item, "arguments"),
                    },
                });
            }
            Some(item) if item.get("type").and_then(Value::as_str) == Some("reasoning") => {
                turn.thinking.push(item);
            }
            _ => {}
        },
        // This API's last word. What the connection does after it is not the
        // model's answer, and waiting for the connection to end is how a
        // finished turn was lost to a transport hiccup.
        "response.completed" => {
            let metrics = turn.metrics.as_mut().unwrap();
            metrics.cached_input_tokens = event
                .response
                .as_ref()
                .and_then(|r| r.pointer("/usage/input_tokens_details/cached_tokens"))
                .and_then(Value::as_u64);
            metrics.service_tier = event
                .response
                .as_ref()
                .and_then(|r| r.get("service_tier"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(usage) = event.response.as_ref().and_then(|response| response.get("usage")) {
                let count = |field| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
                turn.tokens = (count("input_tokens"), count("output_tokens"));
            }
            return Ok(std::ops::ControlFlow::Break(()));
        }
        // The stream reports its own failure and then simply stops; without
        // this the turn would end looking merely empty.
        "response.failed" | "response.incomplete" => {
            let detail = event
                .response
                .as_ref()
                .and_then(|response| response.get("error").or_else(|| response.get("incomplete_details")))
                .map_or_else(|| event.kind.clone(), ToString::to_string);
            return Err(crate::stream_refusal(detail));
        }
        _ => {}
    }
    Ok(std::ops::ControlFlow::Continue(()))
}

fn string(item: &Value, field: &str) -> String {
    item.get(field).and_then(Value::as_str).unwrap_or_default().to_owned()
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
    item: Option<Value>,
    response: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str, call: Option<(&str, &str, &str)>) -> Message {
        Message {
            role: Role::Assistant,
            content: (!text.is_empty()).then(|| text.to_owned()),
            tool_calls: call
                .map(|(id, name, arguments)| ToolCall {
                    id: id.into(),
                    kind: "function".into(),
                    function: FunctionCall { name: name.into(), arguments: arguments.into() },
                })
                .into_iter()
                .collect(),
            tool_call_id: None,
            thinking: vec![json!({"type": "reasoning", "id": "rs_1"})],
            origin: Default::default(),
            event_id: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn a_conversation_becomes_instructions_and_items_in_order() {
        let mut tool_result = Message::user("42");
        tool_result.role = Role::Tool;
        tool_result.tool_call_id = Some("c1".into());
        let messages = vec![
            Message::system("be brief"),
            Message::user("what is 6*7"),
            assistant("let me check", Some(("c1", "calc", r#"{"x":6}"#))),
            tool_result,
        ];
        let (instructions, input) = split_instructions(&messages);

        assert_eq!(instructions, "be brief");
        let kinds: Vec<&str> = input.iter().map(|item| item["type"].as_str().unwrap_or_default()).collect();
        // Reasoning precedes the text it led to, and the tool result follows
        // the call it answers.
        assert_eq!(kinds, ["message", "reasoning", "message", "function_call", "function_call_output"]);
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[4]["output"], "42");
    }

    /// After a mid-session vendor switch the history carries Anthropic thinking
    /// blocks; replaying them as items here is a 400 on every request until the
    /// session ends.
    #[test]
    fn foreign_thinking_blocks_are_not_replayed_on_this_wire() {
        let mut message = assistant("checking", None);
        message.thinking = vec![
            json!({"type":"thinking","thinking":"hm","signature":"sig"}),
            json!({"type":"redacted_thinking","data":"..."}),
            json!({"type":"reasoning","id":"rs_1"}),
        ];
        let (_, input) = split_instructions(&[message]);
        let kinds: Vec<&str> = input.iter().map(|item| item["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["reasoning", "message"]);
    }

    /// A 401 used to be parsed as a catalogue with no models in it.
    #[tokio::test]
    async fn an_unauthorized_catalogue_is_an_error_not_an_empty_list() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
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
        let provider = Responses::new(
            format!("http://{address}/v1"),
            Credential::Key(Some("stale".into())),
            "gpt-test".into(),
            None,
        );
        let error = provider.list_models().await.unwrap_err();
        assert!(
            matches!(error, crate::Error::Refused { status: 401, ref body, .. } if body.contains("bad key")),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_catalogue_is_read_in_either_shape_and_keeps_the_window_it_states() {
        assert_eq!(model_ids(&json!({"data": [{"id": "gpt-4o"}]})), [crate::Model::new("gpt-4o", None)]);
        assert_eq!(
            model_ids(&json!({"models": [{"slug": "gpt-5.6-sol", "context_window": 272_000}]})),
            [crate::Model::new("gpt-5.6-sol", Some(272_000))]
        );
        // The reason the window travels at all: two models on one endpoint do
        // not share one, and compaction is calibrated from the number.
        assert_eq!(
            model_ids(&json!({"models": [
                {"slug": "gpt-5.6-sol", "context_window": 272_000},
                {"slug": "gpt-5.3-codex-spark", "context_window": 128_000}
            ]}))
            .iter()
            .map(|model| model.context_window)
            .collect::<Vec<_>>(),
            [Some(272_000), Some(128_000)]
        );
        assert!(model_ids(&json!({"models": []})).is_empty());
    }

    #[test]
    fn a_chat_style_tool_declaration_is_flattened() {
        let tool = json!({"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}});
        let flat = flatten_tool(&tool);
        assert_eq!(flat["name"], "read");
        assert_eq!(flat["parameters"]["type"], "object");
        assert!(flat.get("function").is_none());
    }

    #[test]
    fn stateless_requests_ask_for_replayable_reasoning() {
        let body = request_body("model", &[Message::user("hello")], &[], None);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn reasoning_effort_is_optional_and_keeps_the_summary() {
        let automatic = request_body("model", &[], &[], None);
        assert!(automatic["reasoning"].get("effort").is_none());
        let high = request_body("model", &[], &[], Some(ReasoningEffort::High));
        assert_eq!(high["reasoning"], json!({"summary": "auto", "effort": "high"}));
    }

    #[test]
    fn assembles_streamed_text_calls_and_usage() {
        let mut turn = AssistantTurn {
            content: None,
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            tokens: (0, 0),
            metrics: None,
        };
        let mut streamed = String::new();
        for line in [
            br#"data: {"type":"response.output_text.delta","delta":"Hi "}"#.as_slice(),
            br#"data: {"type":"response.output_text.delta","delta":"there"}"#,
            br#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1"}}"#,
            br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"x\"}"}}"#,
            br#"data: {"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":3}}}"#,
        ] {
            let _ = process_sse_line(line, &mut turn, &mut |chunk| if let Chunk::Text(text) = chunk { streamed.push_str(text) }).unwrap();
        }

        assert_eq!(turn.content.as_deref(), Some("Hi there"));
        assert_eq!(streamed, "Hi there");
        assert_eq!(turn.tool_calls[0].function.name, "read");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"path":"x"}"#);
        assert_eq!(turn.thinking[0]["id"], "rs_1");
        assert_eq!(turn.tokens, (7, 3));
    }

    #[tokio::test]
    async fn posts_a_responses_request_and_reads_the_stream_back() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 16384];
            let size = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
            let events = "event: response.output_text.delta\n\
                 data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n\
                 data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                events.len()
            )
            .unwrap();
            request
        });

        let provider = Responses::new(
            format!("http://{address}/v1"),
            Credential::Key(Some("secret".into())),
            "gpt-test".into(),
            None,
        );
        let tool = json!({"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}});
        let mut streamed = String::new();
        let turn = provider
            .complete(&[Message::system("be brief"), Message::user("hi")], &[tool], &mut |chunk| {
                if let Chunk::Text(text) = chunk {
                    streamed.push_str(text);
                }
            })
            .await
            .unwrap();

        assert_eq!(turn.content.as_deref(), Some("ok"));
        assert_eq!(streamed, "ok");
        assert_eq!(turn.tokens, (2, 1));

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/responses"), "{request}");
        assert!(request.contains("authorization: Bearer secret"), "{request}");
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["tools"][0]["name"], "read", "tools are sent flat");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn a_failed_stream_reports_instead_of_ending_empty() {
        let mut turn = AssistantTurn {
            content: None,
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            tokens: (0, 0),
            metrics: None,
        };
        let error = process_sse_line(
            br#"data: {"type":"response.failed","response":{"error":{"message":"quota"}}}"#,
            &mut turn,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("quota"), "{error}");
    }
}
