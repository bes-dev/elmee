//! The binary as a caller meets it: arguments, the JSONL protocol, exit codes.
//!
//! Everything else in this project is tested through a library call, which
//! leaves the layer that turns a command line into a run — argument parsing,
//! model resolution, the headless protocol, the driver — covered by nothing.
//! These run the real executable against a stub provider, so a mistake in that
//! layer fails here rather than in someone's terminal.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// A request body as the provider received it, once the run has ended.
type Observed = Arc<Mutex<Option<serde_json::Value>>>;

fn stub_provider(answer: &str) -> (String, Observed, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().expect("an address").to_string();
    let observed: Observed = Arc::new(Mutex::new(None));
    let captured = observed.clone();
    let body = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": answer}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 3},
    })
    .to_string();
    let handle = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else { return };
        *captured.lock().unwrap() = serde_json::from_slice(&read_http_body(&mut socket)).ok();
        let _ = socket.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    });
    (address, observed, handle)
}

fn read_http_body(socket: &mut TcpStream) -> Vec<u8> {
    socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = socket.read(&mut buffer).unwrap_or(0);
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
        });
        if let Some(length) = length
            && request.len() >= body_start + length
        {
            return request[body_start..body_start + length].to_vec();
        }
    }
    request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map_or_else(Vec::new, |header_end| request[header_end + 4..].to_vec())
}

fn elmee(home: &std::path::Path, work: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_elmee"));
    // A private HOME so the run cannot read the operator's models.json,
    // remembered model, or credentials, and cannot write to their sessions.
    command.env("HOME", home).env_remove("ELMEE_MODEL").current_dir(work);
    command
}

