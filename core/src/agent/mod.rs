use serde::Serialize;

use crate::{
    Error, Result,
    evidence::{EvidenceManifest, VerificationLevel},
    interaction::{Interactor, RejectInteractions},
    message::{Message, Role},
    plugin::{MiddlewareEffect, MiddlewareEvent, Pending, Registry},
    session::Session,
};

/// One observable step of an agent turn. Frontends render these; headless
/// mode serializes them as JSONL, so the serde shape is a public contract.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The whole plan, every time it changes, including when it empties. The
    /// agent owns it; a frontend draws what it is sent and keeps no opinion of
    /// its own, because two copies and no way to clear either is how a
    /// half-finished plan came to hang over three unrelated tasks.
    Plan {
        steps: Vec<crate::plan::Step>,
    },
    Text {
        text: String,
    },
    /// The model working. Not the answer, and the only thing to look at during
    /// the half-minute a thinking model spends before producing one.
    Reasoning {
        text: String,
    },
    ModelStart {
        context_chars: usize,
    },
    ModelEnd {
        ok: bool,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        metrics: Option<crate::provider::RequestMetrics>,
    },
    ToolStart {
        id: String,
        name: String,
        arguments: String,
    },
    ToolEnd {
        id: String,
        name: String,
        ok: bool,
        duration_ms: u128,
        output: String,
    },
    /// The older part of the conversation is being summarized: the summary,
    /// as it is written.
    Compacting {
        text: String,
    },
    /// The context was thrown away: how much was taken out of it first, and
    /// what stands in its place.
    Compacted {
        notes: usize,
        summary: String,
    },
}

mod answers;
mod budget;
mod compaction;

use answers::emit_tool;
pub use budget::TokenLedger;

pub struct Agent {
    plugins: Registry,
    session: Session,
    max_steps: usize,
    compact_chars: usize,
    system_prompt: String,
    /// Accumulated (input, output) tokens across this process's requests.
    tokens: (u64, u64),
    /// The last completion-gate failure, so an unchanged one ends the turn
    /// instead of looping forever on a check the agent cannot satisfy.
    last_gate: Option<String>,
    /// Input tokens the provider reported for the last request, and the
    /// context size in characters at that moment. Together they calibrate the
    /// characters-per-token estimate against what the model actually charged,
    /// instead of assuming four.
    measured: Option<(usize, usize)>,
    /// The size of the tool schemas sent with every request.
    schema_chars: usize,
    /// Set when a compaction failed to free room, so the next turn does not
    /// spend another full request on a summary that cannot help.
    compaction_is_futile: bool,
    /// What the agent is working through. The single copy.
    plan: Vec<crate::plan::Step>,
    /// Total tokens this agent may spend before it stops, or 0 for no limit.
    /// Checked before every request, because the only reliable moment to not
    /// spend money is before spending it.
    token_budget: u64,
    /// Append-only aggregate usage for the whole process tree. Each charge is
    /// one short write, so concurrent children cannot overwrite one another.
    token_ledger: Option<std::rc::Rc<dyn budget::TokenLedger>>,
    evidence: EvidenceManifest,
    required_verification: VerificationLevel,
    /// Asynchronous application barriers started before the model answers and
    /// settled before tools can cause external effects.
    pending_barriers: Vec<Pending>,
    workspace: std::rc::Rc<dyn crate::Workspace>,
    /// Whether the last turn was ended by the step guard rather than by the
    /// agent finishing. A frontend reports the two differently.
    guarded: bool,
}

/// Whether a turn that produced no tool calls is the end of the work.
enum Ending {
    Done(String),
    KeepGoing,
}

impl Agent {
    pub fn new(
        plugins: Registry,
        max_steps: usize,
        compact_chars: usize,
        system_prompt: String,
    ) -> Result<Self> {
        Self::new_with_interaction(plugins, max_steps, compact_chars, system_prompt, true)
    }

