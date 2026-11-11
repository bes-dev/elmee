//! The host process as a participant in the project mesh: the name it holds, the
//! tasks it started, the inbox it drains, and the questions it routes to
//! whoever is watching. The mesh itself — what the files mean, how a name is
//! claimed, how a message is delivered — is `elmee_plugin_mesh`, because none of
//! that depends on being a process. This is the half that does.

mod children;
mod mailbox;
mod supervisor;
mod tools;

pub use tools::host_tools;

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use elmee_core::{
    BackgroundEvent, InteractionRequest, InteractionResponse, Interactor, message::ToolCall, unique,
};

use elmee_plugin_mesh::{self as mesh, Envelope, Registration, peers_of as peers};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::events::{
    ChildEvent, Inbox, Options, Participant, Responder, RuntimeEvent, Steerer, TaskView, finished_runs,
    request_id,
};

pub(crate) struct Task {
    view: TaskView,
    stop: Option<oneshot::Sender<()>>,
    /// The supervisor watching the child. Held so a panic in it is a loss that
    /// gets named rather than a task marked "running" forever.
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
pub(crate) struct State {
    discovery_fault: Option<String>,
    /// Runtime teardown and Reporter publication share this mutex. A cloned
    /// Reporter must never act for an identity after its owner releases it.
    closed: bool,
    tasks: HashMap<String, Task>,
    /// Spawned tasks start unattached; only their ACP handshake supplies a
    /// mailbox lifetime. Task labels alone never establish that ownership.
    attachments: HashMap<String, Option<children::Attachment>>,
    /// Context for the first real turn, not work that can wake a model itself.
    initial_identity: Option<BackgroundEvent>,
    seen: HashSet<String>,
    /// Peers whose request this session has taken on and owes an answer.
    owed: Vec<Envelope>,
    /// Settled results waiting for durable publication, never another model turn.
    deliveries: Vec<PendingDelivery>,
    pending_messages: HashMap<String, Envelope>,
    /// mesh operations must not share ownership of a user-driven turn.
    user_turn: bool,
    status: String,
    /// Runs already reported, so a finished one wakes the agent once.
    reported: HashSet<String>,
    /// Current model spec; follows /model switches so child agents and the
    /// peer registration never advertise a stale model.
    model: String,
    /// Spawn count for this parent process only: what an earlier one started
    /// must not consume the new session's safety allowance.
    tasks_started: usize,
}

/// Writes what this agent is doing, and answers peers who asked for it.
#[derive(Clone)]
pub struct Reporter {
    base: PathBuf,
    name: String,
    state: Arc<Mutex<State>>,
    inbox: Inbox,
}

#[derive(Clone)]
struct PendingDelivery {
    request: Envelope,
    answer: String,
    status: mesh::CompletionStatus,
}

/// A dropped frontend future is cancellation, not a successful empty answer.
pub(crate) struct TurnReplies {
    reporter: Reporter,
    settled: bool,
}

impl TurnReplies {
    /// Whoever asked for this turn is told how it ended.
    pub(crate) fn finish(&mut self, answer: &str, ok: bool) {
        self.settle(
            answer,
            if ok { mesh::CompletionStatus::Completed } else { mesh::CompletionStatus::Failed },
        );
    }

    fn settle(&mut self, answer: &str, status: mesh::CompletionStatus) {
        self.reporter.finish(answer, status);
        self.settled = true;
    }
}

impl Drop for TurnReplies {
    fn drop(&mut self) {
        if !self.settled {
            self.reporter
                .finish("The agent turn was cancelled before completion.", mesh::CompletionStatus::Cancelled);
        }
        self.reporter.state.lock().unwrap().user_turn = false;
        self.reporter.set_status("idle");
        // Wake events received while busy may already have been consumed by
        // the frontend. Deferred mesh work needs a new edge after this turn.
        self.reporter.inbox.wake();
    }
}

impl Reporter {
    pub(crate) fn turn_replies(&self, user_turn: bool) -> TurnReplies {
        self.state.lock().unwrap().user_turn = user_turn;
        TurnReplies { reporter: self.clone(), settled: false }
    }
    /// Record the current state, so a peer asking sees the truth.
    ///
    /// Written when the state changes rather than on a timer: there is no
    /// clock to keep and nothing to expire, and rewriting the file also
    /// restores it if anything removed it.
    pub fn set_status(&self, status: &str) {
        self.publish_status(Some(status));
    }

