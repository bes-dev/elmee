//! The clocks are exercised on a paused runtime: every threshold here is
//! virtual, so the tests cost no wall time and cannot flake on a loaded
//! machine. The child on the other end is a task speaking the protocol.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::*;

#[derive(Default)]
struct Recorder {
    reject_attachment: bool,
    attachments: std::sync::Mutex<usize>,
    progress: std::sync::Mutex<Vec<String>>,
    asked: std::sync::Mutex<Vec<Question>>,
    answer: Option<Vec<Vec<String>>>,
}

impl Observer for Recorder {
    fn session_opened(&self, _result: &Value) -> Result<(), String> {
        *self.attachments.lock().unwrap() += 1;
        if self.reject_attachment { Err("attachment refused".into()) } else { Ok(()) }
    }
    fn progress(&self, status: TaskStatus, detail: &str) {
        self.progress.lock().unwrap().push(format!("{} · {detail}", status.as_str()));
    }

    async fn interaction(&self, questions: Vec<Question>) -> InteractionResponse {
        self.asked.lock().unwrap().extend(questions);
        self.answer.clone().map_or(InteractionResponse::Cancel, InteractionResponse::Answers)
    }
}

/// Both pipes of a child: what it writes, and what it reads.
struct Pipes {
    stdout: DuplexStream,
    stdin: DuplexStream,
}

/// A child whose stdout we write and whose stdin we read, as the tests see it.
struct Child {
    out: DuplexStream,
    lines: tokio::io::Lines<BufReader<DuplexStream>>,
}

fn pipes() -> (Pipes, Child) {
    let (out, stdout) = tokio::io::duplex(1 << 16);
    let (stdin, input) = tokio::io::duplex(1 << 16);
    (Pipes { stdout, stdin }, Child { out, lines: BufReader::new(input).lines() })
}

impl Child {
    async fn say(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.out.write_all(line.as_bytes()).await.unwrap();
    }

    async fn hear(&mut self) -> Value {
        serde_json::from_str(&self.lines.next_line().await.unwrap().expect("the parent hung up")).unwrap()
    }

    async fn update(&mut self, update: Value) {
        self.say(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s", "update": update } }))
            .await;
    }

    /// Answer the handshake and the session, and return the prompt's id.
    async fn handshake(&mut self) -> Value {
        let hello = self.hear().await;
        assert_eq!(hello["method"], "initialize");
        assert_eq!(hello["params"]["clientCapabilities"]["elicitation"]["form"], json!({}));
        self.say(json!({ "jsonrpc": "2.0", "id": hello["id"], "result": { "protocolVersion": 1 } })).await;
        let opened = self.hear().await;
        assert_eq!(opened["method"], "session/new");
        assert_eq!(opened["params"]["cwd"], "/work");
        self.say(json!({ "jsonrpc": "2.0", "id": opened["id"], "result": { "sessionId": "s" } })).await;
        let prompt = self.hear().await;
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], "s");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "find it");
        prompt["id"].clone()
    }
}

async fn watched(pipes: Pipes, clocks: Clocks, observer: &Recorder) -> Ended {
    watch(pipes.stdout, pipes.stdin, Path::new("/work"), "find it", clocks, observer).await
}

#[tokio::test]
async fn only_explicit_success_is_a_completed_delegation() {
    for reason in [
        json!("end_turn"),
        json!("cancelled"),
        json!("refusal"),
        json!("max_tokens"),
        json!("max_turn_requests"),
        json!("future_reason"),
        Value::Null,
        json!(12),
    ] {
        let (pipes, mut child) = pipes();
        let recorder = Recorder::default();
        let talking = async {
            let prompt = child.handshake().await;
            child.update(json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"partial finding"}})).await;
            child.say(json!({"id":prompt,"result":{"stopReason":reason}})).await;
        };
        let (ended, ()) = tokio::join!(watched(pipes, Clocks::default(), &recorder), talking);
        let result = ended.outcome.unwrap();
        assert_eq!(result.is_ok(), reason == "end_turn", "{reason}: {result:?}");
        let recognized = reason.as_str().is_some_and(|reason| {
            ["end_turn", "cancelled", "refusal", "max_tokens", "max_turn_requests"].contains(&reason)
        });
        assert_eq!(ended.tripped, !recognized, "{reason}");
        if matches!(reason.as_str(), Some("refusal" | "max_tokens" | "max_turn_requests")) {
            assert!(result.unwrap_err().contains("Partial report:\npartial finding"));
        }
    }
}

