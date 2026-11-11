//! The registrar a plugin declares into and the resolved registry the loop
//! consumes: atomic merging, tool ownership, filtering and dispatch.

use super::*;

struct HandlerEntry {
    owner: &'static str,
    value: Box<dyn Handler>,
    definitions: Vec<serde_json::Value>,
    tools: Vec<String>,
    commands: Vec<CommandSpec>,
}

impl HandlerEntry {
    /// What a handler offers is asked once and kept: its schemas and commands
    /// are never queried again after this.
    fn new(owner: &'static str, value: Box<dyn Handler>) -> Result<Self> {
        let definitions = value.definitions().to_vec();
        let mut tools = Vec::new();
        for definition in &definitions {
            let Some(name) = definition["function"]["name"].as_str() else {
                return Err(Error::Config("tool plugin declared a schema without a name".into()));
            };
            if tools.iter().any(|known| known == name) {
                return Err(Error::Config(format!("handler declares tool {name} twice")));
            }
            tools.push(name.to_owned());
        }
        let commands = value.commands().to_vec();
        for (index, command) in commands.iter().enumerate() {
            if commands[..index].iter().any(|known| known.name == command.name) {
                return Err(Error::Config(format!("handler declares command {} twice", command.name)));
            }
        }
        Ok(Self { owner, value, definitions, tools, commands })
    }
}

struct MiddlewareEntry {
    owner: &'static str,
    phase: MiddlewarePhase,
    value: Box<dyn Middleware>,
}

/// A single plugin's isolated, write-only declaration transaction.
pub struct Registrar {
    owner: &'static str,
    services: Services,
    middleware: Vec<MiddlewareEntry>,
    handlers: Vec<HandlerEntry>,
}

impl Registrar {
    fn new(owner: &'static str) -> Self {
        Self { owner, services: Services::default(), middleware: Vec::new(), handlers: Vec::new() }
    }

    pub fn provide_service<T: ?Sized + 'static>(&mut self, value: Rc<T>) {
        self.services.provide(self.owner, value);
    }

    pub fn provide_handler(&mut self, value: impl Handler + 'static) -> Result<()> {
        let entry = HandlerEntry::new(self.owner, Box::new(value))?;
        for name in &entry.tools {
            if self.tool_names().any(|known| known == name) {
                return Err(Error::Config(format!("plugin {} declares tool {name} twice", self.owner)));
            }
        }
        for command in &entry.commands {
            if self.command_names().any(|known| known == command.name) {
                return Err(Error::Config(format!(
                    "plugin {} declares command {} twice",
                    self.owner, command.name
                )));
            }
        }
        self.handlers.push(entry);
        Ok(())
    }

    pub fn provide_middleware(&mut self, value: impl Middleware + 'static) -> Result<()> {
        if !self.middleware.is_empty() {
            return Err(Error::Config(format!("plugin {} declares more than one middleware", self.owner)));
        }
        let phase = value.phase();
        self.middleware.push(MiddlewareEntry { owner: self.owner, phase, value: Box::new(value) });
        Ok(())
    }

    fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.handlers.iter().flat_map(|handler| handler.tools.iter().map(String::as_str))
    }

    fn command_names(&self) -> impl Iterator<Item = &str> {
        self.handlers.iter().flat_map(|handler| handler.commands.iter()).map(|command| command.name)
    }
}

