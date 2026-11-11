//! The protocol as a client meets it, against the real binary.
//!
//! The plugin's own tests cover translation — what a tool call looks like on
//! the wire — because that is a pure function of an event. What they cannot
//! cover is the part a client actually depends on: that a request is answered
//! exactly once, that an id handed out can be handed back, that a session is
//! usable after a cancelled turn. Those are properties of a running process
//! and only a running process can be asked.
//!
//! A stub provider stands in for the model, so this needs no network, no
//! credentials and no patience.
//!
//! Everything the agent sends is also checked against the published schema,
//! vendored under `fixtures/` — version 1 with its unstable additions, as
//! `@agentclientprotocol/sdk` 2.5.0 ships it: a notification the editor
//! cannot parse is a grey box at best, and the specification moves every few
//! weeks.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, OnceLock};

#[path = "acp/embedding.rs"]
mod embedding;
#[path = "acp/lifecycle.rs"]
mod lifecycle;

/// The schema, compiled once per definition it is asked about.
fn validator(definition: &str) -> &'static jsonschema::Validator {
    static COMPILED: OnceLock<Mutex<HashMap<String, &'static jsonschema::Validator>>> = OnceLock::new();
    let mut compiled = COMPILED.get_or_init(Mutex::default).lock().unwrap();
    compiled.entry(definition.to_owned()).or_insert_with(|| {
        let mut schema: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/acp-schema.json")).expect("the schema is JSON");
        // The root is "any message either side may send"; a definition is one
        // thing in particular, and the root's own branches are what the
        // agent's messages are checked against.
        let root = schema.as_object_mut().and_then(|schema| schema.remove("anyOf")).expect("a root");
        match definition {
            "Agent" => schema["anyOf"] = serde_json::json!([root[0]]),
            name => schema["$ref"] = serde_json::Value::String(format!("#/$defs/{name}")),
        }
        Box::leak(Box::new(jsonschema::validator_for(&schema).expect("the schema compiles")))
    })
}

/// Fail the test on the first message the published schema would refuse.
fn conforms(definition: &str, instance: &serde_json::Value) {
    let complaints: Vec<String> =
        validator(definition).iter_errors(instance).map(|error| error.to_string()).collect();
    assert!(complaints.is_empty(), "not a valid {definition}: {complaints:?}\n{instance}");
}

/// A provider that answers every completion with the same words. Unlike the
/// one-shot stub in `cli.rs` this serves for as long as the process lives: an
/// ACP session takes as many turns as the client asks for.
fn stub_provider(answer: &str) -> String {
    slow_provider(answer, 0)
}

/// The same, but taking its time — long enough for a client to change its mind
/// mid-turn, which is the only way to test what happens when it does.
fn slow_provider(answer: &str, think_for: u64) -> String {
    let body = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": answer}}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 2},
    })
    .to_string();
    serve_json(body, think_for)
}

/// Answer every completion request with the same body, after the same wait.
fn serve_json(body: String, think_for: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().expect("an address").to_string();
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let _ = drain_request(&mut socket);
                std::thread::sleep(std::time::Duration::from_millis(think_for));
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    address
}

fn drain_request(socket: &mut TcpStream) -> String {
    socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut seen = Vec::new();
    let mut byte = [0_u8; 1];
    let mut length = 0_usize;
    while socket.read(&mut byte).unwrap_or(0) == 1 {
        seen.push(byte[0]);
        if seen.ends_with(b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&seen).to_lowercase();
            length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            break;
        }
    }
    let mut body = vec![0_u8; length];
    let _ = socket.read_exact(&mut body);
    String::from_utf8_lossy(&body).into_owned()
}

/// A model that answers by asking, so a turn stops on a question instead of
/// finishing. The turn never gets past it, so one canned answer is enough.
fn asking_provider() -> String {
    let questions = serde_json::json!({
        "questions": [{
            "question": "which one?",
            "header": "Pick",
            "options": [{ "label": "a", "description": "a" }, { "label": "b", "description": "b" }],
        }]
    })
    .to_string();
    let call = serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [{
            "id": "q1",
            "type": "function",
            "function": { "name": "ask_user", "arguments": questions },
        }] } }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 2 },
    });
    serve_json(call.to_string(), 0)
}

/// A model that plays out a short script by looking at what it is asked.
///
/// Two turns need a tool call rather than prose: one to find out who else is
/// working in this project, one to write to them. Each is issued once — the
/// marker is its own result coming back in the history.
fn scripted_provider() -> String {
    serve_scripted()
}

