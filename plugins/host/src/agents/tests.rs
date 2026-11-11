//! Tests for the host module beside this one.
//!
//! A child module, so it still reaches the private items it is testing and
//! is still compiled out of a release build — the only thing it stops doing is
//! competing with the code for the reader.

use std::time::Duration;

use elmee_plugin_mesh::Envelope;

use super::*;

#[tokio::test(flavor = "current_thread")]
async fn discovery_failure_is_visible_and_snapshot_recovers() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-discovery-error"));
            let root = base.join("state");
            let (runtime, mut events) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            while events.try_recv().is_ok() {}
            let damaged = root.join("agents/broken.json");
            fs::write(&damaged, b"{").unwrap();
            assert!(runtime.participants().is_err());
            let summary: serde_json::Value = serde_json::from_str(&runtime.summary()).unwrap();
            assert!(summary.get("error").is_some());
            assert!(summary.get("agents").is_none(), "failed discovery is not an empty successful list");
            runtime.emit_snapshot();
            assert!(runtime.state.lock().unwrap().discovery_fault.is_some());
            assert!(events.try_recv().is_err(), "failed discovery must not publish a normal snapshot");
            fs::remove_file(damaged).unwrap();
            runtime.emit_snapshot();
            assert!(runtime.state.lock().unwrap().discovery_fault.is_none());
            assert!(matches!(events.try_recv(), Ok(RuntimeEvent::Snapshot { .. })));
            drop(runtime);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[cfg(unix)]
