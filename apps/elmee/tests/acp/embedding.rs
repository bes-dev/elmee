//! What an application embedding the agent can ask of it beyond a turn.
//!
//! The handshake is a promise, and the first test here holds the agent to all
//! of it: a capability nobody exercises is one that breaks unnoticed.

use super::*;
use serde_json::{Value, json};

fn prompt(client: &mut Client, session: &str, text: &str) -> (Value, Vec<Value>) {
    let asked = client.send(
        "session/prompt",
        json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
    );
    client.wait_full(asked)
}

fn said(updates: &[Value]) -> String {
    updates.iter().filter_map(|update| update.pointer("/content/text").and_then(Value::as_str)).collect()
}

/// Every capability the handshake declares names a request, and each of those
/// is answered by something other than "no such method". A capability added
/// to the handshake without a line here fails, which is the point.
#[test]
fn everything_the_handshake_declares_is_answered() {
    let (home, work) = scratch("declared");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &[]);
    let hello = client.hello();
    let session = client.open(&work);
    prompt(&mut client, &session, "hello");

    let capabilities = &hello["agentCapabilities"];
    let declared = |pointer: &str| -> Vec<String> {
        capabilities
            .pointer(pointer)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(key, _)| key.clone())
            .collect()
    };
    let mut requests = Vec::new();
    for capability in declared("/sessionCapabilities") {
        requests.push(match capability.as_str() {
            "list" => ("session/list", json!({ "cwd": work })),
            "status" => ("session/status", json!({ "sessionId": session })),
            "title" => ("session/set_title", json!({ "sessionId": session, "title": "named" })),
            "fork" => ("session/fork", json!({ "sessionId": session, "cwd": work, "mcpServers": [] })),
            "resume" => ("session/resume", json!({ "sessionId": session, "cwd": work })),
            "additionalDirectories" => {
                ("session/new", json!({ "cwd": work, "mcpServers": [], "additionalDirectories": [home] }))
            }
            "systemPrompt" => {
                let given =
                    json!({ "mode": "override", "content": [{ "type": "text", "text": "be brief" }] });
                ("session/new", json!({ "cwd": work, "mcpServers": [], "systemPrompt": given }))
            }
            // Heard rather than asked for: a child's stream has its own test.
            "subagents" => continue,
            // Last, in that order, because they end the session the rest use.
            "close" | "delete" => continue,
            other => panic!("the handshake declares {other} and nothing here exercises it"),
        });
    }
    for capability in declared("/auth") {
        requests.push(match capability.as_str() {
            "status" => ("auth/status", json!({})),
            "logout" => ("logout", json!({})),
            other => panic!("the handshake declares auth.{other} and nothing here exercises it"),
        });
    }
    for extension in hello["_meta"].as_object().into_iter().flatten().map(|(key, _)| key.as_str()) {
        requests.push(match extension {
            "rewind" => ("_session/history", json!({ "sessionId": session })),
            "steering" => {
                let idle = json!({ "steering": { "idleBehavior": "promptRequired" } });
                let said = json!([{ "type": "text", "text": "wait" }]);
                ("_session/steering", json!({ "sessionId": session, "prompt": said, "_meta": idle }))
            }
            other => panic!("the handshake declares {other} and nothing here exercises it"),
        });
    }
    requests.push(("session/close", json!({ "sessionId": session })));
    requests.push(("session/delete", json!({ "sessionId": session })));
    assert!(capabilities["loadSession"] == true && capabilities["mcpCapabilities"]["acp"] == true);

    for (method, params) in requests {
        let (answer, _) = client.call(method, params);
        assert!(answer.get("result").is_some(), "{method} is declared and was refused: {answer}");
    }
}

/// The embedding guide names everything the binary
/// declares, so the two cannot drift apart without a test saying so.
#[test]
fn the_embedding_guide_names_every_capability_the_handshake_declares() {
    let (home, work) = scratch("readme");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    let hello = client.hello();
    let guide = include_str!("../../../../docs/embedding.md");
    let capabilities = &hello["agentCapabilities"];
    let keys = |value: &Value| -> Vec<String> {
        value.as_object().into_iter().flatten().map(|(key, _)| key.clone()).collect()
    };
    let mut declared = keys(&capabilities["sessionCapabilities"]);
    declared.extend(keys(&hello["_meta"]));
    declared.extend(keys(&capabilities["auth"]).into_iter().map(|key| format!("auth.{key}")));
    for capability in declared {
        assert!(
            guide.contains(&format!("`{capability}`")),
            "docs/embedding.md does not mention `{capability}`"
        );
    }
}