/// `served` counts this provider's own requests, so a script can answer the
/// same words differently the second time without sharing state between the
/// tests that run beside it.
fn answer_for(request: &str, served: usize) -> serde_json::Value {
    if request.contains("steering correction received") {
        return said("adjusted");
    }
    let call = |name: &str, arguments: String| {
        serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": { "name": name, "arguments": arguments },
            }] } }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 2 },
        })
    };
    // What the client told the model to be reaches the model, beside the
    // agent's own instructions or in their place.
    if request.contains("answer as a pirate") {
        return said(if request.contains("You are a coding agent in elmee") {
            "appended"
        } else {
            "replaced"
        });
    }
    // A tool the client itself serves is called like any other.
    if request.contains("ask the editor") {
        return match request.contains("the selected text") {
            true => said("the editor holds the selected text"),
            // Under whatever name the client gave its server.
            false => {
                let offered = |word: &&str| word.starts_with("mcp__") && word.ends_with("__selection");
                call(request.split(['"', '\\']).find(offered).unwrap_or_default(), "{}".into())
            }
        };
    }
    if request.contains("delegate this") && !request.contains("\\\"status\\\":\\\"running\\\"") {
        let task = serde_json::json!({ "name": "helper", "task": "child task: report the finding" });
        return call("spawn_agent", task.to_string());
    }
    if request.contains(SCRIPTED_TURN) {
        // Asked once. What an answer looks like in the transcript is the
        // agent's business, so the script counts instead of reading it.
        if served > 0 {
            return said("the person chose b");
        }
        let questions = serde_json::json!({
            "questions": [{
                "question": "which one?",
                "header": "Pick",
                "options": [
                    { "label": "a", "description": "the first" },
                    { "label": "b", "description": "the second" },
                ],
            }]
        });
        return call("ask_user", questions.to_string());
    }
    if request.contains("background_task_completed") {
        return said("the helper reported: done");
    }
    if request.contains("child task") && !request.contains("delegate this") {
        return said("the child found it");
    }
    if request.contains("roll call") && !request.contains("\\\"self\\\"") {
        return call("wait_agents", serde_json::json!({ "timeout_ms": 0 }).to_string());
    }
    if let Some(peer) = named_after("poke ", request)
        && !request.contains(PEER_MESSAGE)
    {
        return call("send_to_agent", serde_json::json!({ "to": peer, "message": PEER_MESSAGE }).to_string());
    }
    said("ok")
}

fn said(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 2 },
    })
}

const PEER_MESSAGE: &str = "the build is green, note it";

/// The prompt whose answer changes once the person has been asked.
const SCRIPTED_TURN: &str = "pick one";

/// The agent name written right after `marker`, as the prompt spells it.
fn named_after(marker: &str, text: &str) -> Option<String> {
    let rest = text.rsplit(marker).next()?;
    let name: String = rest.chars().take_while(|c: &char| c.is_ascii_alphanumeric() || *c == '-').collect();
    name.starts_with("agent-").then_some(name)
}

fn serve_scripted() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().expect("an address").to_string();
    let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { continue };
            let calls = std::sync::Arc::clone(&served);
            std::thread::spawn(move || {
                let request = drain_request(&mut socket);
                // Only the scripted turns are counted: a session asks this
                // provider other things too, and a rule that counted those
                // would answer the second turn on the first one's number.
                let served = match request.contains(SCRIPTED_TURN) {
                    true => calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                    false => 0,
                };
                let body = answer_for(&request, served).to_string();
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    address
}

/// A client that speaks the protocol over the binary's own stdio.
struct Client {
    process: Child,
    /// Taken away to hang up on the agent, which is how an editor that dies
    /// looks from this side.
    input: Option<ChildStdin>,
    /// Lines arrive on a channel so a message that never comes fails the test
    /// instead of hanging it.
    output: std::sync::mpsc::Receiver<String>,
    next: i64,
    /// Answers that arrived while waiting for a different request — two
    /// requests can be in flight, and the second may finish first.
    stashed: Vec<serde_json::Value>,
    /// What each outstanding request asked, so its answer can be checked
    /// against the shape the schema gives that method's result.
    asked: HashMap<i64, Option<&'static str>>,
}

/// The result each request is answered with, by the schema's name for it.
/// The schema a method's successful result must satisfy, when the published
/// schema names one. It does not name every method this agent answers — the
/// renamed authentication calls are absent from this revision — and a result
/// with nothing in it cannot be held to the envelope's schema, so an unknown
/// method's answer is checked as an envelope only.
fn response_of(method: &str) -> Option<&'static str> {
    Some(match method {
        "initialize" => "InitializeResponse",
        // Two spellings of each: the protocol renamed them, and this agent
        // answers both with what the older names promise.
        "authenticate" | "auth/login" => "AuthenticateResponse",
        "logout" | "auth/logout" => "LogoutResponse",
        "session/new" => "NewSessionResponse",
        "session/fork" => "ForkSessionResponse",
        "session/set_title" => "SetSessionTitleResponse",
        "session/status" => "SessionStatusResponse",
        "auth/status" => "AuthStatusResponse",
        "session/load" => "LoadSessionResponse",
        "session/resume" => "ResumeSessionResponse",
        "session/prompt" => "PromptResponse",
        "session/list" => "ListSessionsResponse",
        "session/close" => "CloseSessionResponse",
        "session/delete" => "DeleteSessionResponse",
        "session/set_config_option" => "SetSessionConfigOptionResponse",
        _ => return None,
    })
}

impl Client {
    fn start(home: &std::path::Path, work: &std::path::Path, address: &str, extra: &[&str]) -> Self {
        Self::start_with_mesh(home, work, address, extra, None)
    }

    fn start_with_mesh(
        home: &std::path::Path,
        work: &std::path::Path,
        address: &str,
        extra: &[&str],
        mesh: Option<&std::path::Path>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_elmee"));
        if let Some(root) = mesh {
            command.env("ELMEE_MESH_ROOT", root);
        } else {
            command.env_remove("ELMEE_MESH_ROOT");
        }
        let mut process = command
            .args(["--acp", "-m", "ollama/stub"])
            .args(extra)
            .env("HOME", home)
            .env_remove("ELMEE_MODEL")
            .env("ELMEE_BASE_URL", format!("http://{address}/v1"))
            .env("ELMEE_API_KEY", "stub")
            .current_dir(work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("elmee runs");
        let input = process.stdin.take().expect("stdin");
        let (lines, output) = std::sync::mpsc::channel();
        let stdout = process.stdout.take().expect("stdout");
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        Self { process, input: Some(input), output, next: 0, stashed: Vec::new(), asked: HashMap::new() }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&message);
    }

