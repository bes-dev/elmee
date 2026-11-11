//! Compile-time agent assembly from plugins.

use crate::{Agent, Plugin, Registry, Result};

/// A small composition root reusable by binaries, tests and embeddings.
///
/// Plugin code is linked by Cargo; this builder only chooses which linked
/// plugins constitute this agent instance.
pub struct AgentBuilder {
    plugins: Registry,
    system_prompt: String,
    max_steps: usize,
    compact_chars: usize,
    interactive: bool,
}

impl AgentBuilder {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            plugins: Registry::new(),
            system_prompt: system_prompt.into(),
            max_steps: 0,
            compact_chars: 0,
            interactive: true,
        }
    }

    pub fn limits(mut self, max_steps: usize, compact_chars: usize) -> Self {
        self.max_steps = max_steps;
        self.compact_chars = compact_chars;
        self
    }

    pub fn plugin(&mut self, plugin: impl Plugin + 'static) -> Result<&mut Self> {
        self.plugins.install(plugin)?;
        Ok(self)
    }

    /// Whether the frontend can answer questions. A one-shot frontend must
    /// not offer `ask_user` only to cancel every call the model makes to it.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    pub fn allow_tools(&mut self, names: &[String]) -> Result<&mut Self> {
        self.plugins.allow_tools(names)?;
        Ok(self)
    }

    pub fn build(self) -> Result<Agent> {
        Agent::new_with_interaction(
            self.plugins,
            self.max_steps,
            self.compact_chars,
            self.system_prompt,
            self.interactive,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AssistantTurn, Chunk, Provider};
    use std::rc::Rc;

    struct Model;

    #[crate::async_trait(?Send)]
    impl Provider for Model {
        async fn complete(
            &self,
            _: &[crate::message::Message],
            _: &[serde_json::Value],
            _: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
        ) -> Result<AssistantTurn> {
            Ok(AssistantTurn::text("ok"))
        }

        fn set_model(&mut self, _: String) {}

        async fn list_models(&self) -> Result<Vec<crate::provider::Model>> {
            Ok(Vec::new())
        }
    }

    struct ModelPlugin;
    impl Plugin for ModelPlugin {
        fn id(&self) -> &'static str {
            "test-model"
        }

        fn declare(self, plugins: &mut crate::Registrar) -> Result<()> {
            plugins.provide_service(Rc::new(crate::ModelService::new(Model)));
            Ok(())
        }
    }

    #[test]
    fn a_binary_can_build_an_agent_from_only_its_chosen_plugins() {
        let builder = AgentBuilder::new("system");
        assert!(builder.build().is_err(), "a provider is the only required plugin");
        let mut builder = AgentBuilder::new("system");
        builder.plugin(ModelPlugin).unwrap();
        assert!(builder.build().is_ok());
    }

    #[test]
    fn tool_filters_validate_kernel_names_too() {
        let mut builder = AgentBuilder::new("system");
        builder.plugin(ModelPlugin).unwrap();
        builder.allow_tools(&["plan".into()]).unwrap();
        assert!(builder.build().is_ok());

        let mut builder = AgentBuilder::new("system");
        builder.plugin(ModelPlugin).unwrap();
        builder.allow_tools(&["raed".into()]).unwrap();
        let error = match builder.build() {
            Ok(_) => panic!("an unknown tool was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("raed"));
    }

    #[test]
    fn one_shot_agents_cannot_offer_questions_but_interactive_agents_can() {
        for interactive in [false, true] {
            let mut builder = AgentBuilder::new("system").interactive(interactive);
            builder.plugin(ModelPlugin).unwrap();
            builder.allow_tools(&["ask_user".into()]).unwrap();
            let result = builder.build();
            assert_eq!(result.is_ok(), interactive);
        }
        let mut builder = AgentBuilder::new("system").interactive(false);
        builder.plugin(ModelPlugin).unwrap();
        builder.allow_tools(&["plan".into()]).unwrap();
        assert!(builder.build().is_ok(), "only interaction is disabled");
    }
}