/// Instructions a client gives for a session reach the model: beside the
/// agent's own when appended, in their place when they override. A mode this
/// agent does not have opens no session at all.
#[test]
fn a_session_takes_the_clients_instructions_as_asked() {
    let (home, work) = scratch("system-prompt");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.hello();
    let mut opened = |mode: &str| {
        let given = json!({ "mode": mode, "content": [{ "type": "text", "text": "answer as a pirate" }] });
        client.call("session/new", json!({ "cwd": work, "mcpServers": [], "systemPrompt": given })).0
    };
    let (appended, replaced, refused) = (opened("append"), opened("override"), opened("merge"));
    assert_eq!(refused["error"]["code"], -32602, "{refused}");

    let appended = appended["result"]["sessionId"].as_str().expect("a session").to_owned();
    assert_eq!(said(&prompt(&mut client, &appended, "hello").1), "appended");
    let replaced = replaced["result"]["sessionId"].as_str().expect("a session").to_owned();
    assert_eq!(said(&prompt(&mut client, &replaced, "hello").1), "replaced");

    // A fork is the same agent, so it was given the same instructions.
    let (fork, _) = client.call("session/fork", json!({ "sessionId": replaced, "cwd": work }));
    let fork = fork["result"]["sessionId"].as_str().expect("a fork").to_owned();
    assert_eq!(said(&prompt(&mut client, &fork, "again").1), "replaced");
}

/// A conversation is listed by the name a person gave it, whether it was
/// named while open or after it was put away.
#[test]
fn a_session_is_renamed_open_or_saved_and_listed_by_that_name() {
    let (home, work) = scratch("title");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &[]);
    client.hello();
    let session = client.open(&work);
    prompt(&mut client, &session, "the first thing asked");
    let listed = |client: &mut Client| {
        client.call("session/list", json!({ "cwd": work })).0["result"]["sessions"][0].clone()
    };
    assert_eq!(listed(&mut client)["title"], "the first thing asked");

    let named = client.send("session/set_title", json!({ "sessionId": session, "title": "release notes" }));
    let (answer, updates) = client.wait_full(named);
    assert_eq!(answer["result"]["title"], "release notes", "{answer}");
    assert!(updates.iter().any(|update| update["title"] == "release notes"), "{updates:?}");
    assert_eq!(listed(&mut client)["title"], "release notes");
    assert_eq!(client.call("session/status", json!({ "sessionId": session })).0["result"]["status"], "open");

    client.call("session/close", json!({ "sessionId": session }));
    assert_eq!(client.call("session/status", json!({ "sessionId": session })).0["result"]["status"], "saved");
    client.call("session/set_title", json!({ "sessionId": session, "title": "changelog" }));
    assert_eq!(listed(&mut client)["title"], "changelog");
    let (gone, _) = client.call("session/set_title", json!({ "sessionId": "/nowhere.jsonl", "title": "x" }));
    assert!(gone.get("error").is_some(), "{gone}");
}

/// Whether anyone is signed in is something a client can ask before it
/// decides what to offer.
#[test]
fn a_client_can_ask_who_is_signed_in() {
    let (home, work) = scratch("auth-status");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let (nobody, _) = client.call("auth/status", json!({}));
    assert_eq!(nobody["result"]["state"], "unknown", "{nobody}");

    std::fs::create_dir_all(home.join(".elmee")).unwrap();
    let account = json!({ "access_token": "a", "refresh_token": "r", "email": "dev@example.com" });
    std::fs::write(home.join(".elmee/auth.json"), account.to_string()).unwrap();
    let (someone, _) = client.call("auth/status", json!({}));
    assert_eq!(someone["result"]["state"], "authenticated", "{someone}");
    assert_eq!(someone["result"]["accounts"][0]["label"], "dev@example.com");
}

