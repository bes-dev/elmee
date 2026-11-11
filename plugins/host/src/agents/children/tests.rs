//! Tests for the child runtime module beside this one.
//!
//! A child module, so it still reaches the private items it is testing and
//! is still compiled out of a release build — the only thing it stops doing is
//! competing with the code for the reader.

use super::{super::tests::test_config, *};

#[tokio::test(flavor = "current_thread")]
async fn attachment_metadata_requires_a_live_matching_local_identity() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-attachment-validation"));
            let root = base.join("state");
            let (runtime, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            let identity = runtime.mesh_identity();
            let result = |identity| serde_json::json!({"sessionId":"s", "_meta":{"elmee":{"mesh":identity}}});
            assert!(validate_attachment(&root, &result(identity.clone())).is_ok());
            assert!(validate_attachment(&root, &serde_json::json!({"sessionId":"s"})).is_ok());
            for (key, value) in [
                ("root", serde_json::json!("relative")),
                ("root", serde_json::json!(base)),
                ("name", serde_json::json!("other")),
                ("incarnation", serde_json::json!("old")),
            ] {
                let mut invalid = identity.clone();
                invalid[key] = value;
                assert!(validate_attachment(&root, &result(invalid)).is_err(), "{key}");
            }
            for invalid in [serde_json::Value::Null, serde_json::json!({}), serde_json::json!(false)] {
                assert!(validate_attachment(&root, &result(invalid)).is_err());
            }
            drop(runtime);
            assert!(validate_attachment(&root, &result(identity.clone())).is_err());
            let (replacement, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            assert!(validate_attachment(&root, &result(identity)).is_err());
            assert!(validate_attachment(&root, &result(replacement.mesh_identity())).is_ok());
            drop(replacement);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

fn view(id: &str, name: &str, status: &str, detail: &str) -> TaskView {
    TaskView {
        id: id.into(),
        name: name.into(),
        status: status.into(),
        detail: detail.into(),
        started_at: now(),
        finished_at: TaskStatus::terminal(status).then(now),
        model: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        session: None,
    }
}

/// A child that speaks the protocol: answers the handshake, says its piece,
/// answers the prompt, then waits to be let go of as `--acp` does.
const SPEAKING_CHILD: &str = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}'
read _; echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"the finding"}}}}'
echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","_meta":{"elmee":{"inputTokens":9,"outputTokens":4,"durationMs":100}}}}'
cat > /dev/null"#;

fn shell(script: &str) -> tokio::process::Child {
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Poll the task until it ends, or the test gives up.
async fn settled(runtime: &Runtime, id: &str) -> TaskView {
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let view = runtime.state.lock().unwrap().tasks[id].view.clone();
        if view.finished_at.is_some() {
            return view;
        }
    }
    panic!("the task never ended");
}

fn delivered(runtime: &Runtime, tag: &str) -> Option<String> {
    fs::read_dir(runtime.base.join("inbox").join("parent"))
        .unwrap()
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .find(|event| event.contains(tag))
}

#[test]
fn a_report_is_bounded_without_splitting_a_character() {
    let report = elmee_core::text::clip(&"é".repeat(MAX_TRANSCRIPT_REPORT), MAX_TRANSCRIPT_REPORT);
    assert!(report.len() <= MAX_TRANSCRIPT_REPORT, "a report never exceeds what was asked for");
    assert!(report.ends_with('…'), "and says that something was dropped");
}

#[test]
fn a_task_accepts_exactly_one_terminal_transition() {
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().unwrap().tasks.insert(
        "task".into(),
        Task { view: view("task", "worker", "running", "work"), stop: None, supervisor: None },
    );
    finish_task(&state, "task", TaskStatus::Completed, "done", (6, 4));
    finish_task(&state, "task", TaskStatus::Failed, "late failure", (0, 0));
    let state = state.lock().unwrap();
    assert_eq!(state.tasks["task"].view.status, "completed");
    assert_eq!(state.tasks["task"].view.input_tokens, 6);
}

