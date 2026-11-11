//! The runtime of an agent that runs no others: built without `agents`, a
//! session is one agent, a person who may steer it, and the long commands it
//! started. Nothing here claims a name in a project or reads a mailbox.
//!
//! It answers to the same calls as the full runtime, so a frontend and the
//! driver are written once; what a lone agent has no answer for comes back
//! empty rather than absent.

use std::{collections::HashSet, path::Path};

use elmee_core::{BackgroundEvent, InteractionRequest, InteractionResponse, Interactor};
use tokio::sync::mpsc;

use crate::events::{
    Inbox, Options, Participant, Responder, RuntimeEvent, Steerer, finished_runs, request_id,
};

pub struct Runtime {
    inbox: Inbox,
    pending: Responder,
    events: mpsc::UnboundedSender<RuntimeEvent>,
    options: Options,
    runs: tokio::task::JoinHandle<()>,
}

/// A turn nobody else is waiting on: all it owes is a wake for what arrived
/// while it ran.
pub(crate) struct Turn(Inbox);

impl Turn {
    pub(crate) fn finish(&mut self, _answer: &str, _ok: bool) {}
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.0.wake();
    }
}

/// A lone agent is offered no tools for running others.
pub fn host_tools(_roster: Option<&str>) -> Vec<serde_json::Value> {
    Vec::new()
}

impl Runtime {
    pub fn new(
        options: &Options,
        _model_aliases: elmee_plugin_provider::registry::ModelAliases,
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<RuntimeEvent>)> {
        let (events, rx) = mpsc::unbounded_channel();
        let inbox = Inbox::new(events.clone());
        let (cwd, finished) = (options.cwd.clone(), inbox.clone());
        let runs = tokio::task::spawn_local(async move {
            let mut reported = HashSet::new();
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                tick.tick().await;
                for event in finished_runs(&cwd, &mut reported) {
                    finished.push(event);
                }
            }
        });
        Ok((Self { inbox, pending: Responder::default(), events, options: options.clone(), runs }, rx))
    }

    pub fn responder(&self) -> Responder {
        self.pending.clone()
    }

    pub fn steerer(&self) -> Steerer {
        Steerer(self.inbox.clone())
    }

    pub fn working_directory(&self) -> &Path {
        &self.options.cwd
    }

    pub fn name(&self) -> &str {
        self.options.name.as_deref().unwrap_or("agent")
    }

    pub fn mesh_identity(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    pub fn set_model(&mut self, model: String) {
        self.options.model = model;
    }

    pub fn model(&self) -> String {
        self.options.model.clone()
    }

    pub fn participants(&self) -> std::io::Result<Vec<Participant>> {
        Ok(Vec::new())
    }

    pub fn has_background_events(&self) -> bool {
        !self.inbox.is_empty()
    }

    pub(crate) fn begin_turn(&self, _user_turn: bool) -> Turn {
        Turn(self.inbox.clone())
    }
}

impl Interactor for Runtime {
    async fn request(&mut self, request: InteractionRequest) -> InteractionResponse {
        let rx = self.pending.wait(request_id(&request).to_owned());
        let _ = self.events.send(RuntimeEvent::Interaction { origin: None, request });
        rx.await.unwrap_or(InteractionResponse::Cancel)
    }

    async fn refute(&mut self, objective: &str, evidence: &str) -> Option<String> {
        if self.options.depth > 0 {
            return None;
        }
        let Options { cwd, model, max_tokens, token_ledger, .. } = &self.options;
        crate::refute::refute(cwd, model, objective, evidence, *max_tokens, token_ledger.as_deref()).await
    }

    fn background_events(&mut self) -> Vec<BackgroundEvent> {
        self.inbox.drain()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.runs.abort();
    }
}