    fn publish_status(&self, status: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        if let Some(status) = status {
            state.status = status.into();
        }
        let registration = Registration {
            name: self.name.clone(),
            pid: std::process::id(),
            model: state.model.clone(),
            status: state.status.clone(),
        };
        // Presence is true only while this process lives, and it is rewritten
        // at both ends of every turn: syncing it to disk would put a disk
        // flush in front of each model call to preserve what a crash voids.
        let _ = elmee_plugin_atomic::write(
            &self.base.join("agents").join(format!("{}.json", self.name)),
            &serde_json::to_vec(&registration).unwrap_or_default(),
            elmee_plugin_atomic::Options { durable: false, ..Default::default() },
        );
    }

    /// Answer every peer whose request produced this turn.
    ///
    /// The harness sends the reply because the model does not: handed an
    /// `<agent_message>`, it tends to imitate the envelope in its answer text
    /// instead of calling a tool, and the agent that asked waits forever for a
    /// report that was only ever printed to a terminal.
    pub fn reply(&self, answer: &str) {
        self.finish(answer, mesh::CompletionStatus::Completed);
    }

    fn finish(&self, answer: &str, status: mesh::CompletionStatus) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        let owed = std::mem::take(&mut state.owed);
        state.deliveries.extend(owed.into_iter().map(|request| PendingDelivery {
            request,
            answer: answer.into(),
            status,
        }));
        drop(state);
        self.retry_replies();
    }

    fn retry_replies(&self) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        // Keep teardown excluded until durable completion and acknowledgement
        // finish. Checking closed and releasing the mutex would leave a race.
        let deliveries = state.deliveries.clone();
        for delivery in deliveries {
            let request = &delivery.request;
            if let Err(error) =
                mesh::complete(&self.base, &self.name, request, delivery.answer, delivery.status)
            {
                eprintln!("elmee: mesh completion {} retained for retry: {error}", request.id);
                continue;
            }
            let path = self.base.join("inbox").join(&self.name).join(format!("{}.json", request.id));
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("elmee: cannot acknowledge {}: {error}", request.id);
                continue;
            }
            state.deliveries.retain(|entry| entry.request.id != request.id);
            state.seen.remove(&request.id);
        }
    }
}

pub struct Runtime {
    pub(super) state: Arc<Mutex<State>>,
    pub(super) inbox: Inbox,
    pub(super) pending: Responder,
    pub(super) events: mpsc::UnboundedSender<RuntimeEvent>,
    pub(super) base: PathBuf,
    pub(super) name: String,
    /// Captured once with the owned name, never refreshed across attachments.
    incarnation: String,
    pub(super) cwd: PathBuf,
    pub(super) model_aliases: elmee_plugin_provider::registry::ModelAliases,
    pub(super) require_verification: elmee_core::evidence::VerificationLevel,
    /// The boundary this session was composed with, passed to every child it
    /// starts. A child is a fresh composition, so it is told what its box is
    pub(super) depth: usize,
    /// Aggregate cost ceiling and usage ledger inherited by every child.
    pub(super) max_tokens: u64,
    pub(super) token_ledger: Option<PathBuf>,
    /// Wakes `wait_agents` on every task transition. The task map remains the
    /// source of truth, so missed/coalesced notifications are harmless.
    pub(super) task_notify: Arc<Notify>,
    pub(super) shutdown: Arc<AtomicBool>,
    /// The inbox poller. Held for the same reason as each task's supervisor:
    /// if it panics, the inbox silently never fills, so its death is checked
    /// for and said out loud instead.
    pub(super) mailbox: Option<tokio::task::JoinHandle<()>>,
    /// Held open for the life of the process: this is what makes the name ours.
    _name_lock: std::fs::File,
    /// Tasks running a program the project declared rather than elmee.
    pub(super) foreign: HashSet<String>,
}

