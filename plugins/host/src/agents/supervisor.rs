//! Driving a child agent over the Agent Client Protocol, with no policy in it.
//!
//! A subagent here is a function call: forked over project-scoped mesh state,
//! joined, its report read back into the caller's transcript. The pipe between
//! the two is the same protocol an editor speaks to elmee, with this side as
//! the client: a handshake, one session, one prompt, and the updates that
//! stream back until the answer. That is why any program speaking the
//! protocol on stdio can sit on the other end — a declared external agent is
//! delegated to exactly like another elmee.
//!
//! This module is only the streaming core of that: two clocks, a dispatch over
//! the child's messages, and an EOF that means the process ended before it
//! said anything. Exactly two decisions inside the loop are policy — where
//! progress is recorded, and how a question is answered — and those are the
//! [`Observer`].

use std::{path::Path, time::Duration};

use elmee_core::{
    InteractionResponse,
    interaction::{OptionItem, Question},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    time::Instant,
};

/// How often the watchdog looks at its clocks while waiting for output.
const TICK: Duration = Duration::from_secs(5);

/// What a supervised child is doing.
///
/// One vocabulary, spelled once. The view struct keeps a plain string: that is
/// a record a frontend renders, and an unknown word must leave such a record
/// readable rather than unparseable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    /// Stopped on a question only the parent's interface can answer.
    Waiting,
    Completed,
    /// Including a clock that tripped: that is a failure with a reason, not a
    /// separate ending.
    Failed,
    /// The parent asked for it to stop.
    Stopped,
}

impl TaskStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// The same question about a status read back from a record, where an
    /// unrecognized word is something still in flight rather than an ending.
    pub fn terminal(status: &str) -> bool {
        [Self::Completed, Self::Failed, Self::Stopped].into_iter().any(|known| known.as_str() == status)
    }
}

/// Deciding "slow" from "stuck" is undecidable, so there are two clocks: one
/// that any real progress resets, and one ceiling that nothing does. Either
/// may be absent; a child that keeps producing is working.
#[derive(Clone, Copy, Default)]
pub struct Clocks {
    pub idle: Option<Duration>,
    pub ceiling: Option<Duration>,
}

impl Clocks {
    /// Which clock, if either, has run out, and how to say so.
    fn expired(self, started: Instant, progress: Instant) -> Option<(Duration, &'static str)> {
        [(self.idle, progress, "made no progress"), (self.ceiling, started, "ran")].into_iter().find_map(
            |(limit, since, what)| limit.filter(|limit| since.elapsed() > *limit).map(|limit| (limit, what)),
        )
    }
}

/// What a child produced, once it stopped producing.
#[derive(Debug, Default)]
pub struct Ended {
    pub outcome: Option<Result<String, String>>,
    /// The watchdog gave up rather than the child finishing.
    pub tripped: bool,
    /// What the delegation cost, in the child's own accounting.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

/// The two decisions the loop below refuses to make.
pub trait Observer {
    /// Validate attachment metadata before any task content is sent.
    fn session_opened(&self, _result: &Value) -> Result<(), String> {
        Ok(())
    }

    /// Record that the child is doing something, which here means writing a
    /// mesh snapshot other agents can read.
    fn progress(&self, status: TaskStatus, detail: &str);

    /// One update the child streamed, in the protocol's own words.
    fn update(&self, _session: &str, _update: &Value) {}