    /// Answer a request the agent sent us. An editor does this for every
    /// question; no test could do it before, so the answered half of asking
    /// went unchecked.
    fn respond(&mut self, id: &serde_json::Value, result: serde_json::Value) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        self.write(&message);
    }

    /// Read until the agent asks us something, and hand back the request.
    fn until_asked(&mut self, method: &str) -> serde_json::Value {
        loop {
            let message = self.read();
            if message.get("method").and_then(|it| it.as_str()) == Some(method) {
                return message;
            }
        }
    }

    fn write(&mut self, message: &serde_json::Value) {
        let input = self.input.as_mut().expect("still connected");
        writeln!(input, "{message}").expect("the agent is listening");
    }

    /// Close stdin and wait, as the operating system does for an editor that
    /// exits or crashes. `None` if the agent is still running.
    fn hang_up(&mut self, patience: std::time::Duration) -> Option<std::process::ExitStatus> {
        self.input = None;
        let deadline = std::time::Instant::now() + patience;
        while std::time::Instant::now() < deadline {
            if let Some(status) = self.process.try_wait().expect("the agent is ours") {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    /// Send a request without waiting for it, so a second one can be sent
    /// while the first is still running.
    fn send(&mut self, method: &str, params: serde_json::Value) -> i64 {
        self.next += 1;
        let id = self.next;
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write(&message);
        self.asked.insert(id, response_of(method));
        id
    }

    fn wait(&mut self, id: i64) -> (serde_json::Value, Vec<String>) {
        let (answer, updates) = self.wait_full(id);
        let kinds =
            updates.iter().filter_map(|update| update["sessionUpdate"].as_str().map(str::to_owned)).collect();
        (answer, kinds)
    }

    /// Read until one request's answer, collecting the notifications that
    /// arrive first — which is the order a real client sees them in.
    /// The commands a session offers, which arrive right after it opens:
    /// once the client has an id to file them under.
    fn offered(&mut self) -> Vec<String> {
        loop {
            let message = self.read();
            if let Some(commands) = message.pointer("/params/update/availableCommands") {
                return commands
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|command| command["name"].as_str().map(str::to_owned))
                    .collect();
            }
        }
    }

    fn wait_full(&mut self, id: i64) -> (serde_json::Value, Vec<serde_json::Value>) {
        if let Some(at) = self
            .stashed
            .iter()
            .position(|message| message.get("id").and_then(serde_json::Value::as_i64) == Some(id))
        {
            return (self.stashed.remove(at), Vec::new());
        }
        let mut updates = Vec::new();
        loop {
            let message = self.read();
            match message.pointer("/params/update/sessionUpdate").and_then(|it| it.as_str()) {
                Some(_) => updates.push(message["params"]["update"].clone()),
                None if message.get("id").and_then(serde_json::Value::as_i64) == Some(id) => {
                    return (message, updates);
                }
                None if message.get("id").is_some() && message.get("method").is_none() => {
                    self.stashed.push(message);
                }
                None => {}
            }
        }
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> (serde_json::Value, Vec<String>) {
        let id = self.send(method, params);
        self.wait(id)
    }

    /// Every message the agent sends is held to the published schema: the
    /// envelope and, for a successful answer, the result the method promises.
    fn read(&mut self) -> serde_json::Value {
        let line = self
            .output
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the agent said nothing for 30s");
        let message: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|error| panic!("not JSON-RPC: {line} ({error})"));
        conforms("Agent", &message);
        if let Some(id) = message.get("id").and_then(serde_json::Value::as_i64)
            && let Some(Some(definition)) = self.asked.remove(&id)
            && let Some(result) = message.get("result")
        {
            conforms(definition, result);
        }
        message
    }

    fn hello(&mut self) -> serde_json::Value {
        self.hello_showing(serde_json::json!({ "terminal": false }))
    }

    /// The handshake of a client that can show more than the minimum.
    fn hello_showing(&mut self, capabilities: serde_json::Value) -> serde_json::Value {
        let (answer, _) = self.call(
            "initialize",
            serde_json::json!({ "protocolVersion": 1, "clientCapabilities": capabilities }),
        );
        answer["result"].clone()
    }

    fn open(&mut self, cwd: &std::path::Path) -> String {
        let (answer, _) = self.call("session/new", serde_json::json!({ "cwd": cwd, "mcpServers": [] }));
        answer["result"]["sessionId"].as_str().expect("a session id").to_owned()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn scratch(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("elmee-acp-{name}-{}", std::process::id()));
    let (home, work) = (root.join("home"), root.join("work"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    (home, work)
}

/// The handshake is the one exchange every client performs, and the only one
/// that decides whether the rest is even attempted.
#[test]
fn the_handshake_negotiates_down_and_states_what_this_agent_can_do() {
    let (home, work) = scratch("hello");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);

    let ours = client.hello();
    assert_eq!(ours["protocolVersion"], 1);
    assert_eq!(ours["agentInfo"]["name"], "elmee");
    assert_eq!(ours["authMethods"][0]["id"], "codex", "the one account a client can sign this agent in to");
    assert_eq!(ours["agentCapabilities"]["loadSession"], true);

    // A client ahead of us is answered in our version, one behind in its own.
    let (newer, _) = client.call("initialize", serde_json::json!({ "protocolVersion": 99 }));
    assert_eq!(newer["result"]["protocolVersion"], 1);
    let (older, _) = client.call("initialize", serde_json::json!({ "protocolVersion": 0 }));
    assert_eq!(older["result"]["protocolVersion"], 0);
}

/// A client that signs in anyway is answered rather than refused.
///
/// The handshake offers one way to sign in, and editors ask with whatever
/// they remembered regardless — on connect, on reconnect, and on sign-out —
/// in three spellings across protocol versions.
/// An agent that answers "method not found" to any of them is one an editor
/// can decide it failed to connect to, so all three are answered, and the
/// session that was already open is untouched by the exchange.
#[test]
fn a_client_that_signs_in_anyway_is_answered_and_keeps_working() {
    let (home, work) = scratch("auth");
    let mut client = Client::start(&home, &work, &stub_provider("signed in"), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    for (method, params) in [
        ("authenticate", serde_json::json!({})),
        ("authenticate", serde_json::json!({ "methodId": "whatever-the-editor-remembered" })),
        ("auth/login", serde_json::json!({ "methodId": "whatever-the-editor-remembered" })),
        ("logout", serde_json::json!({})),
        ("auth/logout", serde_json::json!({})),
    ] {
        let (answer, _) = client.call(method, params);
        assert!(answer.get("error").is_none(), "{method} was refused: {answer}");
        assert_eq!(answer["result"], serde_json::json!({}), "{method} claimed something: {answer}");
    }

    let (done, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(done["result"]["stopReason"], "end_turn", "the session survived signing in: {done}");
}

/// A configuration option refuses what it cannot set, and stays usable.
///
/// These are the controls an editor draws, so what it sends is whatever its
/// own menu was built from — an option this agent does not have, a switch with
/// no value attached, a level from a different model's vocabulary. Each is one
/// refusal that names the reason, and none of them leaves the option stuck.
#[test]
fn a_config_option_refuses_what_it_cannot_set_without_losing_the_session() {
    let (home, work) = scratch("config-refusals");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let session = client.open(&work);
    let set = |client: &mut Client, body: serde_json::Value| client.call("session/set_config_option", body).0;

    let unknown = set(
        &mut client,
        serde_json::json!({ "sessionId": session, "configId": "temperature", "value": "0.7" }),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    assert!(unknown["error"]["message"].as_str().unwrap().contains("unknown config option"), "{unknown}");

    let empty = set(&mut client, serde_json::json!({ "sessionId": session, "configId": "model" }));
    assert_eq!(empty["error"]["code"], -32602, "{empty}");
    assert!(empty["error"]["message"].as_str().unwrap().contains("value is required"), "{empty}");

    let nonsense = set(
        &mut client,
        serde_json::json!({ "sessionId": session, "configId": "reasoning_effort", "value": "ludicrous" }),
    );
    assert_eq!(nonsense["error"]["code"], -32602, "{nonsense}");
    assert!(
        nonsense["error"]["message"].as_str().unwrap().contains("invalid reasoning effort"),
        "{nonsense}"
    );

    let gone = set(
        &mut client,
        serde_json::json!({ "sessionId": "made-up", "configId": "reasoning_effort", "value": "high" }),
    );
    assert_eq!(gone["error"]["code"], -32602, "{gone}");

    // Nothing above was applied, and the option still takes a value it knows.
    let accepted = set(
        &mut client,
        serde_json::json!({ "sessionId": session, "configId": "reasoning_effort", "value": "high" }),
    );
    let options = accepted["result"]["configOptions"].as_array().expect("the options come back");
    let effort = options.iter().find(|option| option["id"] == "reasoning_effort").expect("the effort option");
    assert_eq!(effort["currentValue"], "high", "{accepted}");
    let model = options.iter().find(|option| option["id"] == "model").expect("the model option");
    assert_eq!(model["currentValue"], "ollama/stub", "a refused switch changed nothing: {accepted}");
}

/// One turn, from the client's side: the notifications it needs to draw
/// anything, and exactly one answer at the end.
#[test]
fn a_turn_streams_updates_and_ends_with_one_answer() {
    let (home, work) = scratch("turn");
    let mut client = Client::start(&home, &work, &stub_provider("stub-answered"), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    let (answer, updates) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn");
    assert!(updates.contains(&"agent_message_chunk".to_owned()), "the answer was streamed: {updates:?}");
    assert!(updates.contains(&"session_info_update".to_owned()), "the first prompt named it: {updates:?}");
    assert!(answer.get("error").is_none());

    // The session is still there afterwards, and says so.
    let (listed, _) = client.call("session/list", serde_json::json!({}));
    let sessions = listed["result"]["sessions"].as_array().expect("a list");
    assert!(sessions.iter().any(|open| open["sessionId"] == session.as_str()));
}

/// A session id is a promise: hand it back and get the conversation. It was
/// once a name of this process's own making, so a client that saved one was
/// given a new, empty session under it — and had no way to tell.
#[test]
fn an_id_handed_out_can_be_handed_back() {
    let (home, work) = scratch("reopen");
    let address = stub_provider("remembered");

    let session = {
        let mut client = Client::start(&home, &work, &address, &[]);
        client.hello();
        let session = client.open(&work);
        assert!(std::path::Path::new(&session).is_absolute(), "a session that has a file is that file");
        client.call(
            "session/prompt",
            serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "remember" }] }),
        );
        session
    };

    let mut reopened = Client::start(&home, &work, &address, &[]);
    reopened.hello();
    let (answer, updates) =
        reopened.call("session/load", serde_json::json!({ "sessionId": session, "cwd": work }));
    assert!(answer.get("error").is_none(), "{answer}");
    assert!(updates.contains(&"user_message_chunk".to_owned()), "what was said came back: {updates:?}");
    assert!(updates.contains(&"agent_message_chunk".to_owned()), "{updates:?}");

    // A path that is not a transcript is refused rather than started empty.
    let (missing, _) = reopened
        .call("session/load", serde_json::json!({ "sessionId": work.join("nothing.jsonl"), "cwd": work }));
    assert!(missing["error"]["message"].as_str().is_some_and(|why| why.contains("no session")), "{missing}");
}

/// Every request is answered exactly once, including the ones that are wrong.
/// A client that gets silence has no way to recover: it is still waiting.
#[test]
fn nothing_a_client_sends_goes_unanswered() {
    let (home, work) = scratch("errors");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);

    // Before the handshake, session work is refused rather than attempted.
    let (early, _) = client.call("session/new", serde_json::json!({ "cwd": work }));
    assert_eq!(early["error"]["code"], -32600, "{early}");
    client.hello();

    let (unknown, _) = client.call("nonsense/method", serde_json::json!({}));
    assert_eq!(unknown["error"]["code"], -32601);

    let (no_cwd, _) = client.call("session/new", serde_json::json!({}));
    assert_eq!(no_cwd["error"]["code"], -32602);

    let (nowhere, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": "made-up", "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(nowhere["error"]["code"], -32602, "{nowhere}");

    // A prompt with no session named at all is refused the same way, rather
    // than being attributed to whichever session happens to be open.
    let (unaddressed, _) =
        client.call("session/prompt", serde_json::json!({ "prompt": [{ "type": "text", "text": "hi" }] }));
    assert_eq!(unaddressed["error"]["code"], -32602, "{unaddressed}");

    // Ending is asymmetric on purpose. Closing something already gone is what
    // a client does when it lost track of a buffer, and answering it with a
    // failure would make an editor report an error for tidying up; deleting a
    // conversation that is not there removed nothing, and says so.
    let (closed_twice, _) = client.call("session/close", serde_json::json!({ "sessionId": "made-up" }));
    assert_eq!(closed_twice["result"], serde_json::json!({}), "{closed_twice}");
    let (never_written, _) = client
        .call("session/delete", serde_json::json!({ "sessionId": work.join("never.jsonl"), "cwd": work }));
    assert_eq!(never_written["error"]["code"], -32602, "{never_written}");

    // A notification carries no id and takes no answer, but must not derail
    // the connection either.
    client.notify("session/cancel", serde_json::json!({ "sessionId": "made-up" }));
    let (still_here, _) = client.call("session/list", serde_json::json!({}));
    assert!(still_here["result"]["sessions"].is_array(), "the connection survived: {still_here}");
}

/// Closing a session releases what it held; asking about it afterwards is an
/// ordinary refusal rather than a hang.
#[test]
fn a_closed_session_is_gone_and_says_so() {
    let (home, work) = scratch("close");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    let (closed, _) = client.call("session/close", serde_json::json!({ "sessionId": session }));
    assert!(closed.get("error").is_none(), "{closed}");
    let (listed, _) = client.call("session/list", serde_json::json!({}));
    assert!(listed["result"]["sessions"].as_array().unwrap().is_empty(), "{listed}");

    let (gone, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(gone["error"]["code"], -32602);
}

/// One process, several sessions: the property the whole design exists for.
/// Each is its own agent, and closing one leaves the others working.
#[test]
fn one_process_serves_several_sessions() {
    let (home, work) = scratch("many");
    let other = work.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();

    let first = client.open(&work);
    let second = client.open(&other);
    assert_ne!(first, second);
    let (listed, _) = client.call("session/list", serde_json::json!({}));
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 2);

    client.call("session/close", serde_json::json!({ "sessionId": first }));
    let (answer, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": second, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn", "the other session is unaffected");
}

#[test]
fn a_closed_acp_session_can_be_deleted_and_an_open_one_cannot() {
    let (home, work) = scratch("delete-session");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &[]);
    client.hello();
    let session = client.open(&work);
    assert!(!std::path::Path::new(&session).exists(), "nothing said yet, so nothing kept");
    let said = serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] });
    client.call("session/prompt", said);
    assert!(std::path::Path::new(&session).is_file());
    let (denied, _) = client.call("session/delete", serde_json::json!({"sessionId":session}));
    assert!(denied.get("error").is_some());
    assert!(std::path::Path::new(&session).is_file());
    let (closed, _) = client.call("session/close", serde_json::json!({"sessionId":session}));
    assert!(closed.get("result").is_some());
    let (deleted, _) = client.call("session/delete", serde_json::json!({"sessionId":session}));
    assert!(deleted.get("result").is_some(), "{deleted}");
    assert!(!std::path::Path::new(&session).exists());
    let (again, _) = client.call("session/delete", serde_json::json!({"sessionId":session}));
    assert!(again.get("error").is_some(), "there is nothing left to delete");
    let (listed, _) = client.call("session/list", serde_json::json!({"cwd":work}));
    assert!(!listed["result"]["sessions"].as_array().unwrap().iter().any(|s| s["sessionId"] == session));
}

