//! The frontend worker owns both the agent and its project runtime. Runtime events are
//! selected even during a model turn, so a child's questions and status updates
//! never block rendering or the main agent.

use std::path::PathBuf;

use elmee_core::{
    AgentEvent, InteractionResponse,
    message::Message,
    session::{Branch, Prompt},
};
use tokio::sync::mpsc;

use elmee_host::{Driver, Participant, Responder, RuntimeEvent, Steerer, session::Restore};

pub enum Cmd {
    Prompt(String),
    Compact,
    /// What was asked on this branch, to choose where to go back to.
    RewindList,
    Rewind {
        prompt: u64,
        restore: Restore,
    },
    /// The lines of conversation this session holds, to choose one.
    Branches,
    Checkout(u64),
    Mcp,
    Clear,
    SetModel {
        spec: String,
    },
    SetReasoningEffort(Option<elmee_core::provider::ReasoningEffort>),
    NewSession(Option<PathBuf>),
    OpenSession(PathBuf),
    Plugin {
        name: String,
        arguments: String,
    },
}

pub enum Out {
    Started,
    Event(AgentEvent),
    Done {
        error: Option<String>,
        aborted: bool,
    },
    Info(String),
    /// A report to read and dismiss rather than a record of what happened.
    Panel {
        title: String,
        body: String,
    },
    Models(Vec<String>),
    ModelChanged(String),
    ReasoningEffortChanged(String),
    Prompts(Vec<Prompt>),
    Branches(Vec<Branch>),
    /// The conversation now stands somewhere else — another session, another
    /// branch, an earlier point of this one — and the screen follows it.
    Moved {
        /// The session file, when it is a different one.
        session: Option<String>,
        messages: Vec<Message>,
        /// A prompt taken back, to be edited and sent again.
        draft: Option<String>,
        note: String,
    },
    Status {
        context_chars: usize,
    },
    /// A command template finished expanding into the prompt to send.
    Expanded(String),
    Interaction {
        origin: Option<String>,
        request: elmee_core::InteractionRequest,
    },
    /// The project's files, for `@` completion, found off the event loop.
    Files(Vec<String>),
    /// A line the person ran themselves finished. It fills the cell that was
    /// already put up for it, and goes no further.
    Shell {
        ok: bool,
        output: String,
    },
    Runtime {
        name: String,
        agents: Vec<Participant>,
    },
}

pub struct Handle {
    pub cmd: mpsc::UnboundedSender<Cmd>,
    /// The same sender the worker reports through, so the frontend can run
    /// something of its own — a walk of the project's files — and have the
    /// answer arrive by the one road everything else arrives by.
    pub report: mpsc::UnboundedSender<Out>,
    pub abort: mpsc::UnboundedSender<()>,
    pub out: mpsc::UnboundedReceiver<Out>,
    pub interaction: mpsc::UnboundedSender<(String, InteractionResponse)>,
    /// Deliberately not a `Cmd`: see `host::Steerer`.
    pub steer: Steerer,
}

pub fn spawn(mut driver: Driver, mut runtime_rx: mpsc::UnboundedReceiver<RuntimeEvent>) -> Handle {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (abort_tx, mut abort_rx) = mpsc::unbounded_channel::<()>();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Out>();
    let out_tx_for_frontend = out_tx.clone();
    let (interaction_tx, mut interaction_rx) = mpsc::unbounded_channel();
    let responder = driver.runtime.responder();
    let steer = driver.runtime.steerer();
    match driver.runtime.participants() {
        Ok(agents) => {
            let _ = out_tx.send(Out::Runtime { name: driver.runtime.name().into(), agents });
        }
        Err(error) => {
            let _ = out_tx.send(Out::Info(format!("Mesh discovery failed: {error}")));
        }
    }

    tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                command = cmd_rx.recv() => {
                    let Some(command) = command else { break };
                    let command = match command {
                        Cmd::Plugin { name, arguments } => {
                            match driver.agent.plugin_command(&name, &arguments) {
                                Ok(outcome) => match outcome.prompt {
                                    Some(prompt) => Cmd::Prompt(prompt),
                                    None => {
                                        let _ = out_tx.send(Out::Info(outcome.message));
                                        continue;
                                    }
                                },
                                Err(error) => {
                                    let _ = out_tx.send(Out::Info(format!("cannot run /{name}: {error}")));
                                    continue;
                                }
                            }
                        }
                        other => other,
                    };
                    match command {
                        Cmd::Prompt(prompt) => {
                            run_turn(
                                &mut driver, &mut runtime_rx, &mut abort_rx,
                                &mut interaction_rx, &responder, &out_tx, Some(prompt),
                            ).await;
                        }
                        other => {
                            let _ = out_tx.send(command_once(other, &mut driver).await);
                        }
                    }
                    let _ = out_tx.send(Out::Status { context_chars: driver.agent.context_chars() });
                }
                event = runtime_rx.recv() => {
                    let Some(event) = event else { continue };
                    match event {
                        RuntimeEvent::Wake => {
                            if driver.has_work() {
                                run_turn(
                                    &mut driver, &mut runtime_rx, &mut abort_rx,
                                    &mut interaction_rx, &responder, &out_tx, None,
                                ).await;
                                let _ = out_tx.send(Out::Status { context_chars: driver.agent.context_chars() });
                            }
                        }
                        other => forward_runtime(other, &out_tx),
                    }
                }
                Some((id, response)) = interaction_rx.recv() => responder.respond(&id, response),
            }
        }
    });

    Handle {
        cmd: cmd_tx,
        report: out_tx_for_frontend,
        abort: abort_tx,
        out: out_rx,
        interaction: interaction_tx,
        steer,
    }
}

