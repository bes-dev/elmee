//! The plugin contract and open registry consumed by the loop.
//!
//! A plugin declares typed services, handlers and middleware into an isolated
//! [`Registrar`]. [`Registry`] atomically merges contributions, checks what the
//! loop itself requires and freezes the graph before anything can run.
//! Independent declarations must commute, and a new universal role belongs
//! here only after two unrelated production features prove that the existing
//! three cannot express it. What one plugin needs of another it is handed by
//! the composition root, as a constructor argument.

use std::{
    any::{Any, TypeId},
    cell::RefCell,
    collections::HashMap,
    future::Future,
    path::Path,
    pin::Pin,
    rc::Rc,
};

use crate::provider::{AssistantTurn, Chunk, Provider};
use crate::{Error, Result, message::ToolCall};
use crate::{agent::TokenLedger, session::Session};

pub type PluginFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Kernel model port stored through the same typed service registry as every
/// other cross-plugin capability.
pub struct ModelService(RefCell<Box<dyn Provider>>);

impl ModelService {
    pub fn new(provider: impl Provider + 'static) -> Self {
        Self(RefCell::new(Box::new(provider)))
    }
}

/// One session value offered to a newly built agent.
pub struct SessionSeed(RefCell<Option<Session>>);

impl SessionSeed {
    pub fn new(session: Session) -> Self {
        Self(RefCell::new(Some(session)))
    }

    fn take(&self) -> Option<Session> {
        self.0.borrow_mut().take()
    }
}

/// A coarse model call requested by a tool after it has produced data.
/// The loop performs and accounts for it without knowing which tool asked.
pub struct CompletionRequest {
    pub messages: Vec<crate::message::Message>,
    pub prefix: String,
    pub suffix: String,
    pub fallback: String,
}

/// Everything learned while executing one tool call. Keeping post-execution
/// facts with the result avoids asking the handler a second time after its
/// state may already have changed.
pub struct ToolOutcome {
    pub output: String,
    pub ok: bool,
    pub completion: Option<CompletionRequest>,
    pub evidence: Option<CommandEvidence>,
    /// Pictures the tool took, for the model to look at: a screenshot, a
    /// rendered chart. They reach the model in the turn that follows the
    /// results, since no wire lets a result itself carry one for every vendor.
    pub images: Vec<crate::message::Image>,
}

impl ToolOutcome {
    pub fn new(output: impl Into<String>, ok: bool) -> Self {
        Self { output: output.into(), ok, completion: None, evidence: None, images: Vec::new() }
    }
}

/// A successful command observation classified by the plugin that knows the
/// command ecosystem. Core only binds the observation to the current tree.
pub struct CommandEvidence {
    pub command: String,
    pub level: crate::evidence::VerificationLevel,
}

/// A plugin-owned request to finish a standing objective. The kernel owns the
/// independent check; the plugin owns the objective and the state transition.
pub struct CompletionClaim {
    pub evidence: String,
    pub context: String,
}

#[derive(Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
}

pub struct CommandOutcome {
    pub message: String,
    pub prompt: Option<String>,
}

impl CommandOutcome {
    pub fn message(message: impl Into<String>) -> Self {
        Self { message: message.into(), prompt: None }
    }

    pub fn prompt(message: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self { message: message.into(), prompt: Some(prompt.into()) }
    }
}

/// A value being computed while the turn does something else.
pub struct Pending(Pin<Box<dyn Future<Output = Option<String>> + Send>>);

impl Pending {
    pub fn new(work: impl Future<Output = Option<String>> + Send + 'static) -> Self {
        Self(Box::pin(work))
    }

    pub async fn settle(self) -> Option<String> {
        self.0.await
    }
}