/// Changing your mind mid-turn. The turn stops, says why, and the session is
/// left usable — a cancelled turn is not a broken one.
#[test]
fn a_cancelled_turn_stops_and_leaves_the_session_working() {
    let (home, work) = scratch("cancel");
    let mut client = Client::start(&home, &work, &slow_provider("ok", 3_000), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    let running = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "think" }] }),
    );
    client.notify("session/cancel", serde_json::json!({ "sessionId": session }));
    let (answer, _) = client.wait(running);
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");

    let (again, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(again["result"]["stopReason"], "end_turn", "the session took another turn");
}

/// One turn at a time, as the protocol says. The refusal must not cost the
/// running turn its own answer.
#[test]
fn a_second_prompt_is_refused_without_disturbing_the_first() {
    let (home, work) = scratch("busy");
    let mut client = Client::start(&home, &work, &slow_provider("ok", 1_500), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    let first = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "one" }] }),
    );
    let second = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "two" }] }),
    );
    let (refused, _) = client.wait(second);
    assert_eq!(refused["error"]["code"], -32600, "{refused}");
    let (answered, _) = client.wait(first);
    assert_eq!(answered["result"]["stopReason"], "end_turn", "{answered}");
}

/// An editor that dies takes its agent with it. A session left open at the end
/// of a connection once kept the process alive forever: its drain task held
/// the session pool, and the pool held the agent whose events that task was
/// waiting for. An orphan like that still owns MCP child processes, and only
/// ever accumulates.
#[test]
fn an_open_session_does_not_outlive_the_client() {
    let (home, work) = scratch("orphan");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    client.open(&work);

    let stopped = client.hang_up(std::time::Duration::from_secs(10));
    assert!(stopped.is_some(), "the agent outlived the editor that was driving it");
}