    pub(crate) fn new_with_interaction(
        mut plugins: Registry,
        max_steps: usize,
        compact_chars: usize,
        system_prompt: String,
        interactive: bool,
    ) -> Result<Self> {
        plugins.add_kernel_tools(answers::definitions(interactive))?;
        plugins.resolve_agent()?;
        let session = plugins.take_session();
        let token_ledger = plugins.token_ledger()?;
        let workspace = plugins.workspace()?;
        let evidence = EvidenceManifest::default();
        let schema_chars = plugins.definitions().iter().map(|schema| schema.to_string().len()).sum();
        Ok(Self {
            plugins,
            session,
            max_steps,
            compact_chars,
            system_prompt,
            tokens: (0, 0),
            last_gate: None,
            measured: None,
            schema_chars,
            compaction_is_futile: false,
            plan: Vec::new(),
            token_budget: 0,
            token_ledger,
            evidence,
            required_verification: VerificationLevel::Unverified,
            pending_barriers: Vec::new(),
            workspace,
            guarded: false,
        })
    }

    /// Shared typed capabilities exposed by installed plugins. Application
    /// workers use these without adding product-specific fields to `Agent`.
    pub fn services(&self) -> crate::Services {
        self.plugins.services().expect("an agent always owns a resolved composition")
    }

    /// Stable explanation of the plugins, capabilities and global modifiers
    /// that make this agent different from another composition.
    pub fn composition(&self) -> String {
        self.plugins.manifest()
    }

    /// Hand over application work that must settle before the first tool.
    pub fn defer_barrier(&mut self, pending: Pending) {
        self.pending_barriers.push(pending);
    }

    /// Settle application barriers. A checkpoint one of them returns is the
    /// working tree as it was before this turn wrote to it, and is kept with
    /// the prompt that started the turn — which is what lets a later rewind
    /// put the files back as well as the conversation.
    pub async fn settle_barriers(&mut self) {
        for pending in std::mem::take(&mut self.pending_barriers) {
            if let Some(checkpoint) = pending.settle().await {
                // Losing the link costs a files-too rewind, never the turn.
                let _ = self.session.checkpoint(checkpoint);
            }
        }
    }

    /// Route a domain-specific command to whichever handler published it.
    pub fn plugin_commands(&self) -> Vec<crate::CommandSpec> {
        self.plugins.commands()
    }

    pub fn plugin_command(&mut self, name: &str, argument: &str) -> Result<crate::CommandOutcome> {
        self.plugins
            .command(name, argument)
            .unwrap_or_else(|| Err(Error::Config(format!("no plugin handles command {name}"))))
    }

    pub fn evidence(&self) -> &EvidenceManifest {
        &self.evidence
    }

    pub fn require_verification(&mut self, level: VerificationLevel) {
        self.required_verification = level;
    }

    pub fn required_verification(&self) -> VerificationLevel {
        self.required_verification
    }

    /// How many characters of context are kept before it is summarized.
    pub fn compact_chars(&self) -> usize {
        self.compact_chars
    }

    async fn tree_hash(&self) -> Option<String> {
        self.workspace.fingerprint().await
    }

    async fn refresh_evidence(&mut self) {
        // Hashing the tree is two `git` runs, and all it can do is mark
        // recorded evidence stale. With nothing recorded there is nothing to
        // mark — the usual case, since only a recognised test command becomes
        // evidence at all — so the runs would buy nothing.
        if self.evidence.is_empty() {
            return;
        }
        let tree = self.tree_hash().await;
        self.evidence.refresh(tree);
    }

    pub async fn run<F>(&mut self, prompt: String, on_event: &mut F) -> Result<String>
    where
        F: FnMut(AgentEvent) + Send,
    {
        self.run_interactive(prompt, on_event, &mut RejectInteractions).await
    }

