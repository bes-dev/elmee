#![forbid(unsafe_code)]

//! Headless frontend: one prompt, then exit.
//!
//! `--json` emits every `AgentEvent` as a JSONL record terminated by `result`
//! or `error`; without it only the answer text is printed. This is a log, not
//! a protocol: nothing is read back. A caller that wants to steer an agent
//! while it works, or a parent driving a delegated child, speaks ACP instead.

use std::io::Write;

use elmee_core::{Agent, AgentEvent, Error, Result};
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use elmee_host::{Driver, RuntimeEvent};

/// The final record of a completed run: everything an orchestrator needs to
/// decide what happened without re-reading the event stream.
fn result_record(agent: &Agent, answer: &str, started: std::time::Instant) -> Value {
    let (input, output) = agent.tokens();
    serde_json::json!({
        "type": "result",
        "answer": answer,
        "session": agent.session_path().map(|path| path.display().to_string()),
        "context_chars": agent.context_chars(),
        "input_tokens": input,
        "output_tokens": output,
        "duration_ms": started.elapsed().as_millis(),
        "verification": agent.evidence().level(),
        "required_verification": agent.required_verification(),
        "verification_satisfied": agent.evidence().satisfies(agent.required_verification()),
        "evidence": agent.evidence().entries,
    })
}

fn failure_record(error: &Error) -> Value {
    if error.is_provider_quota() {
        serde_json::json!({
            "type": "paused",
            "reason": "provider_quota",
            "resumable": true,
            "message": error.to_string(),
        })
    } else {
        serde_json::json!({"type": "error", "message": error.to_string()})
    }
}

pub async fn run(
    driver: &mut Driver,
    prompt: String,
    json: bool,
    mut runtime_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
) -> Result<i32> {
    let started = std::time::Instant::now();
    if json {
        announce_protocol();
    }
    // A plugin's command is the same command whichever way the prompt came in.
    let prompt = match driver.command(&prompt) {
        None => prompt,
        Some(Ok(outcome)) => match outcome.prompt {
            Some(prompt) => prompt,
            None if json => {
                emit(&result_record(&driver.agent, &outcome.message, started));
                return Ok(0);
            }
            None => {
                println!("{}", outcome.message);
                return Ok(0);
            }
        },
        Some(Err(error)) => {
            if json {
                emit(&serde_json::json!({"type": "error", "message": error}));
            } else {
                eprintln!("elmee: {error}");
            }
            return Ok(1);
        }
    };
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut on_event: Box<dyn FnMut(AgentEvent) + Send> = if json {
        Box::new(|event| emit(&serde_json::to_value(&event).expect("serializable event")))
    } else {
        Box::new(|event| {
            if let AgentEvent::Text { text } = event {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
        })
    };

    let responder = driver.runtime.responder();
    enum Outcome {
        Turn(Result<String>),
        Signal(i32),
    }
    let outcome = {
        let turn = driver.turn(Some(prompt), &mut on_event);
        tokio::pin!(turn);
        loop {
            tokio::select! {
            result = &mut turn => break Outcome::Turn(result),
            Some(event) = runtime_rx.recv() => match event {
                RuntimeEvent::Interaction { request, .. } => {
                    let id = match &request {
                        elmee_core::InteractionRequest::Questions { id, .. } => id,
                    };
                    if json {
                        emit(&serde_json::json!({
                            "type": "interaction_cancelled",
                            "id": id,
                            "reason": "one-shot mode cannot answer interactive requests"
                        }));
                    }
                    responder.respond(id, elmee_core::InteractionResponse::Cancel);
                }
                RuntimeEvent::Snapshot { agents, name } if json => emit(&serde_json::json!({
                    "type":"runtime", "name":name, "agents":agents
                })),
                RuntimeEvent::Snapshot { .. } | RuntimeEvent::Wake | RuntimeEvent::Child { .. } => {}
            },
            // 130 and 143 are the shell's own codes for SIGINT and SIGTERM; a
            // supervisor reads them to tell "cancelled" from "crashed".
            code = async {
                tokio::select! {
                    _ = sigint.recv() => 130,
                    _ = sigterm.recv() => 143,
                }
            } => break Outcome::Signal(code),
            }
        }
    };
    match outcome {
        Outcome::Turn(Ok(answer)) => {
            if json {
                emit(&result_record(&driver.agent, &answer, started));
            } else {
                println!();
            }
            Ok(0)
        }
        Outcome::Turn(Err(error)) => {
            if json {
                emit(&failure_record(&error));
            } else {
                eprintln!("elmee: {error}");
            }
            Ok(if error.is_provider_quota() { 75 } else { 1 })
        }
        Outcome::Signal(code) => {
            if json {
                emit(&serde_json::json!({"type": "error", "message": "aborted by signal"}));
            } else {
                eprintln!("aborted");
            }
            Ok(code)
        }
    }
}

/// The shape of the JSONL stream, so a consumer can tell what it is reading.
///
/// Announced as a record rather than assumed, and first, because every reader
/// in this codebase already ignores a `type` it does not know — which is what
/// makes adding one safe, and what makes not having had one a gap rather than
/// a break.
pub const PROTOCOL_VERSION: u32 = 1;

fn announce_protocol() {
    emit(&serde_json::json!({
        "type": "protocol",
        "version": PROTOCOL_VERSION,
        "elmee": env!("CARGO_PKG_VERSION"),
    }));
}

fn emit(value: &Value) {
    println!("{value}");
    let _ = std::io::stdout().flush();
}