#[tokio::test]
async fn another_sessions_updates_cannot_contaminate_or_clear_the_owned_report() {
    let (pipes, mut child) = pipes();
    let recorder = Recorder::default();
    let talking = async {
        let prompt = child.handshake().await;
        child.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"owned answer"}})).await;
        for session in [json!("other"), Value::Null] {
            for update in [
                json!({"sessionUpdate":"tool_call","title":"foreign tool"}),
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"foreign answer"}}),
            ] {
                child
                    .say(json!({"method":"session/update","params":{"sessionId":session,"update":update}}))
                    .await;
            }
        }
        child.say(json!({"id":prompt,"result":{"stopReason":"end_turn"}})).await;
    };
    let (ended, ()) = tokio::join!(watched(pipes, Clocks::default(), &recorder), talking);
    assert_eq!(ended.outcome.unwrap().unwrap(), "owned answer");
    assert!(recorder.progress.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_or_out_of_order_responses_cannot_reopen_or_rebind_a_task() {
    for (after_attachment, wrong_id) in
        [(false, NEW_SESSION), (false, PROMPT), (true, INITIALIZE), (true, NEW_SESSION)]
    {
        let (pipes, mut child) = pipes();
        let recorder = Recorder::default();
        let talking = async {
            if after_attachment {
                child.handshake().await;
            } else {
                assert_eq!(child.hear().await["method"], "initialize");
            }
            child.say(json!({"id":wrong_id, "result":{"sessionId":"replacement"}})).await;
            assert!(child.lines.next_line().await.unwrap().is_none(), "must not send another request");
        };
        let (ended, ()) = tokio::join!(watched(pipes, Clocks::default(), &recorder), talking);
        assert!(ended.tripped);
        assert_eq!(ended.outcome.unwrap().unwrap_err(), "unexpected or duplicate ACP response id");
        assert_eq!(*recorder.attachments.lock().unwrap(), usize::from(after_attachment));
    }
}

#[tokio::test]
async fn invalid_session_handshake_never_receives_the_task_prompt() {
    for session in [Value::Null, json!(17), json!(""), json!("valid-but-refused")] {
        let (pipes, mut child) = pipes();
        let recorder = Recorder { reject_attachment: true, ..Default::default() };
        let talking = async {
            let hello = child.hear().await;
            child.say(json!({"id":hello["id"], "result":{"protocolVersion":1}})).await;
            let opened = child.hear().await;
            child.say(json!({"id":opened["id"], "result":{"sessionId":session}})).await;
            assert!(child.lines.next_line().await.unwrap().is_none(), "task content must not be sent");
        };
        let (ended, ()) = tokio::join!(watched(pipes, Clocks::default(), &recorder), talking);
        assert!(ended.tripped, "the process owner must stop a refused child");
        assert!(ended.outcome.unwrap().is_err());
    }
}

/// A status read back from a record is a plain word, so the check that decides
/// whether work is still in flight is the one place the vocabulary lives.
#[test]
fn one_terminal_vocabulary_decides_what_is_still_in_flight() {
    for ending in [TaskStatus::Completed, TaskStatus::Failed, TaskStatus::Stopped] {
        assert!(TaskStatus::terminal(ending.as_str()), "{} ends a task", ending.as_str());
    }
    for live in [TaskStatus::Running, TaskStatus::Waiting] {
        assert!(!TaskStatus::terminal(live.as_str()), "{} is still moving", live.as_str());
    }
    // An unrecognized word read back from a record is work still in flight.
    for unknown in ["timed_out", "cancelled", "starting", ""] {
        assert!(!TaskStatus::terminal(unknown), "{unknown} must not end a task");
    }
}

#[tokio::test(start_paused = true)]
async fn a_child_that_says_nothing_trips_the_idle_clock() {
    let (pipes, child) = pipes();
    let clocks = Clocks { idle: Some(Duration::from_secs(60)), ceiling: Some(Duration::from_secs(3600)) };
    let ended = watched(pipes, clocks, &Recorder::default()).await;
    drop(child);
    assert!(ended.tripped);
    let error = ended.outcome.unwrap().unwrap_err();
    assert!(error.contains("made no progress for 60s"), "{error}");
}

/// The whole conversation, as a child speaking the protocol has it: the
/// report is what it said after its last tool call, and what the turn cost
/// rides in `_meta`.
#[tokio::test(start_paused = true)]
async fn a_child_is_driven_to_its_answer_and_its_last_words_are_the_report() {
    let (pipes, mut child) = pipes();
    tokio::spawn(async move {
        let prompt = child.handshake().await;
        for tool in ["read src/a.rs", "exec cargo test"] {
            tokio::time::sleep(Duration::from_secs(20)).await;
            child.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "working…" } })).await;
            child.update(json!({ "sessionUpdate": "tool_call", "toolCallId": "c", "title": tool, "status": "in_progress" })).await;
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
        child.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "found " } })).await;
        child.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "it" } })).await;
        child
            .say(json!({ "jsonrpc": "2.0", "id": prompt, "result": {
            "stopReason": "end_turn",
            "_meta": { "elmee": { "inputTokens": 9, "outputTokens": 4, "durationMs": 100000 } },
        } }))
            .await;
    });
    let recorder = Recorder::default();
    let clocks = Clocks { idle: Some(Duration::from_secs(60)), ceiling: None };
    let ended = watched(pipes, clocks, &recorder).await;
    // Sixty virtual seconds under a sixty-second idle threshold: the gaps are
    // what the clock measures, not the total.
    assert!(!ended.tripped);
    assert_eq!(ended.outcome.unwrap().unwrap(), "found it");
    assert_eq!((ended.input_tokens, ended.output_tokens, ended.duration_ms), (9, 4, 100_000));
    let progress = recorder.progress.lock().unwrap();
    assert_eq!(*progress, ["running · read src/a.rs", "running · exec cargo test"]);
}