/// The same, with a question in flight. Nobody is left who could answer it, so
/// waiting for one is waiting forever.
#[test]
fn an_unanswered_question_does_not_outlive_the_client() {
    let (home, work) = scratch("asking");
    let mut client = Client::start(&home, &work, &asking_provider(), &["--no-session"]);
    let (hello, _) = client.call(
        "initialize",
        serde_json::json!({ "protocolVersion": 1, "clientCapabilities": { "elicitation": { "form": {} } } }),
    );
    assert_eq!(hello["result"]["protocolVersion"], 1);
    let session = client.open(&work);

    client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    loop {
        let message = client.read();
        if message.get("method").and_then(|it| it.as_str()) == Some("elicitation/create") {
            break;
        }
    }

    let stopped = client.hang_up(std::time::Duration::from_secs(10));
    assert!(stopped.is_some(), "the agent waited for an answer nobody could give");
}

/// A question the client answers is carried back into the turn.
///
/// Only the abandoned half of asking was covered: a question nobody answers.
/// This is the half an editor actually performs — the form it draws, the
/// choice it sends, and the turn continuing on that choice rather than on a
/// guess.
#[test]
fn an_answered_question_carries_the_choice_back_into_the_turn() {
    let (home, work) = scratch("answering");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.call(
        "initialize",
        serde_json::json!({ "protocolVersion": 1, "clientCapabilities": { "elicitation": { "form": {} } } }),
    );
    let session = client.open(&work);

    let id = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "pick one" }] }),
    );
    let asked = client.until_asked("elicitation/create");
    let form = &asked["params"]["mode"]["form"];
    let schema = &form["requestedSchema"]["properties"]["q0"];
    assert_eq!(
        schema["enum"],
        serde_json::json!(["a", "b"]),
        "an editor draws the options it was given: {asked}"
    );
    assert_eq!(schema["title"], "Pick", "the question keeps its header: {asked}");
    assert_eq!(form["scope"]["sessionId"], serde_json::Value::String(session.clone()));

    client.respond(&asked["id"], serde_json::json!({ "action": "accept", "content": { "q0": "b" } }));
    let (answer, updates) = client.wait_full(id);
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(
        updates.iter().any(|update| update.to_string().contains("chose b")),
        "the choice reached the model: {updates:?}",
    );
}