    pub async fn run_interactive<F, I>(
        &mut self,
        prompt: String,
        on_event: &mut F,
        interactor: &mut I,
    ) -> Result<String>
    where
        F: FnMut(AgentEvent) + Send,
        I: Interactor,
    {
        let mut message = Message::user(prompt.clone());
        // A person who wants the agent to look at a screenshot says its name.
        // Requiring a flag for that would be a flag nobody remembers, and the
        // magic here is bounded: an existing file, in this workspace, whose
        // extension says it is a picture.
        message.images = self.workspace.attachments(&prompt);
        for effect in self.plugins.dispatch(MiddlewareEvent::UserPrompt { prompt: &prompt }).await? {
            match effect {
                MiddlewareEffect::AddContext(context) if !context.is_empty() => {
                    message.content = Some(format!("{}\n\n{context}", message.content.unwrap_or_default()));
                }
                _ => {}
            }
        }
        self.run_message(Some(message), on_event, interactor).await
    }

    pub async fn run_background<F, I>(&mut self, on_event: &mut F, interactor: &mut I) -> Result<String>
    where
        F: FnMut(AgentEvent) + Send,
        I: Interactor,
    {
        self.run_message(None, on_event, interactor).await
    }

    async fn run_message<F, I>(
        &mut self,
        message: Option<Message>,
        on_event: &mut F,
        interactor: &mut I,
    ) -> Result<String>
    where
        F: FnMut(AgentEvent) + Send,
        I: Interactor,
    {
        for id in self.session.dangling_tool_calls() {
            self.session.push(Message::tool(id, "Tool call was interrupted before it completed."))?;
        }
        if self.session.messages().is_empty() {
            self.session.push(Message::system(self.system_prompt.clone()))?;
        }
        let background_only = message.is_none();
        self.guarded = false;
        if let Some(message) = message {
            // A repeated finish-gate complaint may let one stuck turn end, but
            // that escape never carries into fresh user work.
            self.last_gate = None;
            // New work: a plan that is finished has nothing left to say about
            // it, and leaving it up is how the footer came to describe a task
            // three prompts old. An unfinished one stays — the person may be
            // carrying on with it.
            if !self.plan.is_empty() && self.plan.iter().all(|step| !step.open()) {
                self.plan.clear();
                on_event(AgentEvent::Plan { steps: Vec::new() });
            }
            self.session.push(message)?;
        }
        if background_only && self.inject_background(interactor)? == 0 {
            return Ok(String::new());
        }

        let mut shrunk = false;
        for _ in 0..self.steps() {
            self.inject_background(interactor)?;
            if self.over_threshold() && !self.compaction_is_futile {
                let before = self.load();
                if let Some((notes, summary)) = self.make_room(on_event).await? {
                    on_event(AgentEvent::Compacted { notes, summary });
                }
                // Freeing almost nothing means the fixed part of the context
                // — system prompt, tool schemas, the kept tail — is what
                // fills the window. Repeating it would spend a request per
                // turn to no effect.
                if self.load() * 10 > before * 9 {
                    self.compaction_is_futile = true;
                    self.session.push(Message::kernel(
                        "Compaction could not free room: the fixed context is close to the window on \
                         its own. Work in smaller steps, read less at a time, and expect no further \
                         automatic compaction this session.",
                    ))?;
                }
            }
            self.afford()?;
            let load_sent = self.load();
            on_event(AgentEvent::ModelStart { context_chars: self.session.context_chars() });
            let model_started = std::time::Instant::now();
            let turn = self
                .plugins
                .complete(self.session.messages(), &mut |chunk| {
                    on_event(match chunk {
                        crate::provider::Chunk::Text(text) => AgentEvent::Text { text: text.into() },
                        crate::provider::Chunk::Reasoning(text) => {
                            AgentEvent::Reasoning { text: text.into() }
                        }
                    });
                })
                .await;
            let (input_tokens, output_tokens) = turn.as_ref().map_or((0, 0), |turn| turn.tokens);
            on_event(AgentEvent::ModelEnd {
                ok: turn.is_ok(),
                input_tokens,
                output_tokens,
                duration_ms: model_started.elapsed().as_millis(),
                metrics: turn.as_ref().ok().and_then(|turn| turn.metrics.clone()),
            });
            let turn = match turn {
                // The window is smaller than this agent was told, or was never
                // told at all. What was just refused is known not to fit, so
                // the room is set below it and made — once: a second refusal
                // is not about size.
                Err(Error::Overflow(_)) if !shrunk => {
                    shrunk = true;
                    self.compact_chars = self.load() * 3 / 4;
                    self.measured = None;
                    self.compaction_is_futile = false;
                    continue;
                }
                turn => turn?,
            };
            self.tokens.0 += turn.tokens.0;
            self.tokens.1 += turn.tokens.1;
            self.charge_shared_tokens(turn.tokens.0 + turn.tokens.1)?;
            if turn.tokens.0 > 0 {
                self.measured = Some((turn.tokens.0 as usize, load_sent));
            }
            let has_calls = !turn.tool_calls.is_empty();
            let answer = turn.content.clone();
            self.session.push(Message {
                role: Role::Assistant,
                content: turn.content,
                tool_calls: turn.tool_calls.clone(),
                tool_call_id: None,
                thinking: turn.thinking,
                origin: crate::message::MessageOrigin::User,
                event_id: None,
                images: Vec::new(),
            })?;

            if !has_calls {
                match self.finish(answer.unwrap_or_default(), interactor).await? {
                    Ending::Done(answer) => return Ok(answer),
                    Ending::KeepGoing => continue,
                }
            }

            self.run_tools(turn.tool_calls, on_event, interactor).await?;
        }

        // Ending as an answer rather than an error, and recorded like one.
        // A bare Err never reaches the session, so hitting the guard used to be
        // indistinguishable from the agent going quiet: the only way to find
        // out was to count tool calls in the JSONL by hand.
        let answer = format!(
            "Stopped after {} steps without finishing — the runaway guard, not the task, ended this turn. \
             Work already done is on disk and may be half-applied. Say what to do next, or raise the configured step limit.",
            self.steps()
        );
        self.guarded = true;
        self.session.push(Message::assistant_text(&answer))?;
        Ok(answer)
    }

