//! Plugins supplied by the local coding toolkit.

use std::path::{Path, PathBuf};

use elmee_core::{Plugin, PluginFuture, Registrar, Result, ToolOutcome, message::ToolCall};
use elmee_plugin_snapshot::Snapshots;

use crate::tools::ToolSet;

impl elmee_core::Handler for ToolSet {
    fn definitions(&self) -> &[serde_json::Value] {
        ToolSet::definitions(self)
    }

    fn execute<'a>(&'a mut self, call: &'a ToolCall) -> PluginFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let mut outcome = self.execute_outcome(call).await;
            outcome.evidence =
                evidence_command(call, &outcome.output).map(|command| elmee_core::CommandEvidence {
                    level: elmee_plugin_process::verification_level(&command)
                        .unwrap_or(elmee_core::evidence::VerificationLevel::Unverified),
                    command,
                });
            outcome
        })
    }
}

fn evidence_command(call: &ToolCall, output: &str) -> Option<String> {
    // Evidence is a command that finished here and finished well: one still
    // going, or one nobody waited for, has proved nothing yet.
    (call.function.name == "exec"
        && !output.contains("[exit status:")
        && !output.starts_with("Started run ")
        && !output.contains("is still going"))
    .then(|| serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok())
    .flatten()
    .and_then(|args| args["command"].as_str().map(str::to_owned))
}

impl Plugin for ToolSet {
    fn id(&self) -> &'static str {
        "tools"
    }

    fn declare(self, plugins: &mut Registrar) -> Result<()> {
        plugins.provide_handler(self)?;
        plugins.provide_middleware(crate::changed::ChangedFiles)
    }
}

/// Workspace identity and attachment discovery. It is separate from tools so
/// a non-coding agent can expose another world without inheriting file tools.
pub struct WorkspacePlugin {
    cwd: PathBuf,
    snapshots: Option<Snapshots>,
    roots: Vec<PathBuf>,
}

impl WorkspacePlugin {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        Self { cwd, snapshots: None, roots: Vec::new() }
    }

    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        if self.snapshots.is_some() {
            self.snapshots = Some(Snapshots::new(&self.cwd).with_roots(self.roots.clone()));
        }
        self
    }

    /// Use the coding application's snapshot store as the workspace content
    /// identity. Other products do not create a hidden Git store merely by
    /// installing a filesystem capability.
    pub fn with_snapshots(mut self) -> Self {
        self.snapshots = Some(Snapshots::new(&self.cwd).with_roots(self.roots.clone()));
        self
    }
}

impl elmee_core::Workspace for WorkspacePlugin {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn attachments(&self, prompt: &str) -> Vec<elmee_core::message::Image> {
        prompt
            .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .filter(|word| word.contains('.'))
            .filter_map(|word| {
                let word = word.trim_start_matches('@').trim_end_matches(['.', ',', ';', ':', '!', '?']);
                self.cwd.join(word).canonicalize().ok().filter(|path| {
                    path.starts_with(&self.cwd) || self.roots.iter().any(|root| path.starts_with(root))
                })
            })
            .filter(|path| {
                path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
                    matches!(extension.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
                })
            })
            .filter_map(|path| crate::tools::images::load(&path).ok())
            .take(4)
            .collect()
    }

    fn fingerprint(&self) -> PluginFuture<'_, Option<String>> {
        Box::pin(async move {
            match &self.snapshots {
                Some(snapshots) => snapshots.fingerprint().await,
                None => None,
            }
        })
    }
}

impl Plugin for WorkspacePlugin {
    fn id(&self) -> &'static str {
        if self.snapshots.is_some() { "snapshot-workspace" } else { "workspace" }
    }

    fn declare(self, plugins: &mut Registrar) -> Result<()> {
        plugins.provide_service::<dyn elmee_core::Workspace>(std::rc::Rc::new(self));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_attachments_can_come_from_an_explicit_additional_root() {
        use elmee_core::Workspace;
        let root = std::env::temp_dir().join(elmee_core::unique("workspace-roots"));
        let (main, extra) = (root.join("main"), root.join("extra"));
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        let (main, extra) = (main.canonicalize().unwrap(), extra.canonicalize().unwrap());
        let image = extra.join("image.gif");
        std::fs::write(
            &image,
            elmee_core::text::unbase64("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==").unwrap(),
        )
        .unwrap();
        let prompt = image.to_string_lossy();
        assert!(WorkspacePlugin::new(&main).attachments(&prompt).is_empty());
        assert_eq!(WorkspacePlugin::new(main).with_roots(vec![extra]).attachments(&prompt).len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