/// The child's own command line, in the one order that matters: the role
/// first, so nothing it declares can widen what this session grants after it.
#[tokio::test(flavor = "current_thread")]
async fn a_child_is_this_binary_serving_the_protocol_with_the_role_before_the_grant() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-child-args-test"));
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) =
                Runtime::new_at(&test_config(&base, "parent"), base.join("state")).unwrap();
            let args: Vec<String> = runtime
                .child_args("agent-1", "worker", "test/model", Some("reviewer"))
                .into_iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert_eq!(&args[..3], ["--role", "reviewer", "--acp"]);
            let at = |flag: &str| args.iter().position(|arg| arg == flag).map(|at| args[at + 1].clone());
            assert_eq!(at("--model").as_deref(), Some("test/model"));
            assert_eq!(at("--name").as_deref(), Some("worker"));
            assert_eq!(at("--agent-depth").as_deref(), Some("1"));
            assert!(at("--session").is_some_and(|path| path.ends_with("tasks/agent-1.jsonl")));
            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn steering_an_owned_child_does_not_create_a_second_reply_channel() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-owned-message-test"));
            let config = test_config(&base, "parent");
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) = Runtime::new_at(&config, base.join("state")).unwrap();
            runtime.state.lock().unwrap().tasks.insert(
                "task-1".into(),
                Task { view: view("task-1", "worker", "completed", "done"), stop: None, supervisor: None },
            );

            let waited = runtime
                .wait_agents(&ToolCall::new("wait", "wait_agents", r#"{"ids":["task-1"],"timeout_ms":10}"#))
                .await;
            assert!(waited.contains("\"finished_at\""), "{waited}");

            // Make it active for steering: the written envelope must not
            // name a sender, otherwise the child harness owes an extra
            // agent_reply in addition to its canonical completion.
            runtime.state.lock().unwrap().tasks.get_mut("task-1").unwrap().view.status = "running".into();
            let sent = runtime.send_to_agent(&ToolCall::new(
                "send",
                "send_to_agent",
                r#"{"to":"worker","message":"focus on the parser"}"#,
            ));
            let id =
                serde_json::from_str::<serde_json::Value>(&sent).unwrap()["id"].as_str().unwrap().to_owned();
            let envelope: Envelope = serde_json::from_slice(
                &fs::read(runtime.base.join("inbox/worker").join(format!("{id}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(envelope.from, None);
            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

/// Waiting on nothing, with no time to wait, is the status question — and it
/// is the only way to ask it, so there is no listing tool to poll in a loop.
#[tokio::test(flavor = "current_thread")]
async fn waiting_on_no_ids_with_no_timeout_is_the_whole_roster() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-roster-test"));
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) =
                Runtime::new_at(&test_config(&base, "parent"), base.join("state")).unwrap();
            runtime.state.lock().unwrap().tasks.insert(
                "task-1".into(),
                Task {
                    view: view("task-1", "worker", "running", "reading src/parser.rs"),
                    stop: None,
                    supervisor: None,
                },
            );

            let listed =
                runtime.wait_agents(&ToolCall::new("list", "wait_agents", r#"{"timeout_ms":0}"#)).await;
            assert_eq!(listed, runtime.summary(), "{listed}");
            assert!(listed.contains("\"self\":\"parent\"") && listed.contains("worker"), "{listed}");

            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_to_a_name_nobody_answers_to_is_still_delivered() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-absent-message-test"));
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) =
                Runtime::new_at(&test_config(&base, "parent"), base.join("state")).unwrap();

            // Refusing here used to lose a message to a peer that was a
            // moment from claiming its name, or restarting.
            let sent = runtime.send_to_agent(&ToolCall::new(
                "send",
                "send_to_agent",
                r#"{"to":"later","message":"when you get in"}"#,
            ));
            assert!(sent.contains("\"sent\":true"), "{sent}");
            assert!(sent.contains("waits in"), "the model is told nobody is there: {sent}");
            let waiting = fs::read_dir(runtime.base.join("inbox/later")).unwrap().count();
            assert_eq!(waiting, 1, "the record must wait for whoever claims the name");

            let refused = runtime.send_to_agent(&ToolCall::new(
                "bad-send",
                "send_to_agent",
                r#"{"to":"../../escaped","message":"must not publish"}"#,
            ));
            assert!(refused.starts_with("Error: cannot deliver mesh message:"), "{refused}");
            assert!(!base.join("escaped").exists());
            let outside = base.join("outside");
            fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, runtime.base.join("inbox/redirect")).unwrap();
            let refused = runtime.send_to_agent(&ToolCall::new(
                "alias-send",
                "send_to_agent",
                r#"{"to":"redirect","message":"must not follow a filesystem alias"}"#,
            ));
            assert!(refused.starts_with("Error: cannot deliver mesh message:"), "{refused}");
            assert_eq!(fs::read_dir(outside).unwrap().count(), 0);

            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

/// A child that dies before its first event must be reported as failed, with
/// its stderr as the reason. It used to be silence: EOF broke the watch loop
/// with no outcome, report() had nothing to say, and the task sat "starting"
/// forever — invisible to the watchdog, undeliverable to the parent, and
/// unkillable, because the supervisor that stop_agent signals had already
/// ended. Eighteen minutes of spinner over a child that lived milliseconds.
#[tokio::test(flavor = "current_thread")]
async fn a_child_that_dies_at_startup_is_reported_failed_with_its_stderr() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-stillborn"));
            let config = test_config(&base, "parent");
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) = Runtime::new_at(&config, base.join("state")).unwrap();

            // What a config error at startup looks like: one line of stderr,
            // nothing on stdout, exit 1.
            let child = shell("echo 'elmee: configuration error: no role' >&2; exit 1");
            let (_stop, stop_rx) = oneshot::channel();
            runtime.state.lock().unwrap().tasks.insert(
                "task-1".into(),
                Task { view: view("task-1", "worker", "running", "starting"), stop: None, supervisor: None },
            );
            runtime.run_child("task-1".into(), "worker".into(), "look".into(), child, stop_rx);

            let view = settled(&runtime, "task-1").await;
            assert_eq!(view.status, "failed");
            assert!(view.detail.contains("no role"), "stderr is the reason: {}", view.detail);
            // And the parent was told, so the delegation does not just vanish.
            assert!(
                delivered(&runtime, "background_task_failed").is_some(),
                "the failure must reach the parent"
            );
            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

/// A child that reports and then waits, which is what `--acp` does: it stays
/// answerable until its caller lets go of the pipe.
#[tokio::test(flavor = "current_thread")]
async fn a_child_that_stays_reachable_after_reporting_still_delivers_its_report() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-child-test"));
            let config = test_config(&base, "parent");
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) = Runtime::new_at(&config, base.join("state")).unwrap();

            let child = shell(SPEAKING_CHILD);
            let (_stop, stop_rx) = oneshot::channel();
            runtime.state.lock().unwrap().tasks.insert(
                "task-1".into(),
                Task { view: view("task-1", "worker", "running", "starting"), stop: None, supervisor: None },
            );
            runtime.run_child("task-1".into(), "worker".into(), "look".into(), child, stop_rx);

            // Waiting for the child to exit before closing its stdin left
            // both sides idle forever, so the report is what proves the
            // order is right — not merely that nothing panicked.
            let view = settled(&runtime, "task-1").await;
            assert_eq!(view.status, "completed");
            assert_eq!(
                (view.input_tokens, view.output_tokens),
                (9, 4),
                "what it cost, from its own accounting"
            );
            let report = delivered(&runtime, "background_task_completed").expect("the report reached nobody");
            assert!(report.contains("the finding"), "{report}");

            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

/// Any program that speaks the protocol can be delegated to, once the project
/// names it. The parent neither knows nor cares what is on the other end.
#[tokio::test(flavor = "current_thread")]
async fn a_declared_foreign_agent_is_spawned_and_reports_like_any_other() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-foreign-agent"));
            let config = test_config(&base, "parent");
            fs::create_dir_all(base.join(".elmee")).unwrap();
            let declared = serde_json::json!({ "agents": { "echoer": { "command": "sh", "args": ["-c", SPEAKING_CHILD] } } });
            fs::write(base.join(".elmee/agents.json"), declared.to_string()).unwrap();
            let (mut runtime, _events) = Runtime::new_at(&config, base.join("state")).unwrap();

            let refused = runtime
                .spawn_agent(&ToolCall::new("s", "spawn_agent", r#"{"name":"x","task":"look","agent":"nobody"}"#))
                .await;
            assert!(refused.contains("no agent \"nobody\""), "{refused}");

            let started = runtime
                .spawn_agent(&ToolCall::new("s", "spawn_agent", r#"{"name":"helper","task":"look","agent":"echoer"}"#))
                .await;
            let started: serde_json::Value = serde_json::from_str(&started).unwrap_or_else(|_| panic!("{started}"));
            assert_eq!(started["model"], "echoer", "a foreign agent is labelled by its name, not a model");
            let id = started["id"].as_str().unwrap().to_owned();

            // Nothing drains an inbox on the other end, and the protocol takes
            // one prompt at a time: a message to it is refused, not lost.
            let steered = runtime.send_to_agent(&ToolCall::new(
                "m",
                "send_to_agent",
                r#"{"to":"helper","message":"also check the footer"}"#,
            ));
            assert!(steered.contains("no confirmed mesh attachment"), "{steered}");
            assert!(!runtime.base.join("inbox/helper").exists());

            let view = settled(&runtime, &id).await;
            assert_eq!(view.status, "completed", "{}", view.detail);
            assert!(delivered(&runtime, "the finding").is_some());
            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

/// Stopping an agent hangs up on it before killing it. An agent that sees its
/// client go ends what it started; one killed outright cannot, and whatever it
/// had running — a build, a command, agents of its own — carried on with
/// nobody left to end it.
#[tokio::test(flavor = "current_thread")]
async fn a_stopped_agent_is_hung_up_on_and_ends_what_it_started() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-stop-agent"));
            let config = test_config(&base, "parent");
            fs::create_dir_all(base.join(".elmee")).unwrap();
            let marker = base.join("survived");
            // Answers the handshake, starts work of its own, and — like any
            // agent — ends that work when its input closes.
            let script = format!(
                "read _; echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":1}}}}'\n\
                 read _; echo '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":\"s\"}}}}'\n\
                 read _; (sleep 2; touch {}) & work=$!\n\
                 cat > /dev/null; kill $work",
                marker.display()
            );
            let declared =
                serde_json::json!({ "agents": { "worker": { "command": "sh", "args": ["-c", script] } } });
            fs::write(base.join(".elmee/agents.json"), declared.to_string()).unwrap();
            let (mut runtime, _events) = Runtime::new_at(&config, base.join("state")).unwrap();
            let started = runtime
                .spawn_agent(&ToolCall::new(
                    "s",
                    "spawn_agent",
                    r#"{"name":"w","task":"work","agent":"worker"}"#,
                ))
                .await;
            let started: serde_json::Value =
                serde_json::from_str(&started).unwrap_or_else(|_| panic!("{started}"));
            let id = started["id"].as_str().unwrap().to_owned();
            tokio::time::sleep(Duration::from_millis(400)).await;

            let stop = serde_json::json!({ "id": id }).to_string();
            runtime.stop_agent(&ToolCall::new("x", "stop_agent", &stop));
            assert_eq!(settled(&runtime, &id).await.status, "stopped");
            tokio::time::sleep(Duration::from_millis(2200)).await;
            assert!(!marker.exists(), "what the agent had started ran on after the agent was stopped");
            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}