/// The conversation is a tree, and a client can walk it: go back to before a
/// prompt, get that prompt back to edit, and return to the line it left.
#[test]
fn a_client_rewinds_a_conversation_and_returns_to_the_line_it_left() {
    let (home, work) = scratch("rewind");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &[]);
    client.hello();
    let session = client.open(&work);
    prompt(&mut client, &session, "first");
    prompt(&mut client, &session, "second");

    let (tree, _) = client.call("_session/history", json!({ "sessionId": session }));
    let prompts = tree["result"]["prompts"].as_array().expect("prompts").clone();
    assert_eq!(
        prompts.iter().map(|asked| asked["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    let left = tree["result"]["branches"][0]["leaf"].clone();

    let back = json!({ "sessionId": session, "promptId": prompts[1]["id"], "restore": "conversation" });
    let (rewound, _) = client.call("_session/rewind", back);
    assert_eq!(rewound["result"]["prompt"], "second", "the prompt comes back to be edited: {rewound}");
    assert_eq!(rewound["result"]["prompts"].as_array().map(Vec::len), Some(1));

    // What the model is shown is what the tree says: the reply to a new
    // prompt follows "first" alone, and the line left behind is still there.
    prompt(&mut client, &session, "third");
    let (tree, _) = client.call("_session/history", json!({ "sessionId": session }));
    assert_eq!(tree["result"]["branches"].as_array().map(Vec::len), Some(2), "{tree}");
    let (returned, _) = client.call("_session/checkout", json!({ "sessionId": session, "leaf": left }));
    let texts: Vec<&str> = returned["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|asked| asked["text"].as_str())
        .collect();
    assert_eq!(texts, ["first", "second"], "{returned}");

    // The files alone can go back, where they were recorded: the
    // conversation stays where it is and no prompt is handed back.
    assert_eq!(prompts[1]["checkpoint"], true, "the tree was recorded before the prompt's turn: {prompts:?}");
    std::fs::write(work.join("stray.txt"), "written after the second prompt").unwrap();
    let files = json!({ "sessionId": session, "promptId": prompts[1]["id"], "restore": "files" });
    let (restored, _) = client.call("_session/rewind", files);
    assert!(restored["result"]["prompt"].is_null(), "{restored}");
    assert_eq!(restored["result"]["prompts"].as_array().map(Vec::len), Some(2));
    assert!(!work.join("stray.txt").exists(), "the tree is as it was before that prompt");

    let (unknown, _) = client.call("_session/rewind", json!({ "sessionId": session, "promptId": 999_999 }));
    assert!(unknown.get("error").is_some(), "{unknown}");
    let (after, _) = prompt(&mut client, &session, "still working");
    assert_eq!(after["result"]["stopReason"], "end_turn", "a refused rewind leaves the session usable");
}

/// An application can be an MCP server to the agent it embeds, over the
/// connection it already has: no process to start, no port to open.
#[test]
fn a_tool_the_client_serves_is_listed_and_called_over_the_same_connection() {
    let (home, work) = scratch("mcp-over-acp");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    assert_eq!(client.hello()["agentCapabilities"]["mcpCapabilities"]["acp"], true);

    let editor = json!({ "type": "acp", "name": "editor", "serverId": "editor-1" });
    let opening = client.send("session/new", json!({ "cwd": work, "mcpServers": [editor] }));
    let listing = client.until_asked("mcp/message");
    assert_eq!(listing["params"]["serverId"], "editor-1");
    assert_eq!(listing["params"]["method"], "tools/list");
    assert!(
        listing["params"]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"].is_string(),
        "a request with no handshake behind it says what it speaks: {listing}"
    );
    let tool = json!({ "name": "selection", "description": "what is selected", "inputSchema": { "type": "object" } });
    client.respond(&listing["id"], json!({ "result": { "tools": [tool] } }));
    let session = client.wait(opening).0["result"]["sessionId"].as_str().expect("a session").to_owned();

    let asked = client.send(
        "session/prompt",
        json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "ask the editor" }] }),
    );
    let calling = client.until_asked("mcp/message");
    assert_eq!(calling["params"]["method"], "tools/call");
    assert_eq!(calling["params"]["params"]["name"], "selection");
    let result = json!({ "content": [{ "type": "text", "text": "the selected text" }] });
    client.respond(&calling["id"], json!({ "result": result }));
    let (answer, updates) = client.wait_full(asked);
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(said(&updates).contains("the editor holds the selected text"), "{updates:?}");
}