/// The only common contract required of an Elmee plugin.
///
/// Declaration is deliberately write-only. A plugin can publish roles, but
/// cannot inspect its neighbours or observe a half-built
/// composition. The registry contribution becomes visible atomically only if
/// declaration and conflict checks succeed. Arbitrary Rust code cannot be
/// proven pure, so implementations are contractually required to keep
/// `declare` inert; constructors and discovery belong to construction.
pub trait Plugin: Sized {
    fn id(&self) -> &'static str;
    fn declare(self, target: &mut Registrar) -> Result<()>;
}

mod services;
pub use services::*;

/// One event at a stable point in the agent lifecycle.
#[non_exhaustive]
#[derive(Clone, Copy)]
pub enum MiddlewareEvent<'a> {
    UserPrompt {
        prompt: &'a str,
    },
    BeforeTool {
        call: &'a ToolCall,
    },
    AfterTool {
        call: &'a ToolCall,
        output: &'a str,
        ok: bool,
    },
    BeforeFinish {
        answer: &'a str,
    },
    TurnFinished {
        answer: &'a str,
        out_of_tokens: bool,
    },
    /// The older part of the conversation is about to be replaced by a
    /// summary; `messages` is that part.
    BeforeCompaction {
        messages: &'a [crate::message::Message],
    },
    Compacting {
        summary: &'a str,
    },
}

/// A bounded instruction from middleware back to the kernel.
#[non_exhaustive]
pub enum MiddlewareEffect {
    AddContext(String),
    /// At compaction: text to keep after the summary exactly as given —
    /// what a plugin knows for a fact and a summary could only paraphrase.
    Carry(String),
    Continue(String),
    ReplaceText {
        text: String,
        notes: usize,
    },
    RecordCommandEvidence(CommandEvidence),
}

/// Stable semantic order for global behavior modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MiddlewarePhase {
    Context,
    Policy,
    Transform,
    Continuation,
}

/// Composable policy around a turn, ordered by phase and plugin identity.
pub trait Middleware {
    fn phase(&self) -> MiddlewarePhase {
        MiddlewarePhase::Transform
    }

    fn on<'a>(&'a mut self, _event: MiddlewareEvent<'a>) -> PluginFuture<'a, Result<Vec<MiddlewareEffect>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// A set of model-facing or application-facing operations contributed by one plugin.
pub trait Handler {
    fn definitions(&self) -> &[serde_json::Value];
    fn execute<'a>(&'a mut self, call: &'a ToolCall) -> PluginFuture<'a, ToolOutcome>;
    /// Prepare an already-authorized batch. Implementations may overlap
    /// reversible reads, but must not make writes or other irreversible
    /// effects; the loop calls this only after every call passed its gate.
    fn prepare<'a>(&'a mut self, _calls: &'a [ToolCall]) -> PluginFuture<'a, ()> {
        Box::pin(async {})
    }
    fn report(&self) -> String {
        String::new()
    }
    fn commands(&self) -> &[CommandSpec] {
        &[]
    }
    fn command(&mut self, _name: &str, _argument: &str) -> Option<Result<CommandOutcome>> {
        None
    }
    fn completion_claim(&self, _call: &ToolCall) -> Option<CompletionClaim> {
        None
    }
    fn accept_completion(&mut self, _call: &ToolCall) -> bool {
        false
    }
}

/// The world of files visible to an agent.
pub trait Workspace {
    fn cwd(&self) -> &Path;
    fn attachments(&self, _prompt: &str) -> Vec<crate::message::Image> {
        Vec::new()
    }
    fn fingerprint(&self) -> PluginFuture<'_, Option<String>> {
        Box::pin(async { None })
    }
}

pub(crate) struct LocalWorkspace;
impl Workspace for LocalWorkspace {
    fn cwd(&self) -> &Path {
        Path::new(".")
    }
}

mod registry;
pub use registry::*;

impl Plugin for Session {
    fn id(&self) -> &'static str {
        "session"
    }

    fn declare(self, plugins: &mut Registrar) -> Result<()> {
        plugins.provide_service(Rc::new(SessionSeed::new(self)));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
