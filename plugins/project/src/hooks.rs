use std::{ffi::OsString, fs, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{Error, Result};
use elmee_core::message::ToolCall;

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// The room a hook that does real work needs, as against the thirty seconds
/// that suit one standing in front of every tool call.
///
/// A completion check is usually a build or a test suite: without this the
/// documented example — `cargo test` — could not pass on any project past a
/// toy, and the gate it was supposed to be would silently be no gate at all.
/// A session hook is the same shape of thing — it may read a whole transcript,
/// or spend a model call on it — and being killed at thirty seconds would look
/// exactly like having done nothing.
fn work_timeout() -> Duration {
    let seconds = std::env::var("ELMEE_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(600);
    Duration::from_secs(seconds)
}

pub struct Hooks {
    cwd: PathBuf,
    before_tool_call: Vec<Hook>,
    after_tool_call: Vec<Hook>,
    before_completion: Vec<Hook>,
    after_session: Vec<Hook>,
}

impl Hooks {
    pub fn discover(cwd: PathBuf) -> Result<Self> {
        let path = cwd.join(".elmee/hooks.json");
        let config = if path.is_file() {
            serde_json::from_slice::<HookConfig>(&fs::read(path)?)?
        } else {
            HookConfig::default()
        };
        Ok(Self {
            cwd,
            before_tool_call: config.before_tool_call,
            after_tool_call: config.after_tool_call,
            before_completion: config.before_completion,
            after_session: config.after_session,
        })
    }

    pub async fn before_tool_call(&self, call: &ToolCall) -> Result<()> {
        self.run(&self.before_tool_call, &HookEvent::BeforeToolCall { call }).await
    }

    pub async fn after_tool_call(&self, call: &ToolCall, result: &str) -> Result<()> {
        self.run(&self.after_tool_call, &HookEvent::AfterToolCall { call, result }).await
    }

    /// Gate on the agent declaring itself done. A failing hook returns its
    /// output to the model as work still to do, which is how a project makes
    /// "the tests must pass" a rule rather than a hope.
    pub async fn before_completion(&self, answer: &str) -> Result<()> {
        self.run_bounded(&self.before_completion, &HookEvent::BeforeCompletion { answer }, work_timeout())
            .await
    }

    /// Run when a session is over: the editor closed it, the connection ended,
    /// or the process finished its work.
    ///
    /// Nothing here can be refused. The other three hooks gate something that
    /// has not happened yet; this one is told about something already done, so
    /// a failure is reported and nothing more — an exit code that could hold a
    /// process open would be a gate on leaving, which is not a thing to want.
    ///
    /// The payload names the transcript rather than summarising it. Whatever a
    /// distillation wants to know — how many turns, what was edited, what it
    /// cost — is in that file already, and a second, thinner account of it
    /// could only go stale.
    pub async fn after_session(&self, session: Option<&std::path::Path>) -> Result<()> {
        let session = session.map(|path| path.display().to_string());
        let cwd = self.cwd.display().to_string();
        self.run_bounded(&self.after_session, &HookEvent::AfterSession { session, cwd }, work_timeout()).await
    }

    /// The project's completion checks, each with the level it proves.
    pub fn completion_checks(&self) -> Vec<elmee_core::CommandEvidence> {
        self.before_completion
            .iter()
            .map(|hook| {
                let command = std::iter::once(hook.command.clone())
                    .chain(hook.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ");
                let level = hook
                    .verifies
                    .or_else(|| elmee_plugin_process::verification_level(&command))
                    .unwrap_or(elmee_core::evidence::VerificationLevel::Unverified);
                elmee_core::CommandEvidence { command, level }
            })
            .collect()
    }

    async fn run(&self, hooks: &[Hook], event: &HookEvent<'_>) -> Result<()> {
        self.run_bounded(hooks, event, HOOK_TIMEOUT).await
    }

    async fn run_bounded(&self, hooks: &[Hook], event: &HookEvent<'_>, limit: Duration) -> Result<()> {
        if hooks.is_empty() {
            return Ok(());
        }
        let input = serde_json::to_vec(event)?;
        for hook in hooks {
            let program: OsString = if hook.command.contains('/') {
                self.cwd.join(&hook.command).into_os_string()
            } else {
                hook.command.clone().into()
            };
            let mut command = Command::new(program);
            elmee_plugin_process::env::apply(command.as_std_mut());
            command.args(&hook.args).current_dir(&self.cwd);
            let output =
                elmee_plugin_process::feed(command, &input, limit, &format!("hook {}", hook.command)).await?;
            if !output.status.success() {
                return Err(Error::Tool(elmee_core::text::bound(
                    format!("hook {} blocked event: {}{}", hook.command, output.stdout, output.stderr),
                    50 * 1024,
                    true,
                )));
            }
        }
        Ok(())
    }
}

impl elmee_core::Middleware for Hooks {
    fn phase(&self) -> elmee_core::MiddlewarePhase {
        elmee_core::MiddlewarePhase::Policy
    }

    fn on<'a>(
        &'a mut self,
        event: elmee_core::MiddlewareEvent<'a>,
    ) -> elmee_core::PluginFuture<'a, Result<Vec<elmee_core::MiddlewareEffect>>> {
        Box::pin(async move {
            match event {
                elmee_core::MiddlewareEvent::BeforeTool { call } => {
                    Hooks::before_tool_call(self, call).await?
                }
                elmee_core::MiddlewareEvent::AfterTool { call, output, .. } => {
                    Hooks::after_tool_call(self, call, output).await?
                }
                elmee_core::MiddlewareEvent::BeforeFinish { answer } => {
                    Hooks::before_completion(self, answer).await?;
                    return Ok(self
                        .completion_checks()
                        .into_iter()
                        .map(elmee_core::MiddlewareEffect::RecordCommandEvidence)
                        .collect());
                }
                _ => {}
            }
            Ok(Vec::new())
        })
    }
}

impl elmee_core::Plugin for Hooks {
    fn id(&self) -> &'static str {
        "project-hooks"
    }

    fn declare(self, plugins: &mut elmee_core::Registrar) -> Result<()> {
        plugins.provide_middleware(self)
    }
}

