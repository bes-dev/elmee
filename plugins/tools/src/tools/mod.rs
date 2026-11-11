use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;

mod edit;
mod files;
pub(crate) mod images;
mod schema;
mod shell;
mod workspace;

pub mod patch;
pub mod runs;

use schema::definitions;
pub use schema::tool;
pub use shell::missing_ripgrep;
use workspace::Workspace;

use crate::{Error, Result};
use elmee_core::message::ToolCall;

const MAX_OUTPUT: usize = 50 * 1024;
/// How long a tool call is waited on. For a command this is not a deadline but
/// the point at which waiting stops being useful — the work continues as a
/// background run. Adjustable because two minutes is a guess about someone
/// else's machine.
static TOOL_TIMEOUT: std::sync::LazyLock<Duration> = std::sync::LazyLock::new(|| {
    let seconds = std::env::var("ELMEE_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(120);
    Duration::from_secs(seconds)
});

pub struct ToolSet {
    workspace: Workspace,
    definitions: Vec<Value>,
}

impl ToolSet {
    pub fn discover(cwd: PathBuf) -> Result<Self> {
        Ok(Self { workspace: Workspace::new(cwd.canonicalize()?)?, definitions: definitions() })
    }

    /// Offer the host's own tools to the model alongside the built-ins. They
    /// are answered through `Interactor::host_tool`, not here.
    pub fn add_host_tools(&mut self, tools: Vec<Value>) {
        self.definitions.extend(tools);
    }

    pub fn definitions(&self) -> &[Value] {
        &self.definitions
    }

    pub fn cwd(&self) -> &Path {
        self.workspace.root()
    }

    /// Text-only compatibility surface. Use `execute_outcome` for multimodal
    /// consumers so that image content is not discarded.
    pub async fn execute(&mut self, call: &ToolCall) -> (String, bool) {
        let outcome = self.execute_outcome(call).await;
        (outcome.output, outcome.ok)
    }

    /// The full tool result, including images that cannot travel as text.
    pub async fn execute_outcome(&mut self, call: &ToolCall) -> elmee_core::ToolOutcome {
        if call.function.name == "view_image" {
            let image = parse::<schema::ImageArgs>(&call.function.arguments)
                .and_then(|args| self.workspace.resolve(&args.path))
                .and_then(|path| images::load(&path));
            return match image {
                Ok(image) => {
                    let mut outcome = elmee_core::ToolOutcome::new("Image attached for inspection", true);
                    outcome.images.push(image);
                    outcome
                }
                Err(error) => elmee_core::ToolOutcome::new(format!("Error: {error}"), false),
            };
        }
        // Command output is most informative at its tail; file output at its head.
        let keep_head = !matches!(call.function.name.as_str(), "exec" | "write_run");
        // How it went is reported, not inferred: a consumer that has to read
        // the output to find out is one string away from being wrong.
        match self.try_execute(call).await {
            Ok(output) => elmee_core::ToolOutcome::new(truncate(output, keep_head), true),
            Err(error) => elmee_core::ToolOutcome::new(truncate(format!("Error: {error}"), true), false),
        }
    }

    /// One line per tool, so the set an agent has is a list rather than a
    /// hundred and seventy lines to read through. What each one means lives
    /// with its family; what is left here is the registry and the dispatch.
    async fn try_execute(&mut self, call: &ToolCall) -> Result<String> {
        let arguments = &call.function.arguments;
        match call.function.name.as_str() {
            "read" => files::read(&mut self.workspace, &parse(arguments)?),
            "apply_patch" => patch::apply(&mut self.workspace, &parse(arguments)?),
            "exec" => shell::exec(self.workspace.root(), &parse(arguments)?).await,
            "write_run" => shell::feed(self.workspace.root(), &parse(arguments)?).await,
            "stop_run" => shell::stop(self.workspace.root(), &parse(arguments)?),
            name => Err(Error::Tool(format!("unknown tool: {name}"))),
        }
    }
}

/// Read a tool's own argument shape out of what the model sent.
fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T> {
    Ok(serde_json::from_str(arguments)?)
}

/// Cap what is about to enter model context. The same bounding `text` does,
/// at the size a tool result is allowed to be.
pub fn truncate(value: String, keep_head: bool) -> String {
    elmee_core::text::bound(value, MAX_OUTPUT, keep_head)
}

#[cfg(test)]
mod tests;
