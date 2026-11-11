//! Everything a frontend may ask of a session besides a turn: the transcript
//! it is writing, the ones already on disk, the working tree's checkpoints and
//! the commands its plugins own.
//!
//! Gathered here so a frontend depends on the host and on nothing beneath it.
//! When each one opened transcripts and looked up snapshots for itself, what a
//! session could do was whatever its frontend happened to link.

use std::path::{Path, PathBuf};

use elmee_core::{CommandOutcome, Result};
pub use elmee_plugin_session::Summary;

use crate::Driver;

/// The newest `limit` conversations kept for `cwd`, each named by the first
/// thing asked of it.
pub fn past(cwd: &Path, limit: usize) -> Result<Vec<Summary>> {
    let paths = elmee_plugin_session::in_directory(&elmee_plugin_paths::session_directory(cwd))?;
    Ok(elmee_plugin_session::summarize(paths.into_iter().take(limit).collect()))
}

/// Where a new conversation in `cwd` would be written.
pub fn fresh(cwd: &Path) -> Result<PathBuf> {
    elmee_plugin_session::new_path(&elmee_plugin_paths::session_directory(cwd))
}

/// Copy an active conversation into an independent store. No branch cursor or
/// writer lock is shared with its parent, and disk work stays off the frontend.
pub async fn fork_history(
    messages: Vec<elmee_core::message::Message>,
    path: Option<PathBuf>,
) -> Result<elmee_core::session::Session> {
    tokio::task::spawn_blocking(move || {
        let mut session = elmee_plugin_session::open(path)?;
        for message in messages {
            session.push(message)?;
        }
        Ok(session)
    })
    .await
    .map_err(|error| elmee_core::Error::Config(format!("forking session: {error}")))?
}

/// Name a conversation that is not open anywhere.
pub fn rename(path: &Path, title: String) -> Result<()> {
    if !path.is_file() {
        return Err(elmee_core::Error::Config(format!("no session at {}", path.display())));
    }
    elmee_plugin_session::open(Some(path.to_path_buf()))?.set_title(title)
}

/// Remove a closed conversation and everything kept beside it.
pub fn delete(path: &Path) -> Result<()> {
    elmee_plugin_session::delete_session(path).map(drop)
}

/// Every model a session here could be switched to, as far as this machine
/// knows without asking: an answer for a picker to open on at once.
pub fn known_models(cwd: &Path) -> Vec<String> {
    elmee_plugin_provider::registry::known_models(cwd).unwrap_or_default()
}

/// The same, asked of the endpoints themselves — and of no agent, because a
/// running turn owns its provider for as long as the model takes. `None` when
/// they were asked recently enough that the answer already known stands.
pub async fn fresh_models(cwd: &Path) -> Option<Result<Vec<String>>> {
    if elmee_plugin_provider::registry::catalogue_is_fresh() {
        return None;
    }
    Some(models(cwd).await)
}

/// What the endpoints serve right now, asked whether or not they were asked a
/// moment ago.
pub async fn models(cwd: &Path) -> Result<Vec<String>> {
    let registry = elmee_plugin_provider::registry::load(cwd)?;
    let provider = elmee_plugin_provider::MultiProvider::new(registry.entries, 0, String::new());
    Ok(elmee_core::provider::Provider::list_models(&provider)
        .await?
        .into_iter()
        .map(|model| model.id)
        .collect())
}

/// Long commands a project still has running, as text to show. Asked of the
/// directory and of no session, so it answers while a turn is running.
pub fn runs(cwd: &Path) -> String {
    elmee_plugin_tools::runs::summary(cwd)
}

impl Driver {
    /// Continue in another conversation: the one at `path`, or one that is
    /// kept nowhere.
    pub fn open_session(&mut self, path: Option<PathBuf>) -> Result<()> {
        self.agent.replace_session(elmee_plugin_session::open(path)?);
        if let Some(context) = self.agent.services().one::<elmee_plugin_provider::SessionContext>()? {
            context.set(self.agent.session_path());
        }
        Ok(())
    }

    /// `/name arguments`, when `name` is a command a plugin owns. Anything
    /// else is for the model, a slash included.
    pub fn command(&mut self, text: &str) -> Option<std::result::Result<CommandOutcome, String>> {
        let rest = text.strip_prefix('/')?;
        let (name, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if !self.agent.plugin_commands().iter().any(|command| command.name == name) {
            return None;
        }
        Some(self.agent.plugin_command(name, argument.trim()).map_err(|error| error.to_string()))
    }

    /// What a person may type after a slash here: the commands plugins own,
    /// then the prompt templates the project and the user keep in files.
    pub fn commands(&self) -> Vec<(String, String)> {
        let mut commands: Vec<(String, String)> = self
            .agent
            .plugin_commands()
            .iter()
            .map(|command| (command.name.to_owned(), command.description.to_owned()))
            .collect();
        commands.extend(
            elmee_plugin_command::load(self.runtime.working_directory(), Vec::new())
                .into_iter()
                .filter(|command| command.template.is_some())
                .map(|command| (command.name, command.description)),
        );
        commands
    }

    /// A prompt that names a template is the prompt that template expands to,
    /// whichever frontend it was typed into.
    pub(crate) async fn expanded(&self, prompt: String) -> String {
        let Some(rest) = prompt.strip_prefix('/') else { return prompt };
        let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let cwd = self.runtime.working_directory();
        let template = elmee_plugin_command::load(cwd, Vec::new())
            .into_iter()
            .find(|command| command.name == name)
            .and_then(|command| command.template);
        match template {
            Some(template) => elmee_plugin_command::expand(&template, arguments.trim(), cwd).await,
            None => prompt,
        }
    }

    /// Go back to before a prompt of this conversation: the conversation, the
    /// files as they were before that prompt's turn changed them, or both.
    /// Returns the prompt when the conversation moved, to be edited and sent
    /// again.
    ///
    /// Files first: that is the half that can fail, and a conversation
    /// rewound over files that stayed would describe a tree that is not there.
    pub async fn rewind(
        &mut self,
        prompt: u64,
        restore: Restore,
    ) -> std::result::Result<Option<String>, String> {
        if restore != Restore::Conversation {
            let checkpoint = self
                .agent
                .prompts()
                .into_iter()
                .find(|asked| asked.id == prompt)
                .and_then(|asked| asked.checkpoint)
                .ok_or("the files were not recorded before that prompt")?;
            let snapshots = self.snapshots.as_ref().ok_or("this agent records no checkpoints")?;
            snapshots.restore(&checkpoint).await?;
        }
        if restore == Restore::Files {
            return Ok(None);
        }
        self.agent.rewind(prompt).map(|asked| Some(asked.text)).map_err(|error| error.to_string())
    }
}

/// What a rewind puts back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Restore {
    Everything,
    Conversation,
    Files,
}