async fn runtime_shutdown_releases_name_even_with_a_duplicated_descriptor() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-claim-duplicate"));
            let root = base.join("state");
            let (runtime, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            // A duplicate shares the open file description, as does an inherited
            // descriptor between fork and exec. CLOEXEC does not close it at fork.
            let inherited = runtime._name_lock.try_clone().unwrap();
            drop(runtime);
            let (replacement, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            assert_eq!(replacement.name, "worker");
            drop(inherited);
            // Closing the old descriptor must not release the replacement lock.
            assert!(mesh::claim_exact_name(&root, "worker").unwrap().is_none());
            drop(replacement);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_shutdown_waits_for_inflight_reporter_publication() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-reporter-shutdown-race"));
            let root = base.join("state");
            let (runtime, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            let reporter = runtime.reporter();
            runtime.state.lock().unwrap().owed.push(Envelope {
                id: "inflight".into(),
                prompt: "work".into(),
                from: Some("sender".into()),
                ..Default::default()
            });
            // Hold only the reply recipient's publication gate. Completion history
            // can be persisted, but publication cannot finish until this is released.
            fs::create_dir_all(root.join("delivery-locks")).unwrap();
            let gate = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(root.join("delivery-locks/sender"))
                .unwrap();
            gate.lock().unwrap();
            let reply = std::thread::spawn(move || reporter.reply("completed before closing"));
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !root.join("history/worker/inflight.json").exists() {
                assert!(std::time::Instant::now() < deadline, "reply never reached publication");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(runtime.state.try_lock().is_err(), "publication must still hold the state gate");
            let shutdown = runtime.shutdown.clone();
            let observed_root = root.clone();
            let release = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !shutdown.load(Ordering::Relaxed) {
                    assert!(std::time::Instant::now() < deadline, "Runtime drop did not start");
                    std::thread::sleep(Duration::from_millis(5));
                }
                // Drop has started but cannot release the name or remove presence
                // until this in-flight publication is complete.
                assert!(mesh::claim_exact_name(&observed_root, "worker").unwrap().is_none());
                assert!(observed_root.join("agents/worker.json").exists());
                drop(gate);
            });
            drop(runtime);
            release.join().unwrap();
            reply.join().unwrap();
            assert!(!root.join("agents/worker.json").exists());
            let responses = mesh::inbox(&root, "sender").unwrap();
            assert_eq!(responses.len(), 1);
            assert_eq!(responses[0].1.prompt, "completed before closing");
            assert!(mesh::claim_exact_name(&root, "worker").unwrap().is_some());
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn mailbox_reports_one_read_fault_and_recovers_without_restarting() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-inbox-recovery"));
            fs::create_dir_all(&base).unwrap();
            let (runtime, _) = Runtime::new_at(&test_config(&base, "worker"), base.join("state")).unwrap();
            let mailbox = runtime.base.join("inbox").join(&runtime.name);
            fs::remove_dir(&mailbox).unwrap();
            fs::write(&mailbox, b"not a directory").unwrap();
            let faults = || {
                runtime
                    .inbox
                    .snapshot()
                    .iter()
                    .filter(|event| event.prompt.contains("mesh inbox inspection failed"))
                    .count()
            };
            tokio::time::timeout(Duration::from_secs(3), async {
                while faults() == 0 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(faults(), 1, "one persistent fault must not flood model context");
            assert!(!runtime.mailbox.as_ref().unwrap().is_finished());
            fs::remove_file(&mailbox).unwrap();
            assert!(mesh::persist_message(
                &runtime.base,
                &runtime.name,
                BackgroundEvent { id: "after-repair".into(), prompt: "hello".into() },
                None
            ));
            tokio::time::timeout(Duration::from_secs(3), async {
                while !runtime.state.lock().unwrap().pending_messages.contains_key("after-repair") {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(faults(), 1);
            drop(runtime);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn mesh_startup_requires_a_claim_before_publishing_or_recovering() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-mesh-claim-failure"));
            let root = base.join("state");
            fs::create_dir_all(root.join("inbox/worker")).unwrap();
            // Deterministic filesystem failure, including for a privileged user:
            // no candidate claim can be opened under a regular file.
            fs::write(root.join("locks"), b"not a directory").unwrap();
            let request = br#"{"id":"queued","prompt":"must remain untouched","from":"sender"}"#;
            fs::write(root.join("inbox/worker/queued.json"), request).unwrap();
            let result = Runtime::new_at(&test_config(&base, "worker"), root.clone());
            assert!(
                matches!(result, Err(ref error) if error.to_string().contains("could not claim mesh name"))
            );
            assert_eq!(fs::read(root.join("inbox/worker/queued.json")).unwrap(), request);
            assert_eq!(fs::read_dir(root.join("agents")).unwrap().count(), 0);
            assert!(!root.join("history").exists());
            assert!(!root.join("inbox/sender").exists());
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

fn inject_mesh(runtime: &mut Runtime, id: &str) -> Envelope {
    let request = Envelope {
        id: id.into(),
        prompt: format!("work {id}"),
        from: Some("sender".into()),
        ..Default::default()
    };
    assert!(mesh::persist_message(
        &runtime.base,
        &runtime.name,
        BackgroundEvent { id: request.id.clone(), prompt: request.prompt.clone() },
        request.from.as_deref()
    ));
    let mut state = runtime.state.lock().unwrap();
    state.seen.insert(request.id.clone());
    state.pending_messages.insert(request.id.clone(), request.clone());
    runtime.inbox.defer(BackgroundEvent { id: request.id.clone(), prompt: request.agent_input() });
    request
}

fn consume_initial_context(runtime: &mut Runtime) {
    let reporter = runtime.reporter();
    let _turn = reporter.turn_replies(true);
    let events = runtime.background_events();
    runtime.ack_background(&events.into_iter().map(|event| event.id).collect::<Vec<_>>());
}

#[tokio::test(flavor = "current_thread")]
async fn mesh_requests_have_separate_turns_and_never_join_user_work() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-mesh-turn-ownership"));
            fs::create_dir_all(&base).unwrap();
            let (mut runtime, mut events) =
                Runtime::new_at(&test_config(&base, "worker"), base.join("state")).unwrap();
            consume_initial_context(&mut runtime);
            let initial = runtime.background_events();
            runtime.ack_background(&initial.into_iter().map(|event| event.id).collect::<Vec<_>>());
            let first = inject_mesh(&mut runtime, "first");
            let second = inject_mesh(&mut runtime, "second");
            let reporter = runtime.reporter();
            let mut user = reporter.turn_replies(true);
            assert!(runtime.background_events().is_empty());
            assert!(!mesh::execution_started(&runtime.base, &runtime.name, &first).unwrap());
            assert!(!mesh::execution_started(&runtime.base, &runtime.name, &second).unwrap());
            // A user's answer must not leak into an unrelated peer response.
            user.finish("private user answer", true);
            while events.try_recv().is_ok() {}
            drop(user);
            assert!(matches!(events.try_recv(), Ok(RuntimeEvent::Wake)));
            assert!(mesh::inbox(&runtime.base, "sender").unwrap().is_empty());

            let mut turn = reporter.turn_replies(false);
            assert_eq!(
                runtime.background_events().iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
                ["first"]
            );
            assert!(
                runtime.background_events().is_empty(),
                "next model step must not absorb the second request"
            );
            assert!(!mesh::execution_started(&runtime.base, &runtime.name, &second).unwrap());
            turn.finish("first answer", true);
            drop(turn);
            let results = mesh::inbox(&runtime.base, "sender").unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].1.in_reply_to.as_deref(), Some("first"));
            assert_eq!(results[0].1.prompt, "first answer");

            let turn = reporter.turn_replies(false);
            assert_eq!(
                runtime.background_events().iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
                ["second"]
            );
            drop(turn);
            let results = mesh::inbox(&runtime.base, "sender").unwrap();
            assert_eq!(results.len(), 2);
            let cancelled = &results
                .iter()
                .find(|(_, result)| result.in_reply_to.as_deref() == Some("second"))
                .unwrap()
                .1;
            assert_eq!(cancelled.status, Some(mesh::CompletionStatus::Cancelled));
            assert!(!cancelled.prompt.contains("first answer"));
            drop(runtime);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn mesh_outcomes_belong_only_to_consumed_requests_and_dropped_turns_cancel() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-mesh-outcomes"));
            fs::create_dir_all(&base).unwrap();
            let (mut runtime, _) =
                Runtime::new_at(&test_config(&base, "worker"), base.join("state")).unwrap();
            consume_initial_context(&mut runtime);
            let initial = runtime.background_events();
            runtime.ack_background(&initial.into_iter().map(|event| event.id).collect::<Vec<_>>());
            let first = inject_mesh(&mut runtime, "first");
            assert_eq!(runtime.background_events().len(), 1);
            let second = inject_mesh(&mut runtime, "second");
            runtime.reporter().turn_replies(false).finish("provider failed", false);
            let results = mesh::inbox(&runtime.base, "sender").unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].1.in_reply_to.as_deref(), Some(first.id.as_str()));
            assert_eq!(results[0].1.status, Some(mesh::CompletionStatus::Failed));
            assert_eq!(mesh::inbox(&runtime.base, &runtime.name).unwrap()[0].1, second);
            // Poll a turn to its first suspension, then drop it as a frontend abort
            // would. No timing assumptions or error-text classification involved.
            tokio::select! {
                biased;
                _ = async {
                    let _replies = runtime.reporter().turn_replies(false);
                    assert_eq!(runtime.background_events().len(), 1);
                    std::future::pending::<()>().await;
                } => unreachable!(),
                _ = async {} => {},
            }
            let results = mesh::inbox(&runtime.base, "sender").unwrap();
            assert_eq!(results.len(), 2);
            let cancelled = results
                .iter()
                .find(|(_, envelope)| envelope.in_reply_to.as_deref() == Some("second"))
                .unwrap();
            assert_eq!(cancelled.1.status, Some(mesh::CompletionStatus::Cancelled));
            assert!(mesh::inbox(&runtime.base, &runtime.name).unwrap().is_empty());
            assert!(runtime.state.lock().unwrap().owed.is_empty());
            drop(runtime);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn mesh_delivery_retries_without_a_new_turn_and_keeps_the_original_failure() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-mesh-retry"));
            fs::create_dir_all(&base).unwrap();
            let (mut runtime, _) =
                Runtime::new_at(&test_config(&base, "worker"), base.join("state")).unwrap();
            let request = inject_mesh(&mut runtime, "retry");
            runtime.background_events();
            // Fail before a receipt can be persisted, not just during its delivery.
            let history = runtime.base.join("history");
            fs::write(&history, "blocked").unwrap();
            runtime.reporter().turn_replies(false).finish("original failure", false);
            assert!(runtime.state.lock().unwrap().owed.is_empty());
            assert_eq!(runtime.state.lock().unwrap().deliveries.len(), 1);
            runtime.ack_background(std::slice::from_ref(&request.id));
            assert_eq!(mesh::inbox(&runtime.base, &runtime.name).unwrap().len(), 1);
            // A later human answer must not replace the unsaved earlier result.
            runtime.reporter().reply("unrelated later answer");
            fs::remove_file(history).unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                while !runtime.state.lock().unwrap().deliveries.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let results = mesh::inbox(&runtime.base, "sender").unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].1.prompt, "original failure");
            assert_eq!(results[0].1.status, Some(mesh::CompletionStatus::Failed));
            assert_eq!(results[0].1.in_reply_to.as_deref(), Some(request.id.as_str()));
            assert!(mesh::inbox(&runtime.base, &runtime.name).unwrap().is_empty());
            assert!(runtime.state.lock().unwrap().seen.is_empty());
            drop(runtime);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[test]
fn every_host_tool_declares_a_schema_that_parses() {
    // These schemas are string literals built by hand, so a stray comma
    // left behind by an edit is a panic at startup rather than a failure
    // here — which is exactly what happened once.
    let tools = host_tools(None);
    assert!(!tools.is_empty());
    for tool in tools {
        let function = &tool["function"];
        assert!(function["name"].is_string(), "{tool}");
        assert!(function["parameters"]["type"] == "object", "{tool}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_typed_during_a_turn_reaches_the_agent_without_the_worker() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-steer-test"));
            fs::create_dir_all(&base).unwrap();
            let (mut runtime, mut events) =
                Runtime::new_at(&test_config(&base, "solo"), base.join("state")).unwrap();
            // Identity context does not constitute work on an idle runtime.
            let start = runtime.background_events();
            assert!(start.is_empty());
            runtime.ack_background(&start.into_iter().map(|event| event.id).collect::<Vec<_>>());

            let steerer = runtime.steerer();
            steerer.steer("use the other parser");
            steerer.steer("   ");

            let delivered = runtime.background_events();
            assert_eq!(
                delivered.len(),
                2,
                "first real work carries identity; an empty line is not a message"
            );
            assert!(delivered[0].prompt.contains("<agent_identity"));
            assert!(delivered[1].prompt.contains("<user_message"), "{:?}", delivered[1].prompt);
            assert!(delivered[1].prompt.contains("use the other parser"));

            // A turn that ended between the keypress and the write would
            // leave nobody to read it, so the agent is woken either way.
            let woken = std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, RuntimeEvent::Wake));
            assert!(woken, "an idle agent must be told there is something to read");

            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