/// Race one turn against the things only an interactive frontend has: a person
/// pressing Esc, and answers to questions. What happens *inside* the
/// turn is the driver's, not this file's.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    driver: &mut Driver,
    runtime_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    abort_rx: &mut mpsc::UnboundedReceiver<()>,
    interaction_rx: &mut mpsc::UnboundedReceiver<(String, InteractionResponse)>,
    responder: &Responder,
    out: &mpsc::UnboundedSender<Out>,
    prompt: Option<String>,
) {
    while abort_rx.try_recv().is_ok() {}
    let _ = out.send(Out::Started);
    let events = out.clone();
    let mut on_event = move |event: AgentEvent| {
        let _ = events.send(Out::Event(event));
    };
    let run = driver.turn(prompt, &mut on_event);
    tokio::pin!(run);
    loop {
        tokio::select! {
            result = &mut run => {
                let _ = out.send(Out::Done {
                    error: result.err().map(|error| error.to_string()),
                    aborted: false,
                });
                break;
            }
            _ = abort_rx.recv() => {
                let _ = out.send(Out::Done { error: None, aborted: true });
                break;
            }
            Some(event) = runtime_rx.recv() => forward_runtime(event, out),
            Some((id, response)) = interaction_rx.recv() => responder.respond(&id, response),
        }
    }
}

fn forward_runtime(event: RuntimeEvent, out: &mpsc::UnboundedSender<Out>) {
    match event {
        RuntimeEvent::Snapshot { agents, name } => {
            let _ = out.send(Out::Runtime { name, agents });
        }
        RuntimeEvent::Interaction { origin, request } => {
            let _ = out.send(Out::Interaction { origin, request });
        }
        RuntimeEvent::Wake | RuntimeEvent::Child { .. } => {}
    }
}

async fn command_once(command: Cmd, driver: &mut Driver) -> Out {
    if let Cmd::SetModel { spec } = &command {
        driver.set_model(spec);
        return Out::ModelChanged(spec.clone());
    }
    if let Cmd::SetReasoningEffort(effort) = &command {
        driver.set_reasoning_effort(*effort);
        return Out::ReasoningEffortChanged(
            effort.map_or_else(|| "auto".to_owned(), |value| value.as_str().to_owned()),
        );
    }
    let command = match command {
        Cmd::Rewind { prompt, restore } => {
            return match driver.rewind(prompt, restore).await {
                Err(error) => Out::Info(format!("cannot go back: {error}")),
                Ok(None) => {
                    Out::Info("files restored to before that prompt — the conversation is unchanged".into())
                }
                Ok(draft) => Out::Moved {
                    session: None,
                    messages: driver.agent.messages().to_vec(),
                    draft,
                    note: match restore {
                        Restore::Everything => "went back, files included — edit the prompt and press Enter",
                        _ => "went back, files left as they are — edit the prompt and press Enter",
                    }
                    .into(),
                },
            };
        }
        Cmd::NewSession(path) => return replace_session(driver, path),
        Cmd::OpenSession(path) => return replace_session(driver, Some(path)),
        other => other,
    };
    let agent = &mut driver.agent;
    match command {
        Cmd::Prompt(_)
        | Cmd::Plugin { .. }
        | Cmd::SetModel { .. }
        | Cmd::SetReasoningEffort(_)
        | Cmd::Rewind { .. }
        | Cmd::NewSession(_)
        | Cmd::OpenSession(_) => unreachable!(),
        Cmd::Compact => {
            Out::Info(agent.compact().await.map_or_else(
                |error| format!("compaction failed: {error}"),
                |_| "context compacted".to_owned(),
            ))
        }
        Cmd::Checkout(id) => match agent.checkout(id) {
            Err(error) => Out::Info(format!("cannot switch branch: {error}")),
            Ok(()) => Out::Moved {
                session: None,
                messages: agent.messages().to_vec(),
                draft: None,
                note: "switched branch — files are as they were".into(),
            },
        },
        Cmd::RewindList => match agent.prompts() {
            prompts if prompts.is_empty() => Out::Info("nothing said yet to go back to".into()),
            prompts => Out::Prompts(prompts),
        },
        Cmd::Branches => match agent.branches() {
            branches if branches.len() < 2 => {
                Out::Info("this conversation has one branch — /rewind starts another".into())
            }
            branches => Out::Branches(branches),
        },
        Cmd::Mcp => Out::Panel { title: "mcp servers".into(), body: agent.extension_report() },
        Cmd::Clear => Out::Info(agent.clear().map_or_else(
            |error| format!("context clear could not be confirmed: {error}"),
            |()| "context cleared — the tree keeps the history".into(),
        )),
    }
}

fn replace_session(driver: &mut Driver, path: Option<PathBuf>) -> Out {
    if path.as_deref() == driver.agent.session_path() {
        return Out::Info("this session is already open".into());
    }
    match driver.open_session(path.clone()) {
        Ok(()) => {
            let session = path.map_or_else(|| "ephemeral".to_owned(), |path| path.display().to_string());
            Out::Moved {
                note: format!("session → {session}"),
                session: Some(session),
                messages: driver.agent.messages().to_vec(),
                draft: None,
            }
        }
        Err(error) => Out::Info(format!("cannot open session: {error}")),
    }
}