    /// Answer a child that asked a person something. A parent with nobody to
    /// ask cancels, which the child hears as a question declined.
    fn interaction(&self, _questions: Vec<Question>) -> impl Future<Output = InteractionResponse> {
        async { InteractionResponse::Cancel }
    }
}

/// The three requests a delegation is, numbered in the order they go out.
const INITIALIZE: u64 = 1;
const NEW_SESSION: u64 = 2;
const PROMPT: u64 = 3;

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn answer(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

async fn send<W: AsyncWrite + Unpin>(stdin: &mut W, message: Value) -> bool {
    let mut line = message.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.is_ok()
}

/// A form the child put to a person, as the questions this host asks its own
/// frontend: one field per question, its options in the enum. The field names
/// come back with the answers, so they are kept beside each question.
fn questions(params: &Value) -> Vec<(String, Question)> {
    let mut fields: Vec<(&String, &Value)> = params
        .pointer("/mode/form/requestedSchema/properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .collect();
    // `q10` must follow `q9`, whatever a map made of them.
    fields.sort_by_key(|(name, _)| name.trim_start_matches('q').parse::<usize>().unwrap_or(usize::MAX));
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    fields
        .into_iter()
        .map(|(name, field)| {
            let labels = field["enum"].as_array().into_iter().flatten();
            let descriptions = field["enumDescriptions"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(std::iter::repeat(&Value::Null));
            let question = Question {
                header: field["title"].as_str().unwrap_or(name).to_owned(),
                question: field["description"]
                    .as_str()
                    .map_or_else(|| text(&params["message"]), str::to_owned),
                options: labels
                    .zip(descriptions)
                    .map(|(label, description)| OptionItem {
                        label: text(label),
                        description: text(description),
                    })
                    .collect(),
                multi_select: false,
            };
            (name.clone(), question)
        })
        .collect()
}

/// The person's answers, in the shape the child asked for them.
fn filled(fields: &[(String, Question)], response: InteractionResponse) -> Value {
    match response {
        InteractionResponse::Answers(answers) if answers.len() == fields.len() => {
            let content: serde_json::Map<String, Value> = fields
                .iter()
                .zip(answers)
                .map(|((name, _), answer)| (name.clone(), Value::String(answer.join(", "))))
                .collect();
            json!({ "action": "accept", "content": content })
        }
        _ => json!({ "action": "cancel" }),
    }
}

/// Drive the child from its handshake to its answer.
///
/// The report is what the child said after its last tool call: the protocol
/// streams every word of a turn, and the words before a tool call are working
/// notes rather than the answer. Nothing inside elmee asks permission for
/// anything, so a child that asks — an external agent will — is told yes.
pub async fn watch<R, W, O>(
    stdout: R,
    mut stdin: W,
    cwd: &Path,
    task: &str,
    clocks: Clocks,
    observer: &O,
) -> Ended
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    O: Observer,
{
    let mut lines = BufReader::new(stdout).lines();
    let mut ended = Ended::default();
    let started = Instant::now();
    let mut progress = Instant::now();
    let mut answered = String::new();
    let mut expected_response = INITIALIZE;
    let mut session_id: Option<String> = None;
    let hello = json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
            "elicitation": { "form": {} },
        },
    });
    let _ = send(&mut stdin, request(INITIALIZE, "initialize", hello)).await;
    loop {
        let line = match tokio::time::timeout(TICK, lines.next_line()).await {
            Ok(Ok(Some(line))) => Some(line),
            Ok(_) => {
                // The stream ended before any final event: the child died
                // without saying why. Why is on its stderr, which the caller
                // appends.
                ended.outcome = Some(Err("the agent's process ended before it reported".into()));
                break;
            }
            Err(_) => None,
        };
        if let Some((limit, what)) = clocks.expired(started, progress) {
            ended.outcome = Some(Err(format!("watchdog: the agent {what} for {}s", limit.as_secs())));
            ended.tripped = true;
            break;
        }
        let Some(line) = line else { continue };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        progress = Instant::now();
        let id = message.get("id").filter(|id| !id.is_null());
        let method = message.get("method").and_then(Value::as_str);
        // Exactly one parent request is outstanding. IDs from child requests
        // are a separate namespace; only responses advance this handshake.
        if method.is_none() && id.is_some_and(|id| id.as_u64() != Some(expected_response)) {
            ended.outcome = Some(Err("unexpected or duplicate ACP response id".into()));
            ended.tripped = true;
            break;
        }
        if let Some(error) = message.get("error")
            && id.is_some()
        {
            ended.outcome = Some(Err(error["message"].as_str().unwrap_or("the agent refused").to_owned()));
            break;
        }
        match (id, method) {
            (Some(id), Some("elicitation/create")) => {
                let asked = questions(&message["params"]);
                observer.progress(TaskStatus::Waiting, "a question for the parent");
                let response =
                    observer.interaction(asked.iter().map(|(_, question)| question.clone()).collect()).await;
                observer.progress(TaskStatus::Running, "resumed");
                let _ = send(&mut stdin, answer(id, filled(&asked, response))).await;
            }
            (Some(id), Some("session/request_permission")) => {
                let allowed = message["params"]["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|option| option["kind"].as_str().is_some_and(|kind| kind.starts_with("allow")))
                    .map(|option| option["optionId"].clone())
                    .unwrap_or_default();
                let outcome = json!({ "outcome": { "outcome": "selected", "optionId": allowed } });
                let _ = send(&mut stdin, answer(id, outcome)).await;
            }
            (Some(id), Some(method)) => {
                let refused = json!({ "code": -32601, "message": method });
                let _ = send(&mut stdin, json!({ "jsonrpc": "2.0", "id": id, "error": refused })).await;
            }
            (None, Some("session/update")) => {
                if session_id
                    .as_deref()
                    .is_none_or(|owned| message["params"]["sessionId"].as_str() != Some(owned))
                {
                    continue;
                }
                let update = &message["params"]["update"];
                observer.update(session_id.as_deref().unwrap_or_default(), update);
                match update["sessionUpdate"].as_str() {
                    Some("tool_call") => {
                        answered.clear();
                        observer.progress(TaskStatus::Running, update["title"].as_str().unwrap_or("working"));
                    }
                    Some("agent_message_chunk") => {
                        answered.push_str(update["content"]["text"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            (Some(id), None) if id.as_u64() == Some(INITIALIZE) => {
                expected_response = NEW_SESSION;
                let opened = json!({ "cwd": cwd, "mcpServers": [] });
                let _ = send(&mut stdin, request(NEW_SESSION, "session/new", opened)).await;
            }
            (Some(id), None) if id.as_u64() == Some(NEW_SESSION) => {
                let result = &message["result"];
                let Some(session) = result["sessionId"].as_str().filter(|id| !id.is_empty()) else {
                    ended.outcome = Some(Err("session/new returned no valid sessionId".into()));
                    ended.tripped = true;
                    break;
                };
                if let Err(error) = observer.session_opened(result) {
                    ended.outcome = Some(Err(error));
                    ended.tripped = true;
                    break;
                }
                session_id = Some(session.to_owned());
                let prompt = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": task }] });
                expected_response = PROMPT;
                let _ = send(&mut stdin, request(PROMPT, "session/prompt", prompt)).await;
            }
            (Some(id), None) if id.as_u64() == Some(PROMPT) => {
                // What the turn cost rides in `_meta`, which is where the
                // protocol puts what it does not model. A foreign agent says
                // nothing there, and its delegation is simply unpriced.
                let spent = &message["result"]["_meta"]["elmee"];
                let count = |key: &str| spent[key].as_u64().unwrap_or(0);
                ended.input_tokens = count("inputTokens");
                ended.output_tokens = count("outputTokens");
                ended.duration_ms = count("durationMs");
                ended.outcome = Some(match message["result"]["stopReason"].as_str() {
                    Some("end_turn") => Ok(std::mem::take(&mut answered)),
                    Some("cancelled") => Err("the agent's turn was cancelled".into()),
                    Some(reason @ ("max_tokens" | "max_turn_requests" | "refusal")) => {
                        let mut error = format!("the agent did not complete the task: {reason}");
                        if !answered.is_empty() {
                            error.push_str("\nPartial report:\n");
                            error.push_str(&answered);
                        }
                        Err(error)
                    }
                    _ => {
                        ended.tripped = true;
                        Err("session/prompt returned no recognized stopReason".into())
                    }
                });
                break;
            }
            _ => {}
        }
    }
    ended
}

#[cfg(test)]
mod tests;