fn scratch(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("elmee-cli-{name}-{}", std::process::id()));
    let (home, work) = (root.join("home"), root.join("work"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    (home, work)
}

#[test]
fn a_headless_run_announces_its_protocol_and_reports_a_result() {
    let (home, work) = scratch("run");
    let (address, observed, server) = stub_provider("elmee-e2e-ok");
    let output = elmee(&home, &work)
        .args(["--json", "--no-session", "-m", "ollama/stub", "hi"])
        .env("ELMEE_BASE_URL", format!("http://{address}/v1"))
        .env("ELMEE_API_KEY", "stub")
        .output()
        .expect("elmee runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Asserted before the stub is joined: a run that never connected leaves
    // the server thread blocked in accept() forever, and joining it first
    // turned that failure into a hang nothing ever reported.
    assert!(output.status.success(), "exit {:?}\n{stdout}", output.status.code());
    let _ = server.join();

    let request = observed.lock().unwrap().clone().unwrap();
    assert!(!request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "ask_user"));
    assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "view_image"));

    let records: Vec<serde_json::Value> =
        stdout.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let first = records.first().expect("at least one record");
    assert_eq!(first["type"], "protocol", "the stream says what it is, first: {stdout}");
    assert_eq!(first["version"], 1);
    assert_eq!(first["elmee"], env!("CARGO_PKG_VERSION"));

    let result = records.iter().find(|record| record["type"] == "result").expect("a result");
    assert_eq!(result["answer"], "elmee-e2e-ok");
    assert_eq!(result["input_tokens"], 11, "usage is carried through, not guessed");
    assert!(result["duration_ms"].is_number());
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[test]
fn a_goal_cannot_hide_its_own_completion_tool() {
    let (home, work) = scratch("goal-tools");
    let output = elmee(&home, &work)
        .args(["--no-session", "--goal", "finish it", "--tools", "read", "hi"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--goal requires goal_done"));
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[test]
fn filtering_out_goal_done_also_removes_the_goal_command() {
    let (home, work) = scratch("no-goal-command");
    let output = elmee(&home, &work)
        .args(["--explain-plugins", "--tools", "read", "-m", "ollama/stub"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let composition = String::from_utf8_lossy(&output.stdout);
    assert!(!composition.contains("goal"), "{composition}");
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// The prompt this product sends, as the provider receives it, against a file
/// checked in beside this one.
///
/// A byte ceiling bounds how much prose the prompt may carry; it cannot show
/// what that prose became once the environment and the project were folded
/// around it, and it cannot see a fragment appended on the way to the wire —
/// which is exactly how a plugin's prompt contract used to reach the model.
/// The snapshot shows both, as a diff a person reads.
#[test]
fn the_system_prompt_on_the_wire_matches_its_checked_in_snapshot() {
    let (home, work) = scratch("prompt");
    let (address, observed, server) = stub_provider("snapshot");
    let output = elmee(&home, &work)
        .args(["--json", "--no-session", "-m", "ollama/stub", "hi"])
        .env("ELMEE_BASE_URL", format!("http://{address}/v1"))
        .env("ELMEE_API_KEY", "stub")
        .output()
        .expect("elmee runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let _ = server.join();

    let request = observed.lock().unwrap().clone().expect("the provider received a request");
    let system = request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().find(|message| message["role"] == "system"))
        .and_then(|message| message["content"].as_str())
        .expect("a system message")
        .to_owned();
    // What belongs to the machine rather than to the prompt: where the run
    // happened, and what it ran on. Replaced rather than regenerated, so the
    // snapshot is one file everywhere instead of one per developer.
    let rendered = system
        .replace(&work.canonicalize().unwrap().display().to_string(), "<workspace>")
        .replace(std::env::consts::OS, "<platform>");
    assert_eq!(
        rendered,
        include_str!("system-prompt.txt").trim_end_matches('\n'),
        "the rendered system prompt changed; read the diff, then update apps/elmee/tests/system-prompt.txt"
    );
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[test]
fn a_run_with_no_model_anywhere_says_so_and_names_the_remedies() {
    let (home, work) = scratch("nomodel");
    // No model list is compiled in any more, so what this machine can offer is
    // what an endpoint last told it. That is where the remedy's example has to
    // come from.
    std::fs::create_dir_all(home.join(".elmee")).unwrap();
    std::fs::write(home.join(".elmee/model-catalogue.json"), r#"{"codex/gpt-5.6-sol":272000}"#).unwrap();
    let output = elmee(&home, &work).args(["--no-session", "hi"]).output().expect("elmee runs");
    assert!(!output.status.success(), "a run that cannot pick a model must not report success");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-m") && stderr.contains("ELMEE_MODEL"), "{stderr}");
    assert!(stderr.contains("defaultModel"), "{stderr}");
    assert!(stderr.contains("remembered"), "and the way out of it: {stderr}");
    // Naming a remedy without naming a value to give it sends a person to the
    // documentation for a word this process already knows.
    assert!(stderr.contains("this machine has: "), "and what to actually pass: {stderr}");
    assert!(stderr.contains("codex/gpt-5.6-sol"), "as a spec the catalogue reported: {stderr}");
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[test]
fn the_model_used_is_remembered_for_the_next_bare_run() {
    let (home, work) = scratch("remember");
    let (address, _, server) = stub_provider("remembered");
    let run = |extra: &[&str]| {
        let mut command = elmee(&home, &work);
        command
            .args(["--json", "--no-session"])
            .args(extra)
            .env("ELMEE_BASE_URL", format!("http://{address}/v1"))
            .env("ELMEE_API_KEY", "stub");
        command.output().expect("elmee runs")
    };
    // Nothing waits on the turn working out: a model this run was told to use
    // is this machine's model from now on. The note is there before the answer
    // is, which is what a person who switches models and quits expects.
    let unanswered = elmee(&home, &work)
        .args(["--json", "--no-session", "-m", "ollama/stub", "--max-steps", "0", "hi"])
        .env("ELMEE_BASE_URL", "http://127.0.0.1:1/v1")
        .env("ELMEE_API_KEY", "stub")
        .output()
        .expect("elmee runs");
    let _ = unanswered;
    let note = std::fs::read_to_string(home.join(".elmee/last-model.json")).expect("a note");
    assert!(note.contains("ollama/stub"), "a choice is written down whether or not it answered: {note}");

    let first = run(&["-m", "ollama/stub", "hi"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let _ = server.join();
    let note = std::fs::read_to_string(home.join(".elmee/last-model.json")).expect("a note");
    assert!(note.contains("ollama/stub"), "{note}");

    // Without -m the second run resolves from the note rather than failing;
    // the stub has already served its one answer, so what is asserted is that
    // it got as far as asking the provider at all.
    let bare = run(&["hi"]);
    let stderr = String::from_utf8_lossy(&bare.stderr);
    assert!(!stderr.contains("no model"), "a remembered model must resolve: {stderr}");
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[test]
fn unknown_arguments_are_refused_and_help_is_not_an_error() {
    let (home, work) = scratch("args");
    let bad = elmee(&home, &work).args(["--not-a-flag"]).output().expect("elmee runs");
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown option"));

    let help = elmee(&home, &work).arg("--help").output().expect("elmee runs");
    assert!(help.status.success(), "--help is an answer, not a failure");
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--model"), "{text}");

    let version = elmee(&home, &work).arg("--version").output().expect("elmee reports its version");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("elmee {}", env!("CARGO_PKG_VERSION"))
    );

    let marker = home.join("mcp-was-started");
    std::fs::create_dir_all(home.join(".elmee")).unwrap();
    std::fs::write(
        home.join(".elmee/mcp.json"),
        serde_json::to_vec(&serde_json::json!({
            "mcpServers": {"tripwire": {
                "command": "sh",
                "args": ["-c", format!("touch {}", marker.display())]
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let graph = elmee(&home, &work)
        .args(["--explain-plugins", "-m", "ollama/stub"])
        .output()
        .expect("elmee explains its composition");
    let graph_text = String::from_utf8_lossy(&graph.stdout);
    assert!(graph.status.success(), "{}", String::from_utf8_lossy(&graph.stderr));
    assert!(!marker.exists(), "explaining a graph activated its MCP component");
    assert!(!home.join(".elmee/sessions").exists(), "explaining a graph opened a session");
    assert!(graph_text.contains("state: resolved"), "{graph_text}");
    assert!(graph_text.contains("tool exec: tools"), "{graph_text}");
    // Four subagent verbs, and one of them is not a listing tool to poll.
    for verb in ["spawn_agent", "wait_agents", "send_to_agent", "stop_agent"] {
        assert!(graph_text.contains(&format!("tool {verb}:")), "{verb} is missing:\n{graph_text}");
    }
    assert!(!graph_text.contains("list_agents"), "list_agents folded into wait_agents:\n{graph_text}");
    assert!(!graph_text.contains("tool send_message:"), "{graph_text}");
    assert!(graph_text.contains("middleware"), "{graph_text}");
    assert!(
        !graph_text.contains("Frontend")
            && !graph_text.contains("plugins: headless")
            && !graph_text.contains("tui"),
        "presentation belongs to the application, not the agent graph: {graph_text}"
    );

    let restricted = elmee(&home, &work)
        .args(["--explain-plugins", "--no-session", "-m", "ollama/stub", "--tools", "read"])
        .output()
        .expect("elmee explains a restricted composition");
    let restricted_text = String::from_utf8_lossy(&restricted.stdout);
    assert!(restricted.status.success(), "{}", String::from_utf8_lossy(&restricted.stderr));
    assert!(restricted_text.contains("tool read: tools"), "{restricted_text}");
    for forbidden in ["tool exec:", "tool web_fetch:", "tool web_search:", "tool mcp__"] {
        assert!(!restricted_text.contains(forbidden), "{forbidden} escaped --tools:\n{restricted_text}");
    }

    let typo = elmee(&home, &work)
        .args(["--explain-plugins", "--no-session", "-m", "ollama/stub", "--tools", "raed"])
        .output()
        .expect("elmee rejects a misspelled tool");
    assert!(!typo.status.success());
    let typo_error = String::from_utf8_lossy(&typo.stderr);
    assert!(typo_error.contains("unknown tool") && typo_error.contains("raed"), "{typo_error}");
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// A role is only real if naming it starts an agent.
///
/// The built-in `research` role expanded to a permission flag that stopped
/// existing when the policy layer was removed, so `--role research` died at
/// argument parsing — while its unit test went on passing, because it checked
/// the expansion and never ran it. Anything a role can emit has to survive the
/// parser, and the only way to know is to launch.
#[test]
fn naming_a_built_in_role_starts_an_agent() {
    let (home, work) = scratch("roles");
    for role in ["research", "workspace"] {
        let (address, _, server) = stub_provider("elmee-role-ok");
        let output = elmee(&home, &work)
            .args(["--no-session", "-m", "ollama/stub", "--role", role, "hi"])
            .env("ELMEE_BASE_URL", format!("http://{address}/v1"))
            .env("ELMEE_API_KEY", "stub")
            .output()
            .expect("elmee runs");
        let (out, err) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success() && !err.contains("configuration error"), "--role {role}: {err}{out}");
        assert!(out.contains("elmee-role-ok"), "--role {role} never reached the model: {err}{out}");
        let _ = server.join();
    }
}

/// The hook that runs when a session is over, and the one thing it must carry.
///
/// Everything a distillation wants to know — how many turns, what was edited,
/// what it cost — is in the transcript. So the payload names the transcript,
/// and the value of the hook is that a script does not have to be told
/// separately that a session happened or where it went.
#[test]
fn a_finished_session_runs_the_project_hook_and_names_its_transcript() {
    let (home, work) = scratch("session-hook");
    let seen = work.join("seen.json");
    std::fs::create_dir_all(work.join(".elmee")).unwrap();
    std::fs::write(
        work.join(".elmee/hooks.json"),
        format!(r#"{{"after_session":[{{"command":"sh","args":["-c","cat > {}"]}}]}}"#, seen.display()),
    )
    .unwrap();
    let transcript = work.join("run.jsonl");

    let (address, _, server) = stub_provider("elmee-hook-ok");
    let output = elmee(&home, &work)
        .args(["-m", "ollama/stub", "--session"])
        .arg(&transcript)
        .arg("hi")
        .env("ELMEE_BASE_URL", format!("http://{address}/v1"))
        .env("ELMEE_API_KEY", "stub")
        .output()
        .expect("elmee runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let _ = server.join();

    let payload: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&seen).expect("the hook ran")).expect("it was sent JSON");
    assert_eq!(payload["event"], "after_session");
    assert_eq!(payload["session"], transcript.display().to_string());
}
