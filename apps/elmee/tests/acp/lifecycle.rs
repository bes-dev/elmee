use super::*;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

fn mcp(work: &Path, delay: f64) -> serde_json::Value {
    let script = work.join("mcp.py");
    std::fs::write(&script, r#"import json, pathlib, sys, time
pathlib.Path(sys.argv[1]).write_text('started')
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m: continue
    if m.get('method') == 'initialize':
        time.sleep(float(sys.argv[2]))
        r = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'probe','version':'1'}}
    elif m.get('method') == 'tools/list': r = {'tools':[]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#).unwrap();
    json!({"name":"probe", "command":"python3", "args":[script, work.join("started"), delay.to_string()], "env":[]})
}

#[test]
fn first_session_installs_the_clients_mcp_instead_of_reusing_an_incompatible_spare() {
    let (home, work) = scratch("first-client-mcp");
    let declaration = mcp(&work, 0.0);
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let (reply, _) = client.call("session/new", json!({"cwd":work, "mcpServers":[declaration]}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert!(work.join("started").exists(), "the first session must start the client MCP server");
}

#[test]
fn slow_session_creation_does_not_block_control_or_another_sessions_cancellation() {
    let (home, work) = scratch("slow-new-control");
    let mut client = Client::start(&home, &work, &slow_provider("ok", 4000), &["--no-session"]);
    client.hello();
    let session = client.open(&work);
    let prompting =
        client.send("session/prompt", json!({"sessionId":session, "prompt":[{"type":"text","text":"hi"}]}));
    let other = work.join("other");
    std::fs::create_dir(&other).unwrap();
    let declaration = mcp(&other, 2.0);
    let opening = client.send("session/new", json!({"cwd":other, "mcpServers":[declaration]}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !other.join("started").exists() {
        assert!(Instant::now() < deadline, "MCP never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let start = Instant::now();
    let (listed, _) = client.call("session/list", json!({"cwd":work}));
    assert!(listed.get("result").is_some(), "{listed}");
    client.notify("session/cancel", json!({"sessionId":session}));
    let (cancelled, _) = client.wait(prompting);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    assert!(start.elapsed() < Duration::from_secs(1), "control waited for another session's MCP");
    let (opened, _) = client.wait(opening);
    assert!(opened.get("result").is_some(), "{opened}");
}

#[test]
fn a_session_end_hook_does_not_hold_the_connection_after_close_acknowledgement() {
    let (home, work) = scratch("slow-close-control");
    std::fs::create_dir_all(work.join(".elmee")).unwrap();
    std::fs::write(
        work.join(".elmee/hooks.json"),
        json!({"after_session":[{
            "command":"python3", "args":["-c", "import time; time.sleep(2)"]
        }]})
        .to_string(),
    )
    .unwrap();
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let session = client.open(&work);
    client.call("session/close", json!({"sessionId":session}));
    let start = Instant::now();
    let (listed, _) = client.call("session/list", json!({"cwd":work}));
    assert!(listed.get("result").is_some(), "{listed}");
    assert!(start.elapsed() < Duration::from_secs(1), "control waited for a closed session's hook");
}

#[test]
fn steering_reaches_the_current_turn_and_idle_fallback_keeps_the_input_with_the_client() {
    let (home, work) = scratch("steering");
    let mut client = Client::start(&home, &work, &scripted_provider(), &["--no-session"]);
    let hello = client.hello();
    assert_eq!(hello["_meta"]["steering"]["supported"], true);
    let session = client.open(&work);
    let (idle, _) = client.call("_session/steering", json!({"sessionId":session,
        "prompt":[{"type":"text","text":"keep this input"}], "_meta":{"steering":{"idleBehavior":"promptRequired"}}}));
    assert_eq!(idle["result"]["outcome"], "promptRequired");
    let prompting =
        client.send("session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"begin"}]}));
    let (steered, _) = client.call(
        "_session/steering",
        json!({"sessionId":session,
        "prompt":[{"type":"text","text":"steering correction received"}]}),
    );
    assert_eq!(steered["result"]["outcome"], "injected");
    let (done, updates) = client.wait_full(prompting);
    assert_eq!(done["result"]["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|u| u.pointer("/content/text").and_then(|t| t.as_str()) == Some("adjusted")),
        "{updates:?}"
    );
}

#[test]
fn persistent_sessions_and_forks_have_independent_transcripts() {
    let (home, work) = scratch("persistent-fork");
    let mut client = Client::start(&home, &work, &stub_provider("answer"), &[]);
    client.hello();
    let parent = client.open(&work);
    client.call(
        "session/prompt",
        json!({"sessionId":parent,"prompt":[{"type":"text","text":"parent history"}]}),
    );
    let sibling = client.open(&work);
    assert_ne!(parent, sibling, "ordinary new sessions must not reuse one store");
    let (fork, _) = client.call("session/fork", json!({"sessionId":parent,"cwd":work,"mcpServers":[]}));
    let child = fork["result"]["sessionId"].as_str().expect("fork id").to_owned();
    assert_ne!(parent, child);
    client.call("session/prompt", json!({"sessionId":child,"prompt":[{"type":"text","text":"child work"}]}));
    client.call("session/close", json!({"sessionId":child}));
    let (loaded, updates) =
        client.call("session/load", json!({"sessionId":child,"cwd":work,"mcpServers":[]}));
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert!(updates.iter().filter(|k| k.as_str() == "user_message_chunk").count() >= 2);
    let original = std::fs::read_to_string(&parent).unwrap();
    assert!(original.contains("parent history"));
    assert!(!original.contains("child work"), "fork appended into its parent");
}

#[test]
fn a_pending_session_setup_can_be_cancelled_by_request_id() {
    let (home, work) = scratch("cancel-setup");
    let mut client = Client::start(&home, &work, &stub_provider("ok"), &["--no-session"]);
    client.hello();
    let declaration = mcp(&work, 2.0);
    let id = client.send("session/new", json!({"cwd":work,"mcpServers":[declaration]}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !work.join("started").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let start = Instant::now();
    client.notify("$/cancel_request", json!({"requestId":id}));
    let (cancelled, _) = client.wait(id);
    assert_eq!(cancelled["error"]["code"], -32800);
    assert!(start.elapsed() < Duration::from_secs(1));
    client.open(&work);
}

#[test]
fn an_old_cancelled_turn_cannot_release_a_resumed_instance_of_the_same_session() {
    let (home, work) = scratch("resume-incarnation");
    let mut client = Client::start(&home, &work, &slow_provider("ok", 4000), &[]);
    client.hello();
    let session = client.open(&work);
    let first =
        client.send("session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"first"}]}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !Path::new(&session).is_file() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let closing = client.send("session/close", json!({"sessionId":session}));
    let (resumed, _) = client.call("session/resume", json!({"sessionId":session,"cwd":work,"mcpServers":[]}));
    assert!(resumed.get("result").is_some(), "{resumed}");
    client.wait(closing);
    let (cancelled, _) = client.wait(first);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    let second = client
        .send("session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"second"}]}));
    client.call("session/list", json!({"cwd":work}));
    let start = Instant::now();
    client.notify("session/cancel", json!({"sessionId":session}));
    let (cancelled, _) = client.wait(second);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    assert!(start.elapsed() < Duration::from_secs(1));
}