/// Unknown keys are refused rather than ignored. A hook file is the one place
/// where silence is the worst answer: a misspelt or outdated name would leave
/// the project believing it has a gate it does not have, and nothing would ever
/// say so.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookConfig {
    #[serde(default)]
    before_tool_call: Vec<Hook>,
    #[serde(default)]
    after_tool_call: Vec<Hook>,
    #[serde(default)]
    before_completion: Vec<Hook>,
    #[serde(default)]
    after_session: Vec<Hook>,
}

#[derive(Deserialize)]
struct Hook {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// What a `before_completion` check proves when it passes: `static`,
    /// `integration` or `e2e`. Said by the project, because only the project
    /// knows what its check does; when it is not said, the level is read off
    /// the command's own words.
    #[serde(default)]
    verifies: Option<elmee_core::evidence::VerificationLevel>,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum HookEvent<'a> {
    BeforeToolCall {
        call: &'a ToolCall,
    },
    AfterToolCall {
        call: &'a ToolCall,
        result: &'a str,
    },
    BeforeCompletion {
        answer: &'a str,
    },
    /// `session` is `None` for a run that persisted nothing; the hook is still
    /// told, because deciding that there is nothing to do with it belongs to
    /// the hook and not to us.
    AfterSession {
        session: Option<String>,
        cwd: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one hook that cannot refuse anything still has to run, and it has
    /// to carry the transcript's path — that is the whole payload a
    /// distillation needs, and the only thing a script cannot work out for
    /// itself once the process is gone.
    #[tokio::test]
    async fn a_session_hook_runs_and_is_told_where_the_transcript_is() {
        let root = std::env::temp_dir().join(format!("elmee-session-hook-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".elmee")).unwrap();
        let seen = root.join("seen.json");
        fs::write(
            root.join(".elmee/hooks.json"),
            format!(r#"{{"after_session":[{{"command":"sh","args":["-c","cat > {}"]}}]}}"#, seen.display()),
        )
        .unwrap();
        let hooks = Hooks::discover(root.clone()).unwrap();

        hooks.after_session(Some(&root.join("run.jsonl"))).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&fs::read(&seen).unwrap()).unwrap();
        assert_eq!(payload["event"], "after_session");
        assert!(payload["session"].as_str().unwrap().ends_with("run.jsonl"), "{payload}");

        // A run that persisted nothing still reports; whether that is worth
        // acting on is the hook's judgement, not ours.
        hooks.after_session(None).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&fs::read(&seen).unwrap()).unwrap();
        assert!(payload["session"].is_null(), "{payload}");
        fs::remove_dir_all(root).unwrap();
    }

    /// A name that is not a hook is a project believing in a gate it does not
    /// have. Refused loudly rather than ignored.
    #[tokio::test]
    async fn an_unknown_hook_name_is_refused() {
        let root = std::env::temp_dir().join(format!("elmee-hook-name-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".elmee")).unwrap();
        fs::write(root.join(".elmee/hooks.json"), r#"{"before_finish":[{"command":"true"}]}"#).unwrap();

        let refused = match Hooks::discover(root.clone()) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("an unknown hook name was accepted"),
        };
        assert!(refused.contains("before_finish"), "{refused}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failing_hook_blocks_tool_event() {
        let root = std::env::temp_dir().join(format!("elmee-hook-test-{}", std::process::id()));
        fs::create_dir_all(root.join(".elmee")).unwrap();
        fs::write(
            root.join(".elmee/hooks.json"),
            r#"{"before_tool_call":[{"command":"sh","args":["-c","echo denied; exit 1"]}]}"#,
        )
        .unwrap();
        let hooks = Hooks::discover(root.clone()).unwrap();
        let call = ToolCall::new("1", "exec", r#"{"command":"true"}"#);

        assert!(hooks.before_tool_call(&call).await.is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
