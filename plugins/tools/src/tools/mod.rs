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
    /// Whether a picture fits in what one tool result may add.
    pictures_fit: bool,
    /// Whether the model has asked to see an image. Nothing says which models
    /// accept images, and one that does not fails the whole request, so a
    /// picture is only shown unasked to a model that has already asked.
    viewed_image: bool,
    /// The runs this set started, and whether they end with it. A person at
    /// a terminal leaves a training job running on purpose; a client that
    /// closes a session — or goes away — leaves nobody to stop what the
    /// session started, and its runs went on for hours after the client had
    /// finished.
    runs: Vec<String>,
    runs_end_with_it: bool,
}

impl Drop for ToolSet {
    fn drop(&mut self) {
        if self.runs_end_with_it {
            runs::end(self.workspace.root(), &self.runs);
        }
    }
}

impl ToolSet {
    pub fn discover(cwd: PathBuf) -> Result<Self> {
        Ok(Self {
            workspace: Workspace::new(cwd.canonicalize()?)?,
            definitions: definitions(),
            pictures_fit: true,
            viewed_image: false,
            runs: Vec::new(),
            runs_end_with_it: false,
        })
    }

    /// End the commands this set started, still running or not, when it is
    /// dropped: for a frontend whose sessions belong to a client.
    pub fn end_runs_with_it(&mut self) {
        self.runs_end_with_it = true;
    }

    /// Size what a command's result may carry to the agent's compaction
    /// threshold, the way the agent sizes every result.
    pub fn fit_to(&mut self, compact_chars: usize) {
        self.pictures_fit = elmee_core::output_ceiling(compact_chars) >= elmee_core::session::IMAGE_CHARS;
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
                    self.viewed_image = true;
                    let mut outcome = elmee_core::ToolOutcome::new("Image attached for inspection", true);
                    outcome.images.push(image);
                    outcome
                }
                Err(error) => elmee_core::ToolOutcome::new(format!("Error: {error}"), false),
            };
        }
        // How it went is reported, not inferred: a consumer that has to read
        // the output to find out is one string away from being wrong.
        let began = std::time::SystemTime::now();
        match self.try_execute(call).await {
            Ok(output) => {
                let mut outcome = elmee_core::ToolOutcome::new(truncate(output), true);
                // A screenshot command used to cost a second turn: run it,
                // then view_image the path it printed.
                if call.function.name == "exec"
                    && self.pictures_fit
                    && self.viewed_image
                    && let Some((path, image)) = images::drawn(&outcome.output, self.workspace.root(), began)
                {
                    outcome.output.push_str(&format!("\n[{path} is attached for inspection]"));
                    outcome.images.push(image);
                }
                outcome
            }
            Err(error) => elmee_core::ToolOutcome::new(truncate(format!("Error: {error}")), false),
        }
    }

    /// One line per tool, so the set an agent has is a list rather than a
    /// hundred and seventy lines to read through. What each one means lives
    /// with its family; what is left here is the registry and the dispatch.
    async fn try_execute(&mut self, call: &ToolCall) -> Result<String> {
        let arguments = &call.function.arguments;
        match call.function.name.as_str() {
            "read" => files::read(&mut self.workspace, &parse(arguments)?),
            "apply_patch" => self.patch(parse(arguments)?).await,
            "exec" => shell::exec(self.workspace.root(), &parse(arguments)?, &mut self.runs).await,
            "write_run" => shell::feed(self.workspace.root(), &parse(arguments)?).await,
            "stop_run" => shell::stop(self.workspace.root(), &parse(arguments)?),
            name => Err(Error::Tool(format!("unknown tool: {name}"))),
        }
    }
}

impl ToolSet {
    /// Apply a patch, then run the check it carried.
    ///
    /// An edit and the command that shows it worked were two model requests
    /// for one thought: measured, a quarter of a small fix. The patch stands
    /// whatever the check says, so a check that could not start is reported
    /// beside it, not instead of it.
    async fn patch(&mut self, args: schema::PatchArgs) -> Result<String> {
        let applied = patch::apply(&mut self.workspace, &args)?;
        let Some(command) = args.check else { return Ok(applied) };
        let check = schema::ExecArgs { command, yield_ms: None, max_output_bytes: None };
        let output = shell::exec(self.workspace.root(), &check, &mut self.runs)
            .await
            .unwrap_or_else(|error| format!("[the check did not run: {error}]"));
        Ok(format!("{applied}\n$ {}\n{output}", check.command))
    }
}

/// Read a tool's own argument shape out of what the model sent.
fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T> {
    Ok(serde_json::from_str(arguments)?)
}

/// Cap what is about to enter model context. The same bounding `text` does,
/// at the size a tool result is allowed to be. Command output arrives already
/// bounded below this, with both its ends kept.
pub fn truncate(value: String) -> String {
    elmee_core::text::bound(value, MAX_OUTPUT, true)
}

#[cfg(test)]
mod tests;