#[tokio::test(start_paused = true)]
async fn a_ceiling_ends_a_child_that_is_still_producing() {
    let (pipes, mut child) = pipes();
    tokio::spawn(async move {
        child.handshake().await;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let mut line =
                json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s",
                "update": { "sessionUpdate": "tool_call", "toolCallId": "c", "title": "exec" } } })
                .to_string();
            line.push('\n');
            if child.out.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let clocks = Clocks { idle: Some(Duration::from_secs(60)), ceiling: Some(Duration::from_secs(30)) };
    let ended = watched(pipes, clocks, &Recorder::default()).await;
    assert!(ended.tripped);
    let error = ended.outcome.unwrap().unwrap_err();
    assert!(error.contains("ran for 30s"), "{error}");
}

/// A child that died at startup is a death, not silence: EOF before the
/// answer leaves an outcome, so the parent is told.
#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_before_an_answer_is_a_death_not_silence() {
    let (pipes, mut child) = pipes();
    tokio::spawn(async move {
        let hello = child.hear().await;
        child.say(json!({ "jsonrpc": "2.0", "id": hello["id"], "result": { "protocolVersion": 1 } })).await;
        // Then it dies.
    });
    let ended = watched(pipes, Clocks::default(), &Recorder::default()).await;
    assert!(!ended.tripped, "the child ended on its own; no clock gave up on it");
    assert!(ended.outcome.unwrap().unwrap_err().contains("ended before it reported"));
}

/// A child refusing the prompt is an error with its own words, and a turn
/// the child cancelled is not an answer.
#[tokio::test(start_paused = true)]
async fn a_refusal_is_reported_in_the_childs_words() {
    let (pipes, mut child) = pipes();
    tokio::spawn(async move {
        let prompt = child.handshake().await;
        child.say(json!({ "jsonrpc": "2.0", "id": prompt, "error": { "code": -32603, "message": "no such model" } })).await;
    });
    let ended = watched(pipes, Clocks::default(), &Recorder::default()).await;
    assert_eq!(ended.outcome.unwrap().unwrap_err(), "no such model");
}

/// A question the child puts to a person comes back as the questions this
/// host asks its own frontend, and the answer goes back in the form the child
/// asked for. Nobody inside elmee asks permission; a child that does — a
/// foreign agent will — is told yes.
#[tokio::test(start_paused = true)]
async fn a_childs_question_is_asked_of_the_parent_and_its_permission_is_granted() {
    let (pipes, mut child) = pipes();
    tokio::spawn(async move {
        let prompt = child.handshake().await;
        child.say(json!({ "jsonrpc": "2.0", "id": "ask-1", "method": "elicitation/create", "params": {
            "message": "Which?",
            "mode": { "form": { "scope": { "sessionId": "s" }, "requestedSchema": { "type": "object", "properties": {
                "q1": { "type": "string", "title": "Second", "description": "and then?", "enum": ["stop"], "enumDescriptions": ["enough"] },
                "q0": { "type": "string", "title": "First", "description": "Which?", "enum": ["a", "b"], "enumDescriptions": ["the a", "the b"] },
            } } } },
        } }))
        .await;
        let answered = child.hear().await;
        assert_eq!(answered["id"], "ask-1");
        assert_eq!(answered["result"]["action"], "accept");
        assert_eq!(answered["result"]["content"]["q0"], "b");
        assert_eq!(answered["result"]["content"]["q1"], "stop");

        child
            .say(
                json!({ "jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission", "params": {
            "sessionId": "s", "toolCall": { "toolCallId": "c" },
            "options": [
                { "optionId": "no", "name": "Reject", "kind": "reject_once" },
                { "optionId": "yes", "name": "Allow", "kind": "allow_always" },
            ],
        } }),
            )
            .await;
        let allowed = child.hear().await;
        assert_eq!(allowed["result"]["outcome"]["outcome"], "selected");
        assert_eq!(allowed["result"]["outcome"]["optionId"], "yes");

        child.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "done" } })).await;
        child.say(json!({ "jsonrpc": "2.0", "id": prompt, "result": { "stopReason": "end_turn" } })).await;
    });
    let recorder =
        Recorder { answer: Some(vec![vec!["b".into()], vec!["stop".into()]]), ..Recorder::default() };
    let ended = watched(pipes, Clocks::default(), &recorder).await;
    assert_eq!(ended.outcome.unwrap().unwrap(), "done");
    let asked = recorder.asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].header, "First", "fields come in their numbered order, whatever a map made of them");
    assert_eq!(asked[0].question, "Which?");
    assert_eq!(asked[0].options[1].label, "b");
    assert_eq!(asked[0].options[1].description, "the b");
    assert_eq!(asked[1].header, "Second");
    let progress = recorder.progress.lock().unwrap();
    assert_eq!(*progress, ["waiting · a question for the parent", "running · resumed"]);
}