/// A form that comes back half-filled is a cancellation, not half an answer.
///
/// The loop takes one answer per question in order. A client that sends fewer
/// — a form closed early, a field the person skipped — must not have the gaps
/// filled in by position, which would attribute a choice nobody made.
#[test]
fn a_partly_filled_form_cancels_rather_than_inventing_the_rest() {
    let (home, work) = scratch("half-answered");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.call(
        "initialize",
        serde_json::json!({ "protocolVersion": 1, "clientCapabilities": { "elicitation": { "form": {} } } }),
    );
    let session = client.open(&work);

    let id = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "pick one" }] }),
    );
    let asked = client.until_asked("elicitation/create");
    client.respond(&asked["id"], serde_json::json!({ "action": "accept", "content": {} }));

    let (answer, _) = client.wait(id);
    assert!(answer.get("error").is_none(), "the turn ended rather than waiting on a form: {answer}");
    let (still_here, _) = client.call("session/list", serde_json::json!({}));
    assert!(still_here["result"]["sessions"].is_array(), "the session survived: {still_here}");
}

/// A client that cannot ask is not asked.
///
/// Elicitation is a capability, and a client that never declared it has no way
/// to draw a form. Sending it one would hang the turn on an answer that cannot
/// arrive, so the question is refused where it is raised and the turn ends.
#[test]
fn a_client_without_forms_is_never_left_waiting_on_one() {
    let (home, work) = scratch("no-forms");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    let (answer, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "pick one" }] }),
    );
    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "the turn ended without a form: {answer}");
}