    /// `0` means no guard: the turn ends when the agent does.
    fn steps(&self) -> usize {
        match self.max_steps {
            0 => usize::MAX,
            steps => steps,
        }
    }

    /// What happens when the model stops calling tools.
    ///
    /// Three things can still keep the work alive, asked in the order of how
    /// much authority they have: something that arrived while the turn ran, the
    /// project's own completion check, and a standing objective. Only when none
    /// speaks is the answer the answer.
    async fn finish<I: Interactor>(&mut self, answer: String, interactor: &mut I) -> Result<Ending> {
        if self.inject_background(interactor)? > 0 {
            return Ok(Ending::KeepGoing);
        }
        // The project's own completion check can refuse a finish. It hands the
        // agent what is wrong and lets it try again — and stops asking once the
        // same complaint comes back unchanged, because an agent that cannot
        // satisfy a gate must still be able to end its turn.
        let finish_effects =
            match self.plugins.dispatch(MiddlewareEvent::BeforeFinish { answer: &answer }).await {
                Ok(effects) => effects,
                Err(refused) => {
                    let message = format!("Error: {refused}");
                    if self.last_gate.as_deref() == Some(&message) {
                        self.refresh_evidence().await;
                        return Ok(Ending::Done(answer));
                    }
                    self.last_gate = Some(message.clone());
                    self.session.push(Message::kernel(message))?;
                    return Ok(Ending::KeepGoing);
                }
            };
        let finish_commands = finish_effects
            .into_iter()
            .filter_map(|effect| match effect {
                MiddlewareEffect::RecordCommandEvidence(evidence) => Some(evidence),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !finish_commands.is_empty() {
            let tree = self.tree_hash().await;
            for evidence in finish_commands {
                self.evidence.record(
                    "completion_hook",
                    "project completion policy passed",
                    Some(evidence.command),
                    tree.clone(),
                    "passed",
                    evidence.level,
                );
            }
        }
        self.refresh_evidence().await;
        let actual = self.evidence.level();
        if !self.evidence.satisfies(self.required_verification) {
            let message = format!(
                "Error: completion requires {:?} verification, and the current tree has only {:?}. A command you ran counts as having passed, not as proof of what it tested: the proof is the project's own completion check — a before_completion hook in .elmee/hooks.json, which the harness runs itself. If the project has none, say plainly that the work is unverified.",
                self.required_verification, actual
            );
            if self.last_gate.as_deref() == Some(&message) {
                return Ok(Ending::Done(answer));
            }
            self.last_gate = Some(message.clone());
            self.session.push(Message::kernel(message))?;
            return Ok(Ending::KeepGoing);
        }
        self.last_gate = None;
        let continuations = self
            .plugins
            .dispatch(MiddlewareEvent::TurnFinished {
                answer: &answer,
                out_of_tokens: self.tokens_left() == Some(0),
            })
            .await?
            .into_iter()
            .filter_map(|effect| match effect {
                MiddlewareEffect::Continue(prompt) => Some(prompt),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !continuations.is_empty() {
            let prompt = continuations.join("\n\n");
            self.session.push(Message::kernel(prompt))?;
            Ok(Ending::KeepGoing)
        } else {
            // No second `refresh_evidence` here: the tree was hashed a few
            // lines up and nothing between reads the disk, so asking git again
            // cost two more process starts to learn the same hash.
            Ok(Ending::Done(answer))
        }
    }

    /// Carry out the calls one turn asked for.
    ///
    /// Four kinds, in a deliberate order: a continuation's completion claim, a question for
    /// the user, something the frontend answers, and everything else — which
    /// is the only kind a project's hook gets to refuse.
    async fn run_tools<F, I>(
        &mut self,
        calls: Vec<crate::message::ToolCall>,
        on_event: &mut F,
        interactor: &mut I,
    ) -> Result<()>
    where
        F: FnMut(AgentEvent) + Send,
        I: Interactor,
    {
        // Tools are the only thing that writes, so this is the one moment the
        // Application barriers started before the model call must land before
        // the first external effect.
        self.settle_barriers().await;
        // A pure plugin batch can overlap authorized reads. Classify without
        // executing anything so host/internal effects retain call order.
        let batchable = calls.len() > 1
            && calls.iter().all(|call| !self.answers(call) && !interactor.handles_host_tool(call));
        let mut refusals = Vec::new();
        let mut pictures = Vec::new();
        if batchable {
            for call in &calls {
                refusals.push(self.before_tool(call).await);
            }
            let allowed = calls
                .iter()
                .zip(&refusals)
                .filter(|(_, refusal)| refusal.is_ok())
                .map(|(call, _)| call.clone())
                .collect::<Vec<_>>();
            self.plugins.prepare_tools(&allowed).await;
        }
        for (index, call) in calls.into_iter().enumerate() {
            // Loop-owned control calls and a plugin's completion claim need
            // private turn state, so the loop answers them before ordinary
            // handlers run.
            if let Some(result) = self.answer(&call, on_event, interactor).await {
                emit_tool(&call, result, on_event, &mut self.session)?;
                continue;
            }
            let refused = if batchable {
                std::mem::replace(&mut refusals[index], Ok(()))
            } else {
                self.before_tool(&call).await
            };
            on_event(AgentEvent::ToolStart {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            });
            let started = std::time::Instant::now();
            let mut executed = false;
            let mut outcome = if let Err(reason) = refused {
                crate::ToolOutcome::new(reason, false)
            } else if let Some(output) = interactor.host_tool(&call).await {
                // A capability of the active frontend rather than a plugin
                // effect, so nothing in the graph ran and there is no evidence
                // or AfterTool to bind. It passed the same gate as any other
                // call, classified by whoever offered its schema, and it
                // reports failure in prose like the rest of the host layer.
                let ok = !answers::reported_failure(&output);
                crate::ToolOutcome::new(output, ok)
            } else {
                executed = true;
                self.plugins.execute(&call).await
            };
            if outcome.ok
                && let Some(request) = outcome.completion.take()
            {
                let asked = match self.afford() {
                    Ok(()) => self.plugins.complete_without_tools(&request.messages, &mut |_| {}).await,
                    Err(spent) => Err(spent),
                };
                match asked {
                    Ok(turn) => {
                        self.tokens.0 += turn.tokens.0;
                        self.tokens.1 += turn.tokens.1;
                        let _ = self.charge_shared_tokens(turn.tokens.0 + turn.tokens.1);
                        outcome.output = format!(
                            "{}{}{}",
                            request.prefix,
                            turn.content.as_deref().unwrap_or("the completion produced no answer"),
                            request.suffix
                        );
                    }
                    Err(error) => {
                        outcome.output = format!("{}\n\n[tool completion failed: {error}]", request.fallback);
                    }
                }
            }
            let ceiling = self.output_ceiling();
            if outcome.output.len() > ceiling {
                outcome.output = format!(
                    "{}\n[shortened to fit this model's context; ask for less at a time — a narrower \
                     command, a range of lines]",
                    crate::text::middle(&outcome.output, ceiling)
                );
            }
            if executed {
                match self
                    .plugins
                    .dispatch(MiddlewareEvent::AfterTool {
                        call: &call,
                        output: &outcome.output,
                        ok: outcome.ok,
                    })
                    .await
                {
                    // Context that arrives with a result rather than in the
                    // system prompt: local rules a call just made relevant,
                    // paid for once and without disturbing the cached prefix.
                    Ok(effects) => {
                        for effect in effects {
                            if let MiddlewareEffect::AddContext(text) = effect {
                                outcome.output.push('\n');
                                outcome.output.push_str(&text);
                            }
                        }
                    }
                    Err(error) => {
                        outcome.output.push_str(&format!("\nError: {error}"));
                        outcome.ok = false;
                    }
                }
            }
            if executed
                && outcome.ok
                && let Some(evidence) = outcome.evidence
            {
                let tree = self.tree_hash().await;
                self.evidence.record(
                    crate::evidence::OBSERVED,
                    format!("{} completed successfully", evidence.command),
                    Some(evidence.command),
                    tree,
                    &outcome.output,
                    evidence.level,
                );
            }
            on_event(AgentEvent::ToolEnd {
                id: call.id.clone(),
                name: call.function.name.clone(),
                ok: outcome.ok,
                duration_ms: started.elapsed().as_millis(),
                output: outcome.output.clone(),
            });
            self.session.push(Message::tool(call.id.clone(), outcome.output))?;
            if !outcome.images.is_empty() {
                pictures.push((call.id, outcome.images));
            }
        }
        // After every result of the batch, so the results stay one block: a
        // picture is shown the way a person would show it, as the next thing
        // said, named for the call that took it.
        for (id, images) in pictures {
            let mut message =
                Message::background(format!("image:{id}"), format!("What tool call {id} pictured:"));
            message.images = images;
            self.session.push(message)?;
        }
        Ok(())
    }

    /// The project's own say over a tool call, before it runs.
    ///
    /// A hook a repository declares may refuse; nothing else here does. What a
    /// command may reach is decided by the machine the agent runs on, not by
    /// anything inside the turn.
    async fn before_tool(&mut self, call: &crate::message::ToolCall) -> std::result::Result<(), String> {
        match self.plugins.dispatch(MiddlewareEvent::BeforeTool { call }).await {
            Err(error) => Err(format!("Error: {error}")),
            Ok(_) => Ok(()),
        }
    }

    fn inject_background<I: Interactor>(&mut self, interactor: &mut I) -> Result<usize> {
        let events = interactor.background_events();
        let mut count = 0;
        let mut ids = Vec::with_capacity(events.len());
        for event in events {
            if !self.session.contains_event(&event.id) {
                self.session.push(Message::background(event.id.clone(), event.prompt))?;
                count += 1;
            }
            ids.push(event.id);
        }
        interactor.ack_background(&ids);
        Ok(count)
    }

    pub fn extension_report(&self) -> String {
        self.plugins.report()
    }

    /// Where the work is. The frontend needs it to keep a per-project record
    /// of what the harness has taught itself.
    pub fn cwd(&self) -> &std::path::Path {
        self.workspace.cwd()
    }

    /// The name a person gave this conversation, if they gave one.
    pub fn title(&self) -> Option<&str> {
        self.session.title()
    }

    pub fn set_title(&mut self, title: String) -> Result<()> {
        self.session.set_title(title)
    }

    /// What was asked on the active branch, oldest first.
    pub fn prompts(&self) -> Vec<crate::session::Prompt> {
        self.session.prompts()
    }

    /// Every line of conversation this session holds, newest first.
    pub fn branches(&self) -> Vec<crate::session::Branch> {
        self.session.branches()
    }

    /// Continue from the end of another branch. The plan belonged to the
    /// branch being left.
    pub fn checkout(&mut self, id: u64) -> Result<()> {
        self.session.checkout(id)?;
        self.plan.clear();
        Ok(())
    }

    /// Go back to just before a prompt and return it for editing.
    pub fn rewind(&mut self, id: u64) -> Result<crate::session::Prompt> {
        let prompt = self.session.rewind(id)?;
        self.plan.clear();
        Ok(prompt)
    }

    /// Drop the active context (a fresh branch from the session root); the
    /// system prompt is re-inserted on the next turn.
    pub fn clear(&mut self) -> Result<()> {
        self.session.reset()?;
        self.plan.clear();
        Ok(())
    }

    /// Whether the step guard, not the agent, ended the last turn.
    pub fn stopped_by_guard(&self) -> bool {
        self.guarded
    }

    pub fn session_path(&self) -> Option<&std::path::Path> {
        self.session.path()
    }

    pub fn context_chars(&self) -> usize {
        self.session.context_chars()
    }

    /// The conversation as the next request will send it. Read-only: what may
    /// be pushed is the loop's own business.
    pub fn messages(&self) -> &[Message] {
        self.session.messages()
    }

    /// Change the compaction threshold mid-session — a model switch changes
    /// the window this stands for.
    pub fn set_compact_chars(&mut self, chars: usize) {
        self.compact_chars = chars;
    }

    /// Where the shared token ceiling is kept, so anything this run starts on
    /// its own — a background agent, a command that is a program — spends
    /// against the same budget instead of opening a second one.
    pub fn token_ledger(&self) -> Option<&std::path::Path> {
        self.token_ledger.as_ref().and_then(|ledger| ledger.path())
    }

    /// Accumulated (input, output) tokens, as reported by the provider.
    pub fn tokens(&self) -> (u64, u64) {
        self.tokens
    }

    pub fn set_model(&mut self, model: String) {
        let _ = self.plugins.set_model(model);
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<crate::provider::ReasoningEffort>) {
        let _ = self.plugins.set_reasoning_effort(effort);
    }

    pub fn reasoning_effort(&self) -> Option<crate::provider::ReasoningEffort> {
        self.plugins.reasoning_effort()
    }

    /// The active model's context window, when the provider knows it. A
    /// frontend reads this after a switch to re-derive its compaction budget:
    /// a threshold sized for the previous model is a threshold for a window
    /// that is no longer there.
    pub fn context_window(&self) -> Option<usize> {
        self.plugins.context_window()
    }

    /// Swap to another session (for /new and /resume). What was proven in
    /// the one being left says nothing about this one.
    pub fn replace_session(&mut self, session: Session) {
        self.session = session;
        self.evidence = EvidenceManifest::default();
    }
}

#[cfg(test)]
mod tests;
