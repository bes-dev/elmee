//! What the wire has to look like, checked without a model.
//!
//! The translation is where a protocol implementation actually goes wrong —
//! framing is one line and dispatch is a match, but a tool call an editor
//! cannot place on a file is a tool call it draws as a grey box. So that is
//! what these hold.

use std::path::Path;

use serde_json::json;

use super::*;

const CWD: &str = "/work";

fn of(event: &elmee_core::AgentEvent) -> Option<serde_json::Value> {
    update::of(Path::new(CWD), Some(1000), event)
}

fn tool_start(name: &str, arguments: serde_json::Value) -> elmee_core::AgentEvent {
    elmee_core::AgentEvent::ToolStart {
        id: "call-1".into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

/// An editor follows an agent by the files it touches. Every tool we offer has
/// to say what kind of thing it is doing, and the two that name a file have to
/// say which — a call without a location is a call the editor can only list.
#[test]
fn a_tool_call_says_what_kind_it_is_and_where_it_happens() {
    let read = of(&tool_start("read", json!({ "path": "src/main.rs" }))).unwrap();
    assert_eq!(read["sessionUpdate"], "tool_call");
    assert_eq!(read["kind"], "read");
    assert_eq!(read["status"], "in_progress");
    assert_eq!(read["locations"][0]["path"], "/work/src/main.rs", "absolute, as the protocol asks");
    assert_eq!(read["title"], "read src/main.rs");

    let patch =
        "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** Add File: b.rs\n+fresh\n*** End Patch";
    let edit = of(&tool_start("apply_patch", json!({ "patch": patch }))).unwrap();
    assert_eq!(edit["kind"], "edit");
    let touched: Vec<&str> =
        edit["locations"].as_array().unwrap().iter().map(|at| at["path"].as_str().unwrap()).collect();
    assert_eq!(touched, ["/work/a.rs", "/work/b.rs"], "both files the patch names");
    // An edit is drawn as a diff, which is what makes it reviewable in place.
    let diffs = edit["content"].as_array().unwrap();
    assert_eq!(diffs.len(), 2);
    assert_eq!(diffs[0]["type"], "diff");
    assert_eq!(diffs[0]["path"], "/work/a.rs");
    assert_eq!(diffs[0]["oldText"], "old");
    assert_eq!(diffs[0]["newText"], "new");
    assert!(diffs[1]["oldText"].is_null(), "a new file had no text before");
    assert_eq!(diffs[1]["newText"], "fresh");

    let gone = "*** Begin Patch\n*** Delete File: a.rs\n*** End Patch";
    assert_eq!(of(&tool_start("apply_patch", json!({ "patch": gone }))).unwrap()["kind"], "delete");

    let run = of(&tool_start("exec", json!({ "command": "cargo test" }))).unwrap();
    assert_eq!(run["kind"], "execute");
    assert_eq!(run["title"], "exec cargo test", "a person reads the command, not the arguments");
    assert!(run["locations"].as_array().unwrap().is_empty());

    // A tool the mapping has never heard of still produces a well-formed call.
    let unknown = of(&tool_start("something_new", json!({}))).unwrap();
    assert_eq!(unknown["kind"], "other");
    assert_eq!(unknown["title"], "something_new");
}

/// The end of a call carries what it produced, and whether it worked, in the
/// one shape the protocol has for it.
#[test]
fn a_finished_tool_call_reports_its_outcome() {
    let ended = |ok| {
        of(&elmee_core::AgentEvent::ToolEnd {
            id: "call-1".into(),
            name: "exec".into(),
            ok,
            duration_ms: 3,
            output: "done".into(),
        })
        .unwrap()
    };
    assert_eq!(ended(true)["sessionUpdate"], "tool_call_update");
    assert_eq!(ended(true)["status"], "completed");
    assert_eq!(ended(true)["content"][0]["content"]["text"], "done");
    assert_eq!(ended(false)["status"], "failed");
}

/// Our plan carries a citation the protocol has no field for. It goes in
/// `_meta`, which is where the specification puts what it does not model —
/// dropping it would lose the only thing that makes "done" mean anything.
#[test]
fn a_plan_keeps_its_proof_where_the_protocol_allows_extras() {
    let step = |text: &str, status, proof: Option<&str>| elmee_core::plan::Step {
        step: text.into(),
        status,
        proof: proof.map(str::to_owned),
        proven: proof.is_some(),
    };
    let plan = of(&elmee_core::AgentEvent::Plan {
        steps: vec![
            step("wire it up", elmee_core::plan::Status::Done, Some("cargo test")),
            step("ship", elmee_core::plan::Status::Doing, None),
        ],
    })
    .unwrap();

    assert_eq!(plan["sessionUpdate"], "plan");
    assert_eq!(plan["entries"][0]["status"], "completed");
    assert_eq!(plan["entries"][1]["status"], "in_progress");
    assert_eq!(plan["entries"][0]["_meta"]["elmee"]["proof"], "cargo test");
    assert_eq!(plan["entries"][0]["_meta"]["elmee"]["proven"], true);
}

/// Text and thinking are different channels in the protocol, and an editor
/// draws them differently. Everything else a turn emits has no counterpart,
/// and inventing one would give clients something to learn to ignore.
#[test]
fn speech_and_thinking_are_separate_and_the_rest_is_silence() {
    let text = of(&elmee_core::AgentEvent::Text { text: "hello".into() }).unwrap();
    assert_eq!(text["sessionUpdate"], "agent_message_chunk");
    assert_eq!(text["content"]["text"], "hello");

    let thought = of(&elmee_core::AgentEvent::Reasoning { text: "hmm".into() }).unwrap();
    assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");

    assert!(of(&elmee_core::AgentEvent::ModelStart { context_chars: 1 }).is_none());
    // A compaction is said only to a client that asked to hear of one, so
    // it is not this function's to translate.
    assert!(of(&elmee_core::AgentEvent::Compacted { notes: 1, summary: "kept".into() }).is_none());
    let done = update::compaction("c1", "completed", Some("kept"));
    assert_eq!(done["summary"][0]["text"], "kept");
    assert!(update::compaction("c1", "failed", None).get("summary").is_none());

    // What a request cost is how full the context is, against the window —
    // and without a window there is no gauge to draw, so nothing is said.
    let charged = elmee_core::AgentEvent::ModelEnd {
        ok: true,
        input_tokens: 400,
        output_tokens: 9,
        duration_ms: 1,
        metrics: None,
    };
    let usage = of(&charged).unwrap();
    assert_eq!(usage["sessionUpdate"], "usage_update");
    assert_eq!(usage["used"], 400);
    assert_eq!(usage["size"], 1000);
    assert!(update::of(Path::new(CWD), None, &charged).is_none());
}

/// A conversation handed back is the updates that would have produced it —
/// tool calls included, since the transcript keeps both the call and what it
/// returned. What the harness wrote in the user's voice stays out.
#[test]
fn a_replayed_conversation_has_its_tool_calls() {
    use elmee_core::message::{FunctionCall, Message, MessageOrigin, ToolCall};
    let mut asked = Message::assistant_text("");
    asked.tool_calls = vec![ToolCall {
        id: "c1".into(),
        kind: "function".into(),
        function: FunctionCall { name: "read".into(), arguments: json!({ "path": "a.rs" }).to_string() },
    }];
    let mut stamped = Message::user("<agent_identity/>");
    stamped.origin = MessageOrigin::Background;
    let messages = [
        Message::system("be brief"),
        Message::user("look at a.rs"),
        stamped,
        asked,
        Message::tool("c1", "fn main() {}"),
        Message::assistant_text("it is a main"),
    ];
    let kinds: Vec<String> = update::replay(Path::new(CWD), &messages)
        .iter()
        .map(|update| update["sessionUpdate"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["user_message_chunk", "tool_call", "tool_call_update", "agent_message_chunk"]);
}

/// The client's content blocks become the one string a turn starts from. A
/// file the editor attached is fenced under its own name, because it is data
/// the person is pointing at rather than something they said.
#[test]
fn attached_context_arrives_as_data_and_not_as_instructions() {
    let cwd = std::env::temp_dir().join(format!("elmee-acp-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    let text = prompt_text(
        &json!({
            "prompt": [
                { "type": "text", "text": "explain this" },
                { "type": "resource", "resource": { "uri": "file:///src/a.rs", "text": "fn main() {}" } },
                { "type": "resource_link", "uri": "file:///src/b.rs" },
                { "type": "image", "mimeType": "image/png", "data": elmee_core::text::base64(b"PNG!") },
            ]
        }),
        &cwd,
    );
    assert!(text.starts_with("explain this"));
    assert!(text.contains("<context uri=\"file:///src/a.rs\">"), "{text}");
    assert!(text.contains("fn main() {}"));
    assert!(text.contains("/src/b.rs"), "a link is the path the tools can open: {text}");
    // A picture pasted inline is written where the workspace looks for
    // pictures, and named, which is how one reaches the model.
    let saved = text.lines().last().unwrap();
    assert!(saved.starts_with("media/acp/") && saved.ends_with(".png"), "{saved}");
    assert_eq!(std::fs::read(cwd.join(saved)).unwrap(), b"PNG!");
    let _ = std::fs::remove_dir_all(&cwd);
}

/// One session, one turn — with one exception the protocol does not have to
/// know about: a turn nobody asked for gives way to a person, because they
/// waited for neither of them.
#[test]
fn a_session_runs_one_turn_at_a_time_and_a_person_outranks_a_wake() {
    assert_eq!(claim(false, false, true), Claim::Free, "an idle session takes a prompt");
    assert_eq!(claim(false, false, false), Claim::Free, "and a wake");

    // A turn a person asked for holds the session against everything.
    assert_eq!(claim(true, false, true), Claim::Busy, "a second prompt is refused, as the protocol says");
    assert_eq!(claim(true, false, false), Claim::Busy, "and a subagent's report waits its turn");

    // A turn nobody asked for yields to a person, and to nobody else.
    assert_eq!(claim(true, true, true), Claim::Interrupting);
    assert_eq!(claim(true, true, false), Claim::Busy);
}

/// A client naming servers for a session hands them over as descriptions, not
/// as the protocol's own spelling: a composition root should never have to
/// read an ACP schema to act on one.
#[test]
fn servers_the_client_declares_arrive_as_plain_descriptions() {
    let client = Client::new(mpsc::unbounded_channel().0);
    let servers = declared(
        &json!({
            "mcpServers": [
                { "name": "notes", "command": "npx", "args": ["-y", "notes-mcp"],
                  "env": [{ "name": "TOKEN", "value": "abc" }] },
                { "name": "broken" },
                { "type": "http", "name": "issues", "url": "https://mcp.example/mcp",
                  "headers": [{ "name": "Authorization", "value": "Bearer t" }] },
                { "type": "acp", "name": "editor", "serverId": "srv-1" },
            ]
        }),
        &client,
    );
    assert_eq!(servers.len(), 3, "one with neither a command nor a url is not a server we can reach");
    assert!(servers[2].served.is_some(), "a server the client itself is, is asked over the connection");
    assert_eq!(servers[0].name, "notes");
    assert_eq!(servers[0].args, ["-y", "notes-mcp"]);
    assert_eq!(servers[0].env, [("TOKEN".to_owned(), "abc".to_owned())]);
    assert_eq!(servers[1].url.as_deref(), Some("https://mcp.example/mcp"));
    assert_eq!(servers[1].headers, [("Authorization".to_owned(), "Bearer t".to_owned())]);
    assert!(declared(&json!({}), &client).is_empty());
}

/// Version negotiation goes down, never up: a client on an older protocol is
/// answered in its own, and one ahead of us is answered in ours.
#[test]
fn the_handshake_meets_the_client_where_it_is() {
    assert_eq!(initialize(&json!({ "protocolVersion": 99 }), None)["protocolVersion"], 1);
    assert_eq!(initialize(&json!({ "protocolVersion": 0 }), None)["protocolVersion"], 0);
    let ours = initialize(&json!({ "protocolVersion": 1 }), None);
    assert_eq!(ours["agentInfo"]["name"], "elmee");
    assert_eq!(ours["agentCapabilities"]["promptCapabilities"]["embeddedContext"], true);
    assert_eq!(ours["authMethods"].as_array().unwrap().len(), 0, "nothing to sign in to");
    // A conversation is a file, so it can be handed back and reopened.
    assert_eq!(ours["agentCapabilities"]["loadSession"], true);
    assert_eq!(ours["agentCapabilities"]["sessionCapabilities"]["resume"], json!({}));
    assert_eq!(ours["agentCapabilities"]["mcpCapabilities"]["http"], true);
}

/// A picture is named by this agent, not by whoever sent it.
///
/// The client supplies the media type, and the file name was built from it, so
/// a subtype of `../../..` walked the picture out of `media/acp` and wrote it
/// wherever the filesystem allowed. Prompt content is not always the person at
/// the keyboard: an embedded resource, an MCP server's answer or a page an
/// agent fetched arrive through the same field.
#[test]
fn a_picture_cannot_name_the_place_it_is_written() {
    let cwd = std::env::temp_dir().join(format!("elmee-acp-image-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    let picture = |mime: &str| {
        prompt_text(
            &json!({ "prompt": [{ "type": "image", "mimeType": mime, "data": elmee_core::text::base64(b"PNG!") }] }),
            &cwd,
        )
    };

    // Four steps up from the name land in the workspace root, which is
    // writable, so this escaped without needing a single permission.
    assert_eq!(picture("image/../../../../escaped.txt"), "", "a media type chose a path");
    assert!(!cwd.join("escaped.txt").exists(), "a picture was written outside media/acp");
    for hostile in ["image/png/../evil", "image/", "image/../..", "image/waytoolongsubtype", "text/plain"] {
        assert_eq!(picture(hostile), "", "{hostile} was accepted as a name");
    }
    assert!(!cwd.join("media").exists(), "nothing was written for a media type this agent refuses");

    // The ordinary ones still arrive, including a structured suffix, which is
    // named by what it is rather than by its whole spelling.
    let png = picture("image/png");
    assert!(png.starts_with("media/acp/") && png.ends_with(".png"), "{png}");
    assert_eq!(std::fs::read(cwd.join(&png)).unwrap(), b"PNG!");
    let svg = picture("image/svg+xml");
    assert!(svg.ends_with(".svg"), "{svg}");
    let icon = picture("image/x-icon");
    assert!(icon.ends_with(".x-icon"), "a hyphen is part of a name, not a step in a path: {icon}");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// Instructions for a session are applied as asked or not at all: a mode this
/// agent does not have, or content it cannot read, is refused before any
/// session exists.
#[test]
fn a_system_prompt_is_taken_as_asked_or_refused() {
    let given = |mode: &str, content: serde_json::Value| {
        system_prompt(&json!({ "systemPrompt": { "mode": mode, "content": content } }))
    };
    let text = json!([{ "type": "text", "text": "be brief" }, { "type": "text", "text": "in French" }]);
    let appended = given("append", text.clone()).unwrap().unwrap();
    assert!(!appended.replace);
    assert_eq!(appended.text, "be brief\n\nin French");
    assert!(given("override", text.clone()).unwrap().unwrap().replace);
    assert!(given("merge", text).is_err());
    assert!(given("append", json!([])).is_err());
    assert!(given("append", json!([{ "type": "image", "data": "" }])).is_err());
    assert!(system_prompt(&json!({})).unwrap().is_none());
}