/// A resolved registry populated by atomic plugin contributions.
#[derive(Default)]
pub struct Registry {
    installed: Vec<&'static str>,
    services: Services,
    middleware: Vec<MiddlewareEntry>,
    definitions: Vec<serde_json::Value>,
    /// Schemas answered by the loop itself, owned by no handler.
    kernel: Vec<serde_json::Value>,
    handlers: Vec<HandlerEntry>,
    requirements: Vec<Requirement>,
    resolved: bool,
    allowed_tools: Option<std::collections::BTreeSet<String>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&mut self, plugin: impl Plugin + 'static) -> Result<&mut Self> {
        if self.resolved {
            return Err(Error::Config("composition is already resolved".into()));
        }
        let id = plugin.id();
        if id == "core-agent" {
            return Err(Error::Config("plugin id core-agent is reserved for the kernel".into()));
        }
        if self.installed.contains(&id) {
            return Err(Error::Config(format!("plugin {id} is already installed")));
        }
        let mut contribution = Registrar::new(id);
        plugin.declare(&mut contribution)?;
        self.commit(contribution)?;
        Ok(self)
    }

    fn commit(&mut self, contribution: Registrar) -> Result<()> {
        let mut definitions = self.definitions.clone();
        for name in contribution.tool_names() {
            if definitions.iter().any(|known| known["function"]["name"].as_str() == Some(name)) {
                return Err(Error::Config(format!("tool plugin collides on tool {name}")));
            }
        }
        for handler in &contribution.handlers {
            definitions.extend_from_slice(&handler.definitions);
        }
        definitions.sort_by(|left, right| {
            left["function"]["name"].as_str().cmp(&right["function"]["name"].as_str())
        });
        for name in contribution.command_names() {
            if self.handlers.iter().any(|handler| handler.commands.iter().any(|command| command.name == name))
            {
                return Err(Error::Config(format!("plugins collide on command {name}")));
            }
        }
        let Registrar { owner, services, middleware, handlers } = contribution;
        self.services.merge(services);
        self.middleware.extend(middleware);
        self.middleware.sort_by_key(|entry| (entry.phase, entry.owner));
        self.handlers.extend(handlers);
        self.handlers.sort_by_key(|entry| entry.owner);
        self.installed.push(owner);
        self.installed.sort_unstable();
        self.definitions = definitions;
        Ok(())
    }

    pub fn installed(&self) -> &[&'static str] {
        &self.installed
    }

    /// Restrict the complete model-facing tool namespace before resolution.
    /// The filter applies across handlers, so application policy cannot be
    /// bypassed merely because a tool moved into another plugin.
    pub fn allow_tools(&mut self, names: &[String]) -> Result<()> {
        if self.resolved {
            return Err(Error::Config("composition is already resolved".into()));
        }
        self.allowed_tools = Some(names.iter().cloned().collect());
        Ok(())
    }

    /// Add tools implemented by the kernel itself. They join the same checked
    /// and filtered namespace as plugin tools, but need no handler because the
    /// loop answers them from its private state.
    pub(crate) fn add_kernel_tools(&mut self, definitions: Vec<serde_json::Value>) -> Result<()> {
        if self.resolved {
            return Err(Error::Config("composition is already resolved".into()));
        }
        for definition in &definitions {
            let Some(name) = definition["function"]["name"].as_str() else {
                return Err(Error::Config("kernel declared a tool schema without a name".into()));
            };
            if self.definitions.iter().any(|known| known["function"]["name"].as_str() == Some(name)) {
                return Err(Error::Config(format!("kernel tool collides on tool {name}")));
            }
        }
        self.kernel.extend(definitions.iter().cloned());
        self.definitions.extend(definitions);
        self.definitions.sort_by(|left, right| {
            left["function"]["name"].as_str().cmp(&right["function"]["name"].as_str())
        });
        Ok(())
    }

    pub fn services(&self) -> Result<Services> {
        if !self.resolved {
            return Err(Error::Config("composition must be resolved before services are used".into()));
        }
        Ok(self.services.clone())
    }

    pub async fn dispatch(&mut self, event: MiddlewareEvent<'_>) -> Result<Vec<MiddlewareEffect>> {
        if !self.resolved {
            return Err(Error::Config("composition must be resolved before middleware runs".into()));
        }
        let mut effects = Vec::new();
        for middleware in &mut self.middleware {
            effects.extend(middleware.value.on(event).await?);
        }
        Ok(effects)
    }

    /// Resolve every declared dependency and freeze the composition.
    pub fn resolve(&mut self) -> Result<()> {
        if self.resolved {
            return Ok(());
        }
        for requirement in &self.requirements {
            self.check(
                requirement.owner,
                requirement.type_id,
                requirement.type_name,
                requirement.cardinality,
            )?;
        }
        if let Some(allowed) = &self.allowed_tools {
            let known = self
                .definitions
                .iter()
                .filter_map(|definition| definition["function"]["name"].as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let missing =
                allowed.iter().filter(|name| !known.contains(name.as_str())).cloned().collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(Error::Config(format!("unknown tool(s) in --tools: {}", missing.join(", "))));
            }
            for handler in &mut self.handlers {
                handler.definitions.retain(|definition| {
                    definition["function"]["name"].as_str().is_some_and(|name| allowed.contains(name))
                });
                handler.tools.retain(|name| allowed.contains(name));
            }
            self.definitions.retain(|definition| {
                definition["function"]["name"].as_str().is_some_and(|name| allowed.contains(name))
            });
            self.kernel.retain(|definition| {
                definition["function"]["name"].as_str().is_some_and(|name| allowed.contains(name))
            });
        }
        self.resolved = true;
        Ok(())
    }

    /// Resolve the additional capabilities required by an agent loop.
    pub(crate) fn resolve_agent(&mut self) -> Result<()> {
        if self.resolved {
            return Err(Error::Config("agent composition was resolved before kernel validation".into()));
        }
        self.kernel_requirement::<dyn Workspace>(Cardinality::AtMost);
        self.kernel_requirement::<dyn TokenLedger>(Cardinality::AtMost);
        self.kernel_requirement::<ModelService>(Cardinality::Exactly);
        self.kernel_requirement::<SessionSeed>(Cardinality::AtMost);
        self.resolve()
    }

    fn kernel_requirement<T: ?Sized + 'static>(&mut self, cardinality: Cardinality) {
        let type_id = TypeId::of::<ServiceValue<T>>();
        if !self.requirements.iter().any(|requirement| {
            requirement.owner == "core-agent"
                && requirement.type_id == type_id
                && requirement.cardinality == cardinality
        }) {
            self.requirements.push(Requirement {
                owner: "core-agent",
                type_id,
                type_name: std::any::type_name::<T>(),
                cardinality,
            });
            self.requirements.sort_by_key(|requirement| (requirement.owner, requirement.type_name));
        }
    }

    fn check(&self, owner: &str, type_id: TypeId, type_name: &str, cardinality: Cardinality) -> Result<()> {
        let count = self.services.count(type_id);
        let valid = match cardinality {
            Cardinality::Exactly => count == 1,
            Cardinality::AtMost => count <= 1,
        };
        if valid {
            Ok(())
        } else {
            Err(Error::Config(format!(
                "plugin {owner} requires {cardinality:?} provider of {type_name}; found {count}"
            )))
        }
    }

    /// Stable semantic description of the resolved graph, suitable for diffs.
    pub fn manifest(&self) -> String {
        let mut lines = vec![format!("state: {}", if self.resolved { "resolved" } else { "declared" })];
        lines.push(format!("plugins: {}", self.installed.join(", ")));
        for (name, owners) in self.services.providers() {
            lines.push(format!("service {name}: {}", owners.join(", ")));
        }
        for requirement in &self.requirements {
            lines.push(format!(
                "requires {}: {:?} {}",
                requirement.owner, requirement.cardinality, requirement.type_name
            ));
        }
        for (position, middleware) in self.middleware.iter().enumerate() {
            lines.push(format!("middleware {position:02} {}: {:?}", middleware.owner, middleware.phase));
        }
        for definition in &self.kernel {
            if let Some(name) = definition["function"]["name"].as_str() {
                lines.push(format!("tool {name}: core-agent"));
            }
        }
        for handler in &self.handlers {
            for definition in &handler.definitions {
                if let Some(name) = definition["function"]["name"].as_str() {
                    lines.push(format!("tool {name}: {}", handler.owner));
                }
            }
            for command in &handler.commands {
                lines.push(format!("command {}: {}", command.name, handler.owner));
            }
        }
        lines.sort_unstable();
        lines.join("\n")
    }

    pub(crate) fn take_session(&mut self) -> Session {
        self.services
            .one::<SessionSeed>()
            .ok()
            .flatten()
            .and_then(|seed| seed.take())
            .unwrap_or_else(Session::memory)
    }

    pub(crate) fn workspace(&self) -> Result<Rc<dyn Workspace>> {
        Ok(self.services.one::<dyn Workspace>()?.unwrap_or_else(|| Rc::new(LocalWorkspace)))
    }

    pub(crate) fn token_ledger(&self) -> Result<Option<Rc<dyn TokenLedger>>> {
        self.services.one::<dyn TokenLedger>()
    }

    pub fn definitions(&self) -> &[serde_json::Value] {
        &self.definitions
    }
    #[allow(clippy::await_holding_refcell_ref)] // one agent serializes model calls and model switches
    pub async fn complete(
        &self,
        messages: &[crate::message::Message],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        self.complete_with(messages, &self.definitions, on_text).await
    }
    /// A private model request used to summarize existing data. Auxiliary
    /// requests must never invite a new tool call or pay to resend schemas.
    #[allow(clippy::await_holding_refcell_ref)] // one agent serializes its provider requests
    pub async fn complete_without_tools(
        &self,
        messages: &[crate::message::Message],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        self.complete_with(messages, &[], on_text).await
    }

    #[allow(clippy::await_holding_refcell_ref)] // one agent serializes its provider requests
    async fn complete_with(
        &self,
        messages: &[crate::message::Message],
        definitions: &[serde_json::Value],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        let provider = self.model()?;
        let provider = provider.0.borrow();
        provider.complete(messages, definitions, on_text).await
    }
    pub fn set_model(&mut self, model: String) -> Result<()> {
        let provider = self.model()?;
        provider.0.borrow_mut().set_model(model);
        Ok(())
    }
    pub fn set_reasoning_effort(&mut self, effort: Option<crate::provider::ReasoningEffort>) -> Result<()> {
        let provider = self.model()?;
        provider.0.borrow_mut().set_reasoning_effort(effort);
        Ok(())
    }
    pub fn reasoning_effort(&self) -> Option<crate::provider::ReasoningEffort> {
        self.model().ok()?.0.borrow().reasoning_effort()
    }
    pub fn context_window(&self) -> Option<usize> {
        self.model().ok()?.0.borrow().context_window()
    }
    pub async fn execute(&mut self, call: &ToolCall) -> ToolOutcome {
        if !self.resolved {
            return ToolOutcome::new("Error: composition must be resolved before tools run", false);
        }
        let Some(index) = self.handlers.iter().position(|plugin| plugin.tools.contains(&call.function.name))
        else {
            return ToolOutcome::new(format!("Error: no plugin owns tool {}", call.function.name), false);
        };
        self.handlers[index].value.execute(call).await
    }
    pub async fn prepare_tools(&mut self, calls: &[ToolCall]) {
        for handler in &mut self.handlers {
            let owned = calls
                .iter()
                .filter(|call| handler.tools.contains(&call.function.name))
                .cloned()
                .collect::<Vec<_>>();
            if !owned.is_empty() {
                handler.value.prepare(&owned).await;
            }
        }
    }
    /// Whether a name survived resolution into the model-visible namespace.
    /// The loop consults this before answering its own tools, so a filtered
    /// kernel tool fails closed exactly like a filtered plugin tool.
    pub(crate) fn has_tool(&self, name: &str) -> bool {
        self.definitions.iter().any(|definition| definition["function"]["name"].as_str() == Some(name))
    }

    pub fn report(&self) -> String {
        self.handlers
            .iter()
            .filter_map(|plugin| {
                let report = plugin.value.report();
                (!report.is_empty()).then_some(report)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    pub fn commands(&self) -> Vec<CommandSpec> {
        let mut commands =
            self.handlers.iter().flat_map(|handler| handler.commands.iter().copied()).collect::<Vec<_>>();
        commands.sort_by_key(|command| command.name);
        commands
    }
    pub fn command(&mut self, name: &str, argument: &str) -> Option<Result<CommandOutcome>> {
        if !self.resolved {
            return Some(Err(Error::Config("composition must be resolved before commands run".into())));
        }
        self.handlers
            .iter_mut()
            .find(|handler| handler.commands.iter().any(|command| command.name == name))
            .and_then(|handler| handler.value.command(name, argument))
    }
    pub fn completion_claim(&self, call: &ToolCall) -> Option<CompletionClaim> {
        self.handler_for(call).and_then(|handler| handler.value.completion_claim(call))
    }
    pub fn accept_completion(&mut self, call: &ToolCall) -> bool {
        self.handler_for_mut(call).is_some_and(|handler| handler.value.accept_completion(call))
    }

    fn model(&self) -> Result<Rc<ModelService>> {
        self.services
            .one::<ModelService>()?
            .ok_or_else(|| Error::Config("no provider plugin is installed".into()))
    }

    fn handler_for(&self, call: &ToolCall) -> Option<&HandlerEntry> {
        self.handlers.iter().find(|handler| handler.tools.contains(&call.function.name))
    }

    fn handler_for_mut(&mut self, call: &ToolCall) -> Option<&mut HandlerEntry> {
        self.handlers.iter_mut().find(|handler| handler.tools.contains(&call.function.name))
    }
}