impl Runtime {
    pub fn new(
        config: &Options,
        model_aliases: elmee_plugin_provider::registry::ModelAliases,
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<RuntimeEvent>)> {
        let base = elmee_plugin_paths::project_state_directory(&config.cwd);
        Self::new_at_with(config, base, model_aliases)
    }

    #[cfg(test)]
    fn new_at(
        config: &Options,
        base: PathBuf,
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<RuntimeEvent>)> {
        Self::new_at_with(config, base, Default::default())
    }

    fn new_at_with(
        config: &Options,
        base: PathBuf,
        model_aliases: elmee_plugin_provider::registry::ModelAliases,
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<RuntimeEvent>)> {
        fs::create_dir_all(base.join("agents"))?;
        fs::create_dir_all(base.join("inbox"))?;
        fs::create_dir_all(base.join("tasks"))?;
        let (name, name_lock) = mesh::claim_available_name(&base, config.name.as_deref().unwrap_or("agent"))
            .map_err(|error| {
                std::io::Error::new(error.kind(), format!("could not claim mesh name: {error}"))
            })?;
        let incarnation = mesh::incarnation(&base, &name)?;
        // Recovery mutates durable messages. Never enter it without exclusive
        // ownership, including when creating/opening the claim file failed.
        mesh::prepare_startup(&base, &name)?;
        fs::create_dir_all(base.join("inbox").join(&name))?;
        let (events, rx) = mpsc::unbounded_channel();
        let identity = BackgroundEvent {
            id: unique("identity"),
            prompt: format!(
                "<agent_identity name=\"@{name}\">Use this project-visible name as your role and address when coordinating with peers.</agent_identity>"
            ),
        };
        let mut runtime = Self {
            state: Arc::new(Mutex::new(State {
                initial_identity: Some(identity),
                model: config.model.clone(),
                ..State::default()
            })),
            pending: Responder::default(),
            inbox: Inbox::new(events.clone()),
            events,
            base,
            incarnation,
            name,
            cwd: config.cwd.clone(),
            model_aliases,
            require_verification: config.require_verification,
            depth: config.depth,
            max_tokens: config.max_tokens,
            token_ledger: config.token_ledger.clone(),
            task_notify: Arc::new(Notify::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            mailbox: None,
            _name_lock: name_lock,
            foreign: HashSet::new(),
        };
        runtime.write_registration("idle");
        runtime.emit_snapshot();
        runtime.start_mailbox();
        Ok((runtime, rx))
    }

    pub fn responder(&self) -> Responder {
        self.pending.clone()
    }

    pub fn steerer(&self) -> Steerer {
        Steerer(self.inbox.clone())
    }

    pub fn working_directory(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The mailbox this session holds, for a parent that started it to attach
    /// to. Cooperative routing metadata, not authentication of a same-user peer.
    pub fn mesh_identity(&self) -> serde_json::Value {
        serde_json::json!({ "root": self.base, "name": self.name, "incarnation": self.incarnation })
    }

    /// A handle to the reporting side of the runtime, usable while a turn
    /// holds the runtime itself: neither writing the registration nor
    /// answering a peer needs exclusive access.
    pub fn reporter(&self) -> Reporter {
        Reporter {
            base: self.base.clone(),
            name: self.name.clone(),
            state: self.state.clone(),
            inbox: self.inbox.clone(),
        }
    }

    /// Start a turn: the project sees this agent busy, and whoever asked for
    /// the work is owed its outcome from here until the guard is settled.
    pub(crate) fn begin_turn(&self, user_turn: bool) -> TurnReplies {
        let reporter = self.reporter();
        let replies = reporter.turn_replies(user_turn);
        reporter.set_status("busy");
        replies
    }

    /// Follow a /model switch so children and the registration stay current.
    pub fn set_model(&self, model: String) {
        self.state.lock().unwrap().model = model;
    }

    pub fn model(&self) -> String {
        self.state.lock().unwrap().model.clone()
    }

    pub fn participants(&self) -> std::io::Result<Vec<Participant>> {
        participants(&self.state, &self.base, &self.name)
    }

    pub fn summary(&self) -> String {
        match self.participants() {
            Ok(agents) => serde_json::json!({"self": self.name, "agents": agents}),
            Err(error) => serde_json::json!({"self": self.name, "error": error.to_string()}),
        }
        .to_string()
    }

    pub fn has_background_events(&self) -> bool {
        !self.inbox.is_empty()
    }

    fn write_registration(&self, status: &str) {
        self.reporter().set_status(status);
    }

    fn emit_snapshot(&self) {
        emit_snapshot(&self.state, &self.events, &self.base, &self.name);
    }
}

impl Interactor for Runtime {
    async fn request(&mut self, request: InteractionRequest) -> InteractionResponse {
        let rx = self.pending.wait(request_id(&request).to_owned());
        let _ = self.events.send(RuntimeEvent::Interaction { origin: None, request });
        rx.await.unwrap_or(InteractionResponse::Cancel)
    }

    async fn refute(&mut self, objective: &str, evidence: &str) -> Option<String> {
        let model = self.state.lock().unwrap().model.clone();
        // A checker that spawned its own checkers would recurse; depth already
        // stops that, and a child has no business auditing its parent's goal.
        if self.depth > 0 {
            return None;
        }
        crate::refute::refute(
            &self.cwd,
            &model,
            objective,
            evidence,
            self.max_tokens,
            self.token_ledger.as_deref(),
        )
        .await
    }

    async fn host_tool(&mut self, call: &ToolCall) -> Option<String> {
        match call.function.name.as_str() {
            "spawn_agent" => Some(self.spawn_agent(call).await),
            "send_to_agent" => Some(self.send_to_agent(call)),
            "stop_agent" => Some(self.stop_agent(call)),
            "wait_agents" => Some(self.wait_agents(call).await),
            _ => None,
        }
    }

    fn handles_host_tool(&self, call: &ToolCall) -> bool {
        matches!(call.function.name.as_str(), "spawn_agent" | "send_to_agent" | "stop_agent" | "wait_agents")
    }

    fn background_events(&mut self) -> Vec<BackgroundEvent> {
        self.surface_dead_mailbox();
        self.surface_dead_supervisors();
        let mut state = self.state.lock().unwrap();
        let events = self.inbox.drain();
        let mut dispatched = Vec::new();
        for event in events {
            if let Some(request) = state.pending_messages.get(&event.id).cloned() {
                // One mesh envelope owns one background turn. Keep later mesh
                // work queued even when the agent polls between tool steps;
                // otherwise unrelated senders receive the same mixed answer.
                if state.user_turn || !state.owed.is_empty() {
                    self.inbox.defer(event);
                    continue;
                }
                match mesh::begin_execution(&self.base, &self.name, &request) {
                    Ok(true) => {
                        state.owed.push(request);
                    }
                    Ok(false) => {
                        state.pending_messages.remove(&event.id);
                        state.seen.remove(&event.id);
                        continue;
                    }
                    Err(error) => {
                        eprintln!("elmee: cannot dispatch mesh {}: {error}", event.id);
                        self.inbox.defer(event);
                        continue;
                    }
                }
                state.pending_messages.remove(&event.id);
            }
            dispatched.push(event);
        }
        if (!dispatched.is_empty() || state.user_turn)
            && let Some(identity) = state.initial_identity.take()
        {
            dispatched.insert(0, identity);
        }
        dispatched
    }

    fn ack_background(&mut self, ids: &[String]) {
        let mut state = self.state.lock().unwrap();
        for id in ids {
            if state.owed.iter().any(|request| &request.id == id)
                || state.deliveries.iter().any(|delivery| &delivery.request.id == id)
            {
                continue;
            }
            let _ = fs::remove_file(self.base.join("inbox").join(&self.name).join(format!("{id}.json")));
            state.seen.remove(id);
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        // Aborted rather than left to notice the flag: the poller holds no
        // resource that needs a graceful end, and exit should not wait a tick.
        if let Some(mailbox) = self.mailbox.take() {
            mailbox.abort();
        }
        let _ = fs::remove_file(self.base.join("agents").join(format!("{}.json", self.name)));
        for task in state.tasks.values_mut() {
            if let Some(stop) = task.stop.take() {
                let _ = stop.send(());
            }
        }
        // Closing our descriptor alone can leave the lifetime lock held by a
        // duplicate inherited during a concurrent fork, until that child execs.
        // Teardown has fenced Reporter publication and removed presence, so
        // explicitly end ownership now, while the shared state is still closed.
        // An unlock failure keeps the conservative close-on-drop behavior.
        let _ = self._name_lock.unlock();
    }
}

/// Everyone in the project, from the two places that know: the mesh, which
/// sees anyone holding a name, and this session, which knows what it started.
///
/// They overlap by design — a running child holds its own name and writes its
/// own registration, so it is a peer *and* a task — and that overlap is why
/// there is one list. A name this session owns is described by its task, which
/// says more than a registration can; a name it does not own is described by
/// what that agent published about itself. Live entries come first, then what
/// has finished, each in the order it started.
pub(super) fn participants(
    state: &Arc<Mutex<State>>,
    base: &Path,
    me: &str,
) -> std::io::Result<Vec<Participant>> {
    let mut agents: Vec<Participant> = peers(base, me)?
        .into_iter()
        .map(|peer| Participant { name: peer.name, status: peer.status, model: peer.model, task: None })
        .collect();
    let (mut tasks, attachments) = {
        let state = state.lock().unwrap();
        (state.tasks.values().map(|task| task.view.clone()).collect::<Vec<_>>(), state.attachments.clone())
    };
    tasks.sort_by_key(|task| task.started_at);
    for task in tasks {
        let attachment = attachments.get(&task.id);
        let mine = Participant {
            name: task.name.clone(),
            status: task.status.clone(),
            model: String::new(),
            task: Some(task),
        };
        match agents.iter().position(|agent| match attachment {
            Some(attachment) => attachment.as_ref().is_some_and(|binding| {
                agent.name == binding.name
                    && mesh::incarnation(base, &binding.name)
                        .is_ok_and(|incarnation| incarnation == binding.incarnation)
            }),
            None => agent.name == mine.name,
        }) {
            // The registration it wrote for itself, described by the task it
            // was given: the model it advertises, the work it was sent to do.
            Some(at) => {
                agents[at] =
                    Participant { name: agents[at].name.clone(), model: agents[at].model.clone(), ..mine }
            }
            None => agents.push(mine),
        }
    }
    agents.sort_by_key(|agent| {
        let finished = agent.task.as_ref().and_then(|task| task.finished_at);
        (finished.is_some(), agent.task.as_ref().map_or(0, |task| task.started_at))
    });
    Ok(agents)
}

pub(super) fn emit_snapshot(
    state: &Arc<Mutex<State>>,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    base: &Path,
    name: &str,
) {
    match participants(state, base, name) {
        Ok(agents) => {
            state.lock().unwrap().discovery_fault = None;
            let _ = events.send(RuntimeEvent::Snapshot { agents, name: name.into() });
        }
        Err(error) => {
            let error = error.to_string();
            let mut state = state.lock().unwrap();
            if state.discovery_fault.as_ref() != Some(&error) {
                eprintln!("elmee: mesh discovery failed; snapshot not refreshed: {error}");
                state.discovery_fault = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests;