/// Two ways of spelling one directory are one project.
///
/// Everything a project owns — the mesh its agents find each other through,
/// its inboxes, its session files — is keyed by the working directory. A
/// terminal keys it by the path the kernel reports, with no symlinks left in
/// it; a client sends the path a person typed. When those differ, an editor's
/// session and a terminal's in the same directory become two projects that
/// cannot see each other, and neither has any way to notice.
#[cfg(unix)]
#[test]
fn one_directory_spelled_two_ways_is_one_project() {
    let (home, work) = scratch("symlink");
    let real = work.join("project");
    let link = work.join("by-another-name");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // Started in the real directory, as a terminal would be, so the agent it
    // warms up belongs to the same project the client is about to name.
    let mut client = Client::start(&home, &real, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    client.open(&real);
    client.open(&link);

    let projects: Vec<_> = std::fs::read_dir(home.join(".elmee/projects"))
        .expect("a project directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(projects.len(), 1, "one directory became {} projects: {projects:?}", projects.len());
}

#[test]
fn a_session_speaks_when_a_peer_writes_to_it() {
    let (home, work) = scratch("peers");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.hello();
    let (first, second) = (client.open(&work), client.open(&work));

    // An agent publishes itself when it starts working, so both must work once
    // before either can be found.
    for session in [&first, &second] {
        client.call(
            "session/prompt",
            serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hello" }] }),
        );
    }

    let asked = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": first, "prompt": [{ "type": "text", "text": "roll call" }] }),
    );
    let (_, updates) = client.wait_full(asked);
    let roster = updates
        .iter()
        .filter_map(|update| update.pointer("/content/0/content/text").and_then(|it| it.as_str()))
        .find(|text| text.contains("agents"))
        .expect("the roster came back");
    let peer = named_after("\"name\":\"", roster)
        .unwrap_or_else(|| panic!("the other session is not in the mesh: {roster}"));

    client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": first, "prompt": [{ "type": "text", "text": format!("poke {peer}") }] }),
    );

    // Nothing is sent to the second session from here on. What it says, it
    // says because a peer wrote to it.
    loop {
        let message = client.read();
        let spoke = message["params"]["sessionId"] == second.as_str()
            && message.pointer("/params/update/sessionUpdate").and_then(|it| it.as_str())
                == Some("agent_message_chunk");
        if spoke {
            return;
        }
    }
}

/// The same hook on the path an editor actually takes: a session ends when the
/// client closes it, not when the process does. One warm process serves many
/// sessions, so a hook that only fired at exit would fire once for a day's
/// work and name none of it.
#[test]
fn closing_a_session_runs_the_project_hook_for_that_session() {
    let (home, work) = scratch("session-hook");
    let seen = work.join("seen.json");
    std::fs::create_dir_all(work.join(".elmee")).unwrap();
    std::fs::write(
        work.join(".elmee/hooks.json"),
        format!(r#"{{"after_session":[{{"command":"sh","args":["-c","cat > {}"]}}]}}"#, seen.display()),
    )
    .unwrap();

    let mut client = Client::start(&home, &work, &stub_provider("ok"), &[]);
    client.hello();
    let session = client.open(&work);
    let said = serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] });
    client.call("session/prompt", said);
    client.call("session/close", serde_json::json!({ "sessionId": session }));

    // The close is answered before the hook runs, so a client is never held
    // waiting on someone's script — which is also why this waits for the file.
    // The file exists before `cat` has finished filling it, so it is read
    // until it parses rather than as soon as it appears.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let payload: serde_json::Value = loop {
        if let Ok(payload) = std::fs::read(&seen).and_then(|bytes| Ok(serde_json::from_slice(&bytes)?)) {
            break payload;
        }
        assert!(std::time::Instant::now() < deadline, "the hook did not run");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(payload["event"], "after_session");
    assert_eq!(payload["session"], session, "the hook is told which session ended");
}

/// A skill the project keeps is a command in an editor as it is in the
/// terminal: offered in the same list, and expanded before the model sees it.
#[test]
fn a_project_skill_is_offered_as_a_command_and_expands_before_the_model_sees_it() {
    let (home, work) = scratch("template");
    std::fs::create_dir_all(work.join(".elmee/skills/review")).unwrap();
    std::fs::write(
        work.join(".elmee/skills/review/SKILL.md"),
        "---\ndescription: review a path\n---\nReview $1 for defects.\n",
    )
    .unwrap();
    let mut client = Client::start(&home, &work, &stub_provider("reviewed"), &[]);
    client.hello();
    let (opened, _) = client.call("session/new", serde_json::json!({ "cwd": work, "mcpServers": [] }));
    let offered = client.offered();
    assert!(offered.iter().any(|name| name == "review"), "the skill is advertised: {offered:?}");
    let session = opened["result"]["sessionId"].as_str().unwrap().to_owned();

    let (answer, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "/review src/lib.rs" }] }),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let transcript = std::fs::read_to_string(&session).expect("a persisted session is its file");
    assert!(transcript.contains("Review src/lib.rs for defects."), "{transcript}");
    assert!(
        transcript.contains(r#""content":"/review src/lib.rs\n\nFollow the skill"#),
        "it opens with what was typed, which is what the conversation is listed by: {transcript}"
    );
}

/// A command the agent's plugins own runs when an editor sends its name, and
/// never reaches the model. The menu an editor draws from the advertised list
/// sends exactly this: a slash, the name, and whatever was typed after.
#[test]
fn a_command_the_agent_offers_runs_without_the_model() {
    let (home, work) = scratch("command");
    let mut client = Client::start(&home, &work, &stub_provider("the model spoke"), &["--no-session"]);
    client.hello();
    let (opened, _) = client.call("session/new", serde_json::json!({ "cwd": work, "mcpServers": [] }));
    assert!(!client.offered().is_empty(), "the commands follow the session they belong to");
    let session = opened["result"]["sessionId"].as_str().unwrap().to_owned();

    let asked = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "/goal" }] }),
    );
    let (answer, updates) = client.wait_full(asked);
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let said: Vec<&str> = updates.iter().filter_map(|update| update["content"]["text"].as_str()).collect();
    assert!(!said.is_empty(), "the command answered: {updates:?}");
    assert!(
        said.iter().all(|text| !text.contains("the model spoke")),
        "and the model was not asked: {said:?}"
    );
}