pub(super) fn test_config(cwd: &Path, name: &str) -> Options {
    Options {
        cwd: cwd.to_path_buf(),
        name: Some(name.into()),
        model: "test/model".into(),
        depth: 0,
        require_verification: elmee_core::evidence::VerificationLevel::Unverified,
        max_tokens: 0,
        token_ledger: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reporter_cannot_publish_after_runtime_shutdown_or_overwrite_a_new_owner() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-stale-reporter"));
            let root = base.join("state");
            let (runtime, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            let reporter = runtime.reporter();
            runtime.state.lock().unwrap().owed.push(Envelope {
                id: "stale-reply".into(),
                prompt: "old work".into(),
                from: Some("sender".into()),
                ..Default::default()
            });
            drop(runtime);
            reporter.set_status("working");
            reporter.reply("late answer");
            assert!(!root.join("agents/worker.json").exists());
            assert!(!root.join("history/worker/stale-reply.json").exists());
            assert!(mesh::inbox(&root, "sender").unwrap().is_empty());
            let (replacement, _) = Runtime::new_at(&test_config(&base, "worker"), root.clone()).unwrap();
            assert_eq!(replacement.name, "worker");
            let presence = fs::read(root.join("agents/worker.json")).unwrap();
            reporter.set_status("stale");
            reporter.reply("still late");
            assert_eq!(fs::read(root.join("agents/worker.json")).unwrap(), presence);
            drop(replacement);
            fs::remove_dir_all(base).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_child_holding_its_own_name_is_one_participant_and_not_two() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let base = std::env::temp_dir().join(unique("elmee-participants-test"));
            fs::create_dir_all(&base).unwrap();
            let (runtime, _events) =
                Runtime::new_at(&test_config(&base, "parent"), base.join("state")).unwrap();

            // A running child is both: it holds a name and publishes a
            // registration, and this session is answerable for it. Two
            // lists listed it twice; one list must not.
            let (name, _held) = mesh::claim_available_name(&runtime.base, "worker").unwrap();
            runtime.reporter().set_status("busy");
            elmee_plugin_atomic::write_json(
                &runtime.base.join("agents").join(format!("{name}.json")),
                &Registration {
                    name: name.clone(),
                    pid: std::process::id(),
                    model: "test/model".into(),
                    status: "busy".into(),
                },
            )
            .unwrap();
            let view = TaskView {
                id: "task-1".into(),
                name: name.clone(),
                status: "running".into(),
                detail: "reading src/parser.rs".into(),
                started_at: elmee_core::now(),
                finished_at: None,
                model: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                session: None,
            };
            runtime
                .state
                .lock()
                .unwrap()
                .tasks
                .insert("task-1".into(), Task { view, stop: None, supervisor: None });

            let agents = runtime.participants().unwrap();
            let listed: Vec<_> = agents.iter().filter(|agent| agent.name == name).collect();
            assert_eq!(listed.len(), 1, "one agent, one entry: {:?}", agents.len());
            let child = listed[0];
            assert!(child.is_live_task(), "this session started it, so it carries its task");
            assert_eq!(child.task.as_ref().unwrap().detail, "reading src/parser.rs");
            assert_eq!(child.model, "test/model", "and keeps what it published about itself");

            drop(runtime);
            let _ = fs::remove_dir_all(base);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn project_peers_exchange_wake_up_messages() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let cwd = std::env::temp_dir().join(unique("elmee-mesh-test"));
            fs::create_dir_all(&cwd).unwrap();
            let config = |name: &str| test_config(&cwd, name);
            let state_dir = cwd.join("state");
            let (a, _a_rx) = Runtime::new_at(&config("alice"), state_dir.clone()).unwrap();
            let (mut b, mut b_rx) = Runtime::new_at(&config("bob"), state_dir).unwrap();
            let output = a.send_to_agent(&ToolCall::new(
                "send",
                "send_to_agent",
                r#"{"to":"bob","message":"review auth"}"#,
            ));
            assert!(output.contains("\"sent\":true"));
            tokio::time::timeout(Duration::from_secs(2), async {
                while !matches!(b_rx.recv().await, Some(RuntimeEvent::Wake)) {}
            })
            .await
            .unwrap();
            let messages = b.background_events();
            assert!(messages.iter().any(|event| event.prompt.contains("review auth")));
            b.ack_background(&messages.into_iter().map(|event| event.id).collect::<Vec<_>>());

            // Bob answers by finishing his turn; the harness returns the
            // answer to whoever asked, because the model does not.
            b.reporter().reply("auth looks fine");
            let replies: Vec<_> = fs::read_dir(a.base.join("inbox").join("alice"))
                .unwrap()
                .flatten()
                .filter_map(|entry| fs::read(entry.path()).ok())
                .filter_map(|data| serde_json::from_slice::<Envelope>(&data).ok())
                .collect();
            assert_eq!(replies.len(), 1, "alice is owed exactly one answer");
            assert!(replies[0].prompt.contains("auth looks fine"));
            assert_eq!(replies[0].from.as_deref(), Some("bob"));
            assert!(replies[0].in_reply_to.is_some());
            assert!(!replies[0].expects_reply(), "results must not cause reply loops");
            // Answering once settles the debt.
            b.reporter().reply("and again");
            assert_eq!(fs::read_dir(a.base.join("inbox").join("alice")).unwrap().count(), 1);

            let base = b.base.clone();
            drop((a, b));
            let _ = fs::remove_dir_all(base);
            let _ = fs::remove_dir_all(cwd);
        })
        .await;
}
