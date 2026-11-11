//! Everything that happens around a turn, shared by every frontend.
//!
//! A frontend decides how a prompt arrives and where the output is shown. It
//! does not decide whether a snapshot is taken, whether a peer that asked gets
//! an answer, whether the project is told this agent is busy, or whether a
//! finished background run wakes anyone. Those are the agent's behaviour, and
//! behaviour that differs because someone chose a different way to look at the
//! same agent is a bug — which is precisely what happened: the terminal
//! interface had all four, the duplex protocol had two, and a one-shot run had
//! none, so `/rewind` quietly did nothing outside the TUI and a long command
//! started by a headless agent had nobody left to notice it finish.
//!
//! What legitimately differs between frontends is *lifetime* — one turn, or
//! until the caller closes the pipe, or until a person quits. That belongs to
//! the frontend's loop. Everything inside a turn belongs here.

use std::rc::Rc;

use elmee_core::{Agent, AgentEvent, Result};
use elmee_plugin_snapshot::Snapshots;

use crate::runtime::Runtime;

pub struct Driver {
    pub agent: Agent,
    pub runtime: Runtime,
    /// What records the working tree before a prompt may change it, when
    /// the composition has one.
    pub(crate) snapshots: Option<Rc<Snapshots>>,
}

impl Driver {
    pub fn new(agent: Agent, runtime: Runtime) -> Self {
        let snapshots = agent.services().one::<Snapshots>().ok().flatten();
        Self { agent, runtime, snapshots }
    }

    /// Run on another model from now on: the agent, the compaction threshold
    /// sized for its window, what the project sees this agent advertise, and
    /// this machine's choice for the next run all follow together.
    pub fn set_model(&mut self, spec: &str) {
        self.agent.set_model(spec.to_owned());
        if let Some(window) = self.agent.context_window() {
            self.agent.set_compact_chars(elmee_core::compact_chars_for(window));
        }
        self.runtime.set_model(spec.to_owned());
        crate::remember_model(spec);
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<elmee_core::provider::ReasoningEffort>) {
        self.agent.set_reasoning_effort(effort);
    }

    pub fn reasoning_effort(&self) -> Option<elmee_core::provider::ReasoningEffort> {
        self.agent.reasoning_effort()
    }

    /// Is there anything waiting that would make a prompt-less turn worthwhile?
    pub fn has_work(&self) -> bool {
        self.runtime.has_background_events()
    }

    /// Take one turn. `None` means the turn is driven by what arrived in the
    /// inbox rather than by something a person typed.
    pub async fn turn(
        &mut self,
        prompt: Option<String>,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> Result<String> {
        let prompt = match prompt {
            Some(prompt) => Some(self.expanded(prompt).await),
            None => None,
        };
        let mut turn = self.runtime.begin_turn(prompt.is_some());
        // Recording the tree overlaps the model call: nothing may write
        // until it lands, and the model call does not write.
        if let (Some(snapshots), Some(prompt)) = (&self.snapshots, &prompt) {
            self.agent.defer_barrier(snapshots.begin(prompt));
        }
        let outcome = match prompt {
            Some(prompt) => self.agent.run_interactive(prompt, on_event, &mut self.runtime).await,
            None => self.agent.run_background(on_event, &mut self.runtime).await,
        };
        // Freeze the outcome before any asynchronous observers. Delivery
        // retries must never substitute the answer from a later user turn.
        match &outcome {
            Ok(answer) if self.agent.stopped_by_guard() => {
                turn.finish(&format!("Agent turn stopped by its step/resource guard.\n{answer}"), false);
            }
            Ok(answer) => turn.finish(answer, true),
            Err(error) => turn.finish(&error.to_string(), false),
        }
        // A turn that never called a tool leaves its snapshot unsettled, and
        // the record must stay bounded by the turn that started it.
        self.agent.settle_barriers().await;
        outcome
    }
}