/// A child is a session of its own on the parent's connection: announced on
/// the parent's, heard in its own words under its own id, and said to be idle
/// when it is done. Only a client that asked to be shown children is told.
#[test]
fn a_child_streams_as_a_session_of_its_own() {
    let (home, work) = scratch("subagents");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    client.hello_showing(json!({ "subagents": {} }));
    let session = client.open(&work);
    client.send(
        "session/prompt",
        json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "delegate this" }] }),
    );

    let (mut child, mut heard, mut tools) = (None::<String>, String::new(), Vec::new());
    loop {
        let message = client.read();
        let (from, update) = (&message["params"]["sessionId"], &message["params"]["update"]);
        if update["sessionUpdate"] == "subagent_update" {
            assert_eq!(from, session.as_str(), "a child is announced on its parent's session");
            match update["state"]["state"].as_str() {
                Some("running") => {
                    assert_eq!(update["title"], "helper");
                    child = update["sessionId"].as_str().map(str::to_owned);
                }
                Some("idle") => break,
                other => panic!("an unexpected child state {other:?}"),
            }
        } else if child.as_deref().is_some_and(|child| from == child) {
            heard.push_str(update.pointer("/content/text").and_then(Value::as_str).unwrap_or_default());
        } else if update["sessionUpdate"] == "tool_call" {
            tools.push(update["name"].clone());
        }
    }
    assert!(heard.contains("the child found it"), "the child's own words arrive under its own id: {heard}");
    assert_eq!(tools, ["spawn_agent"], "a tool call names the tool, not only a title for it");
}

/// A compaction is announced as one: the summary that now stands in the
/// conversation's place, to a client that said it can show that.
#[test]
fn a_compaction_is_shown_to_a_client_that_can_show_one() {
    let (home, work) = scratch("compaction");
    // A provider that charges like a real one, so the room is measured.
    let body = json!({
        "choices": [{ "message": { "role": "assistant", "content": "the summary" } }],
        "usage": { "prompt_tokens": 2500, "completion_tokens": 2 },
    });
    let provider = serve_json(body.to_string(), 0);
    let mut client = Client::start(&home, &work, &provider, &["--no-session", "--compact-chars", "30000"]);
    client.hello_showing(json!({ "session": { "compaction": {} } }));
    let session = client.open(&work);
    prompt(&mut client, &session, "first");
    prompt(&mut client, &session, "second");
    let (answer, updates) = prompt(&mut client, &session, &"a long paste ".repeat(5000));
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let compacted = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "compaction_update")
        .unwrap_or_else(|| panic!("no compaction was announced: {updates:?}"));
    assert_eq!(compacted["status"], "completed");
    assert!(compacted["summary"][0]["text"].as_str().is_some_and(|summary| summary.contains("the summary")));
}

/// A server the client declared that would not start is said to the person,
/// not only written to a log nobody reads.
#[test]
fn a_server_that_would_not_start_is_a_notice() {
    let (home, work) = scratch("notice");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello_showing(json!({ "session": { "notices": {} } }));
    let broken = json!({ "name": "absent", "command": "elmee-no-such-mcp-server", "args": [], "env": [] });
    client.send("session/new", json!({ "cwd": work, "mcpServers": [broken] }));
    loop {
        let update = client.read()["params"]["update"].clone();
        if update["sessionUpdate"] == "notice" {
            assert_eq!(update["severity"], "warning");
            assert!(update["title"].as_str().is_some_and(|title| title.contains("absent")), "{update}");
            return;
        }
    }
}

/// The thin clients are the embedding surface as an application meets it, so
/// each drives the real binary: a session, a turn, and a function of its own
/// served to the model as a tool over the same connection.
fn thin_client(runtime: &str, script: &str, name: &str) -> String {
    let (home, work) = scratch(name);
    let run = std::process::Command::new(runtime)
        .arg(script)
        .args([env!("CARGO_BIN_EXE_elmee"), work.to_str().unwrap()])
        .env("HOME", &home)
        .env_remove("ELMEE_MODEL")
        .env_remove("ELMEE_MESH_ROOT")
        .env("ELMEE_BASE_URL", format!("http://{}/v1", scripted_provider()))
        .env("ELMEE_API_KEY", "stub")
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../clients"))
        .output()
        .expect("the client runs");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    String::from_utf8_lossy(&run.stdout).trim().to_owned()
}

#[test]
fn the_python_client_runs_a_turn_and_serves_a_tool() {
    assert_eq!(
        thin_client("python3", "python/example.py", "python-client"),
        "the editor holds the selected text"
    );
}

#[test]
fn the_typescript_client_runs_a_turn_and_serves_a_tool() {
    // Node runs TypeScript as it is since 23.6; an older one, or none, has
    // nothing to say about this file.
    let recent = std::process::Command::new("node").arg("--version").output().is_ok_and(|node| {
        let version = String::from_utf8_lossy(&node.stdout);
        version.trim_start_matches('v').split('.').next().and_then(|major| major.parse::<u32>().ok())
            >= Some(24)
    });
    if !recent {
        return eprintln!("skipped: no node 24 on this machine");
    }
    assert_eq!(
        thin_client("node", "typescript/example.ts", "typescript-client"),
        "the editor holds the selected text"
    );
}