#[test]
fn session_handshake_reports_the_claimed_mesh_identity_not_the_requested_name() {
    let (home, work) = scratch("mesh-identity");
    let provider = stub_provider("unused");
    let mut first = Client::start(&home, &work, &provider, &["--no-session", "--name", "worker"]);
    first.hello();
    let (opened, _) = first.call("session/new", serde_json::json!({ "cwd": work }));
    let original = opened["result"]["_meta"]["elmee"]["mesh"].clone();
    assert_eq!(original["name"], "worker");

    let mut second = Client::start(&home, &work, &provider, &["--no-session", "--name", "worker"]);
    second.hello();
    let (opened, _) = second.call("session/new", serde_json::json!({ "cwd": work }));
    let allocated = &opened["result"]["_meta"]["elmee"]["mesh"];
    assert_eq!(allocated["name"], "worker-2", "{opened}");
    assert_eq!(allocated["root"], original["root"]);
    assert_ne!(allocated["incarnation"], original["incarnation"]);
    for identity in [&original, allocated] {
        let root = std::path::Path::new(identity["root"].as_str().unwrap());
        let address = identity["name"].as_str().unwrap();
        let incarnation: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("locks").join(address)).unwrap()).unwrap();
        assert_eq!(identity["incarnation"], incarnation);
        assert!(!incarnation.as_str().unwrap().is_empty());
        assert!(root.join("agents").join(format!("{address}.json")).is_file());
    }
}

/// Model and reasoning depth are real request choices: both are listed when a
/// session opens and switchable while it is idle.
#[test]
fn the_model_is_a_config_option_and_can_be_switched() {
    let (home, work) = scratch("model");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let (opened, _) = client.call("session/new", serde_json::json!({ "cwd": work, "mcpServers": [] }));
    let option = &opened["result"]["configOptions"][0];
    assert_eq!(option["id"], "model");
    assert_eq!(option["category"], "model");
    assert_eq!(option["currentValue"], "ollama/stub");
    let reasoning = &opened["result"]["configOptions"][1];
    assert_eq!(reasoning["id"], "reasoning_effort");
    assert_eq!(reasoning["category"], "thought_level");
    assert_eq!(reasoning["currentValue"], "auto");
    let session = opened["result"]["sessionId"].as_str().unwrap().to_owned();

    let (switched, _) = client.call(
        "session/set_config_option",
        serde_json::json!({ "sessionId": session, "configId": "model", "value": "ollama/other" }),
    );
    assert_eq!(switched["result"]["configOptions"][0]["currentValue"], "ollama/other", "{switched}");
    let (switched, _) = client.call(
        "session/set_config_option",
        serde_json::json!({ "sessionId": session, "configId": "reasoning_effort", "value": "high" }),
    );
    assert_eq!(switched["result"]["configOptions"][1]["currentValue"], "high", "{switched}");
    let (answer, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn", "the session still works on the other model");
}

/// A past conversation can be found by what it was about and taken up without
/// hearing it again — which is what a client that kept its own copy wants.
#[test]
fn a_past_session_is_listed_by_its_subject_and_can_be_resumed_quietly() {
    let (home, work) = scratch("resume");
    let address = stub_provider("noted");
    let session = {
        let mut client = Client::start(&home, &work, &address, &[]);
        client.hello();
        let session = client.open(&work);
        client.call(
            "session/prompt",
            serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "keep this in mind" }] }),
        );
        session
    };

    let mut later = Client::start(&home, &work, &address, &["--no-session"]);
    later.hello();
    let (listed, _) = later.call("session/list", serde_json::json!({ "cwd": work }));
    let past = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sessionId"] == session.as_str())
        .unwrap_or_else(|| panic!("the session on disk is offered: {listed}"));
    assert_eq!(past["title"], "keep this in mind");

    let (resumed, updates) =
        later.call("session/resume", serde_json::json!({ "sessionId": session, "cwd": work }));
    assert!(resumed.get("error").is_none(), "{resumed}");
    assert!(!updates.contains(&"user_message_chunk".to_owned()), "resume does not replay: {updates:?}");
    let (answer, _) = later.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "go on" }] }),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

/// A subagent is another process speaking this same protocol, driven by the
/// parent as a client. The delegating turn ends at once; the report arrives
/// later as a turn nobody prompted, and the editor hears it like any other.
#[test]
fn a_delegated_task_runs_as_another_process_speaking_the_protocol() {
    let (home, work) = scratch("delegate");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.hello();
    let session = client.open(&work);

    let asked = client.send(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "delegate this" }] }),
    );
    let (answer, updates) = client.wait_full(asked);
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let spawned =
        updates.iter().find(|update| update["title"] == "spawn_agent").expect("the delegation is shown");
    assert_eq!(spawned["kind"], "other");

    // Nothing is sent from here on. What the session says next, it says
    // because its child reported.
    loop {
        let message = client.read();
        let text =
            message.pointer("/params/update/content/text").and_then(|it| it.as_str()).unwrap_or_default();
        if message["params"]["sessionId"] == session.as_str() && text.contains("the helper reported") {
            return;
        }
    }
}

/// Signing in is something a client can offer: the handshake names the way,
/// and a turn that needs an account nobody signed in to says so in the
/// protocol's own terms rather than as a failure.
#[test]
fn a_client_is_told_how_to_sign_in_and_when_it_must() {
    let (home, work) = scratch("sign-in");
    let mut client =
        Client::start(&home, &work, &stub_provider("unused"), &["--no-session", "-m", "codex/gpt-test"]);
    let hello = client.hello();
    assert_eq!(hello["authMethods"][0]["id"], "codex", "{hello}");

    let session = client.open(&work);
    let (refused, _) = client.call(
        "session/prompt",
        serde_json::json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    assert_eq!(refused["error"]["code"], -32000, "nobody is signed in on this machine: {refused}");

    let (out, _) = client.call("logout", serde_json::json!({}));
    assert!(out.get("result").is_some(), "{out}");
}
