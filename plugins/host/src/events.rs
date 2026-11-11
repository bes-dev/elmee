//! What every runtime has, whether or not it runs other agents: the events a
//! frontend is sent, the queue of things that reach an agent between requests,
//! the way a question finds its answer, and the shapes a frontend draws.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use elmee_core::{BackgroundEvent, InteractionRequest, InteractionResponse, unique};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// What one session's runtime is told about the run it belongs to.
#[derive(Clone)]
pub struct Options {
    pub cwd: PathBuf,
    /// The name asked for in the project; the one granted may carry a suffix.
    pub name: Option<String>,
    pub model: String,
    /// Non-zero for a spawned agent, which may not spawn its own checkers.
    pub depth: usize,
    pub require_verification: elmee_core::evidence::VerificationLevel,
    /// Aggregate cost ceiling and usage ledger inherited by every child.
    pub max_tokens: u64,
    pub token_ledger: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub detail: String,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// What this agent runs on. A delegated agent may be on another model
    /// entirely, and a report from one of three reviewers is unreadable
    /// without knowing which.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Anyone answering to a name in this project — one list, whether this session
/// started them or a person did.
///
/// The two were separate lists until they were not worth being: a running child
/// holds its own name and writes its own registration, so it appeared *twice*,
/// once as a task and once as a peer, and a caller had to remember which list a
/// name came from to know what it could do with it. What actually differs is
/// ownership, so ownership is the field: `task` is present exactly when this
/// session started the agent and is answerable for it, and everything that only
/// an owner may do — waiting on it, stopping it, spending its tokens — is
/// gated on that rather than on a separate namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskView>,
}

impl Participant {
    /// Started by this session, and still able to act on what it is told.
    pub fn is_live_task(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !terminal(&task.status))
    }
}

pub enum RuntimeEvent {
    Snapshot {
        agents: Vec<Participant>,
        name: String,
    },
    Interaction {
        origin: Option<String>,
        request: InteractionRequest,
    },
    Wake,
    /// An agent this session started, as the protocol it speaks reports it.
    Child {
        session: String,
        name: String,
        event: ChildEvent,
    },
}

/// What a child's own session did: opened, streamed one protocol update, or
/// stopped working.
pub enum ChildEvent {
    Opened,
    Update(serde_json::Value),
    Ended,
}

/// Whether a task status read back from a record is an ending. An
/// unrecognized word is something still in flight.
pub(crate) fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "stopped")
}

#[derive(Clone, Default)]
pub struct Responder(Arc<Mutex<HashMap<String, oneshot::Sender<InteractionResponse>>>>);

impl Responder {
    pub fn respond(&self, id: &str, response: InteractionResponse) {
        if let Some(waiter) = self.0.lock().unwrap().remove(id) {
            let _ = waiter.send(response);
        }
    }

    /// Register a question and hand back where its answer will arrive.
    pub(crate) fn wait(&self, id: String) -> oneshot::Receiver<InteractionResponse> {
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().insert(id, tx);
        rx
    }
}

/// What reaches an agent between requests rather than interrupting one: a
/// person steering, a long command that ended, a peer's message.
#[derive(Clone)]
pub(crate) struct Inbox {
    queue: Arc<Mutex<VecDeque<BackgroundEvent>>>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
}

impl Inbox {
    pub(crate) fn new(events: mpsc::UnboundedSender<RuntimeEvent>) -> Self {
        Self { queue: Arc::default(), events }
    }

    /// Queue an event and say so: the wake is what an idle agent needs and a
    /// busy one ignores.
    pub(crate) fn push(&self, event: BackgroundEvent) {
        self.defer(event);
        let _ = self.events.send(RuntimeEvent::Wake);
    }

    /// Queue an event that is not yet anyone's turn to start.
    pub(crate) fn defer(&self, event: BackgroundEvent) {
        self.queue.lock().unwrap().push_back(event);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }

    pub(crate) fn drain(&self) -> Vec<BackgroundEvent> {
        self.queue.lock().unwrap().drain(..).collect()
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<BackgroundEvent> {
        self.queue.lock().unwrap().iter().cloned().collect()
    }

    /// Start whatever is waiting, if anything is.
    pub(crate) fn wake(&self) {
        if !self.is_empty() {
            let _ = self.events.send(RuntimeEvent::Wake);
        }
    }
}

/// The person at the keyboard, reaching an agent that is already working.
///
/// A handle rather than a command, and that is the whole design. The worker
/// awaits the turn *inside* a `select!` arm, so while one is running nothing
/// reads its command channel — a steering command sent there would sit unread
/// until the turn ended, which is the queue it was meant to replace. Writing
/// into the shared inbox instead is the path a peer's message already takes
/// from the mailbox task, and the agent drains it at the top of every step.
#[derive(Clone)]
pub struct Steerer(pub(crate) Inbox);

impl Steerer {
    pub fn steer(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let id = unique("steer");
        let prompt = elmee_core::text::fenced("user_message", &format!(" id=\"{id}\""), text);
        self.0.push(BackgroundEvent { id, prompt });
    }
}

pub(crate) fn request_id(request: &InteractionRequest) -> &str {
    match request {
        InteractionRequest::Questions { id, .. } => id,
    }
}

/// Long commands that ended since the last look, each as the event the agent
/// hears: it is told between requests instead of polling, and the report
/// carries the output the command left behind.
pub(crate) fn finished_runs(cwd: &Path, reported: &mut HashSet<String>) -> Vec<BackgroundEvent> {
    let mut events = Vec::new();
    for run in elmee_plugin_tools::runs::list(cwd) {
        let Some(code) = elmee_plugin_tools::runs::outcome(cwd, &run.id) else { continue };
        if !reported.insert(run.id.clone()) {
            continue;
        }
        let output = elmee_plugin_tools::runs::output(cwd, &run.id);
        elmee_plugin_tools::runs::forget(cwd, &run.id);
        let verdict = if code == 0 { "finished" } else { "failed" };
        let body = format!(
            "{}\n{}",
            run.command,
            elmee_core::text::fenced(
                "output",
                " trusted=\"no\"",
                &elmee_plugin_tools::tools::truncate(output, false),
            )
        );
        events.push(BackgroundEvent {
            id: format!("run-{}", run.id),
            prompt: elmee_core::text::fenced(
                "run",
                &format!(" id=\"{}\" status=\"{verdict}\" exit=\"{code}\"", run.id),
                &body,
            ),
        });
    }
    events
}
