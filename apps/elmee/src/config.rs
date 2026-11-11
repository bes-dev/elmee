//! The command line and the environment, turned into what one run was asked
//! to be. Application policy, which is why it lives in the application: an
//! embedder composes agents from its own configuration and never sees this.

use std::{env, path::PathBuf};

use elmee_core::{DEFAULT_COMPACT_CHARS, Error, Result, evidence::VerificationLevel};

#[derive(Clone)]
pub struct Config {
    /// The -m/ELMEE_MODEL spec; the frontend falls back to the registry's
    /// `defaultModel` when absent.
    pub model: Option<String>,
    /// OpenAI reasoning depth; `None` leaves the model's own default in place.
    pub reasoning_effort: Option<elmee_core::provider::ReasoningEffort>,
    pub max_steps: usize,
    /// Total tokens a run may spend, 0 for no limit. The one guard that makes
    /// explicitly budgeted unattended operation safe to leave running.
    pub max_tokens: u64,
    /// Append-only usage ledger shared by the root and every child process.
    /// Internal CLI plumbing; users normally only set `--max-billed-tokens`.
    pub token_ledger: Option<PathBuf>,
    pub compact_chars: usize,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub session: Option<PathBuf>,
    pub prompt: Option<String>,
    pub json: bool,
    /// Serve the Agent Client Protocol on stdio: the embedding surface.
    pub acp: bool,
    /// MCP servers the caller handed over for this run, beyond what the
    /// machine declares in its own files. Empty for every ordinary start; an
    /// editor opening a session is the thing that fills it.
    pub mcp_servers: Vec<elmee_plugin_mcp::Declared>,
    /// Standing objective to work toward, for runs with no interactive user.
    pub goal: Option<String>,
    /// The name asked for in the project mesh.
    pub name: Option<String>,
    /// Non-zero for spawned agents; prevents accidental recursive fan-out.
    pub agent_depth: usize,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    /// Which of the project's skills this agent is offered; `None` is all of
    /// them, which is what every agent got before roles existed.
    pub skills: Option<Vec<String>>,
    /// Which tools it is offered. This
    /// is about attention — a reviewer carrying eleven schemas it will never
    /// call pays for them on every request.
    pub tools: Option<Vec<String>>,
    /// How much proof a finish needs before the loop accepts it.
    pub require_verification: VerificationLevel,
}

/// Interactive sessions follow mature harness semantics: context compacts,
/// while spending is bounded only when the operator explicitly asks for it.
pub const DEFAULT_MAX_TOKENS: u64 = 0;

/// A runaway guard, not a working limit.
///
/// What actually bounds a turn is the context window — compaction, and the
/// agent's own judgement about being finished — and, when one is set, the token
/// budget. Counting tool calls measures none of that: sixty-four edits to one
/// file and sixty-four test-suite runs are nothing alike, and the old default of
/// 64 cut real work in half mid-refactor, leaving a tree that did not build.
/// This is set where only a loop that has stopped making progress can reach it.
pub const DEFAULT_MAX_STEPS: usize = 1000;

impl Config {
    pub fn from_env_args() -> Result<Self> {
        // A role is expanded into ordinary flags here and nowhere else: after
        // this line nothing in elmee knows roles exist. `--cwd` is read first
        // because it decides which project's roles these are.
        let argv: Vec<String> = env::args().skip(1).collect();
        let at = argv.iter().position(|arg| arg == "--cwd").and_then(|at| argv.get(at + 1));
        let here = at.map_or_else(|| env::current_dir().unwrap_or_default(), PathBuf::from);
        let mut args = elmee_plugin_project::roles::expand(&here, argv)?.into_iter();
        let mut model = env::var("ELMEE_MODEL").ok();
        let mut reasoning_effort = match env::var("ELMEE_REASONING_EFFORT") {
            Ok(value) => parse_reasoning_effort(&value)?,
            Err(_) => None,
        };
        let mut max_steps = DEFAULT_MAX_STEPS;
        let mut max_tokens = DEFAULT_MAX_TOKENS;
        let mut token_ledger = None;
        let mut compact_chars = DEFAULT_COMPACT_CHARS;
        let mut session = None;
        let mut persist = true;
        let mut resume = false;
        let mut prompt = Vec::new();
        let mut json = false;
        let mut acp = false;
        let mut goal = None;
        let mut name = None;
        let mut agent_depth = 0;
        let mut cwd_override: Option<PathBuf> = None;
        let mut additional_dirs = Vec::new();
        let mut system_prompt = None;
        let mut append_system_prompt: Option<String> = None;
        let mut skills = None;
        let mut tools = None;
        let mut require_verification = VerificationLevel::Unverified;
        let mut explain = false;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--model" | "-m" => model = Some(next_value(&mut args, "--model")?),
                "--reasoning-effort" => {
                    reasoning_effort = parse_reasoning_effort(&next_value(&mut args, "--reasoning-effort")?)?;
                }
                "--max-steps" => max_steps = next_number(&mut args, "--max-steps")?,
                "--max-billed-tokens" => {
                    max_tokens = next_number(&mut args, "--max-billed-tokens")? as u64;
                }
                "--token-ledger" => token_ledger = Some(next_value(&mut args, "--token-ledger")?.into()),
                "--compact-chars" => compact_chars = next_number(&mut args, "--compact-chars")?,
                "--session" => session = Some(next_value(&mut args, "--session")?.into()),
                "--continue" | "-c" => resume = true,
                "--no-session" => persist = false,
                "--json" => json = true,
                "--acp" => acp = true,
                // Consumed by the stock application after assembly. Keeping
                // it out of Config prevents composition diagnostics becoming
                // agent-loop configuration.
                "--explain-plugins" => explain = true,
                "--goal" => goal = Some(next_value(&mut args, "--goal")?),
                "--name" => name = Some(next_value(&mut args, "--name")?),
                "--agent-depth" => agent_depth = next_number(&mut args, "--agent-depth")?,
                "--add-dir" => additional_dirs.push(PathBuf::from(next_value(&mut args, "--add-dir")?)),
                "--cwd" => cwd_override = Some(next_value(&mut args, "--cwd")?.into()),
                "--system-prompt" => system_prompt = Some(next_value(&mut args, "--system-prompt")?),
                "--append-system-prompt" => {
                    let extra = next_value(&mut args, "--append-system-prompt")?;
                    append_system_prompt = Some(match append_system_prompt.take() {
                        Some(already) => format!("{already}\n\n{extra}"),
                        None => extra,
                    });
                }
                "--skills" => skills = Some(names(&next_value(&mut args, "--skills")?)),
                "--tools" => tools = Some(names(&next_value(&mut args, "--tools")?)),
                "--require-verification" => {
                    require_verification =
                        VerificationLevel::parse(&next_value(&mut args, "--require-verification")?)?;
                }
                "-" => prompt.push("-".to_owned()),
                value if value.starts_with('-') => {
                    return Err(Error::Config(format!("unknown option: {value}\n\n{HELP}")));
                }
                value => prompt.push(value.to_owned()),
            }
        }

        if goal.is_some()
            && tools
                .as_ref()
                .is_some_and(|tools| !tools.iter().any(|tool| tool == elmee_plugin_goal::DONE_TOOL))
        {
            return Err(Error::Config(
                "--goal requires goal_done in --tools; add it or remove --goal".into(),
            ));
        }

        let cwd = match cwd_override {
            Some(path) => path.canonicalize().map_err(|error| {
                Error::Config(format!("--cwd {} is not accessible: {error}", path.display()))
            })?,
            None => env::current_dir()?,
        };
        if !persist || explain {
            session = None;
        } else if session.is_none() {
            let directory = elmee_plugin_paths::session_directory(&cwd);
            session = Some(if resume {
                elmee_plugin_session::in_directory(&directory)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::Config(format!("no session found in {}", directory.display())))?
            } else {
                elmee_plugin_session::new_path(&directory)?
            });
        }

        for directory in &mut additional_dirs {
            *directory =
                directory.canonicalize().map_err(|error| Error::Config(format!("--add-dir: {error}")))?;
            if !directory.is_dir() {
                return Err(Error::Config("--add-dir must name a directory".into()));
            }
        }
        additional_dirs.sort();
        additional_dirs.dedup();
        Ok(Self {
            model,
            reasoning_effort,
            max_steps,
            max_tokens,
            token_ledger,
            compact_chars,
            cwd,
            additional_dirs,
            session,
            prompt: (!prompt.is_empty()).then(|| prompt.join(" ")),
            json,
            acp,
            mcp_servers: Vec::new(),
            goal,
            name,
            agent_depth,
            system_prompt,
            append_system_prompt,
            skills,
            tools,
            require_verification,
        })
    }
}

fn names(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_owned).collect()
}

fn next_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next().ok_or_else(|| Error::Config(format!("missing value for {name}")))
}

fn next_number(args: &mut impl Iterator<Item = String>, name: &str) -> Result<usize> {
    next_value(args, name)?.parse().map_err(|_| Error::Config(format!("{name} must be an integer")))
}

fn parse_reasoning_effort(value: &str) -> Result<Option<elmee_core::provider::ReasoningEffort>> {
    if value == "auto" {
        return Ok(None);
    }
    elmee_core::provider::ReasoningEffort::parse(value).map(Some).ok_or_else(|| {
        Error::Config(format!(
            "--reasoning-effort must be auto, {}",
            elmee_core::provider::ReasoningEffort::VALUES.join(", ")
        ))
    })
}

// A plain literal, flush against the left margin: an escaped newline would
// swallow the leading whitespace that aligns the option descriptions.
pub const HELP: &str = "\
Usage: elmee --model MODEL [options] [PROMPT]

Without PROMPT, elmee starts the interactive TUI. With PROMPT it runs
headless: the final answer goes to stdout. PROMPT of - reads stdin.

Commands:
  login [codex]                Sign in to Codex, which a ChatGPT
                               subscription pays for (the default)
  logout [codex]               Forget the Codex sign-in
  notify [--to NAME] TEXT      Deliver an event to agents running in this
                               directory (--stdin adds an untrusted payload)

Options:
  -m, --model MODEL            provider/model spec against models.json (bare
                               names hit the default endpoint; claude-* →
                               anthropic; falls back to defaultModel, then to
                               whichever model was used last)
  --reasoning-effort LEVEL     OpenAI reasoning depth: auto, none, minimal,
                               low, medium, high, or xhigh (default: auto)
  --list-models                Print every available model spec and exit
  --max-steps N                Runaway guard on model/tool turns
                               (default: 1000; 0 removes it)
  --max-billed-tokens N        Stop starting model requests once root + child
                               spend reaches N; the request that crosses it
                               is paid in full (default: unlimited)
  --compact-chars N            Compact above N context chars (default: derived
                               from the model's declared contextWindow, else
                               400000)
  --session PATH               Persist and resume a JSONL session
  -c, --continue               Resume the newest session for this directory
  --no-session                 Do not persist the session
  --add-dir DIR                Additional workspace directory (repeatable)
  --cwd DIR                    Run in DIR instead of the current directory
  --json                       Headless: emit JSONL agent events, not text
  --acp                        Serve the Agent Client Protocol on stdio, so an
                               editor can drive this process; one warm process
                               serves every session it opens
  --explain-plugins            Resolve the graph, print its stable manifest,
                               and exit before running a frontend
  --goal TEXT                  Keep working toward TEXT across turns
  --name NAME                  Project-visible name for peer messaging
  --role NAME                  Start as a role declared in .elmee/roles: its
                               instructions, skills and tools
  --skills NAMES               Offer only these skills (comma separated)
  --tools NAMES                Offer only these tools (comma separated)
  --system-prompt TEXT         Replace the built-in system prompt
  --append-system-prompt TEXT  Append TEXT to the system prompt
  --require-verification LEVEL Refuse a finish the project's own completion
                               check has not proven: static | integration |
                               e2e | external (default: unverified)

Provider transports and credentials are declared in ~/.elmee/models.json.
A provider that lists models offers those; one that lists none is asked what it
serves, so a local runtime stays current and a gateway with hundreds does not
bury the few that matter. Set \"discover\": true or false on a provider to say so
outright. Project .elmee/models.json files may only select a default and add
model IDs or contextWindow metadata for machine-declared providers; built-in
defaults cover Ollama, Anthropic API keys, and subscription-backed Codex.

Environment:
  ELMEE_MODEL                   model when -m/--model is omitted
  ELMEE_REASONING_EFFORT        reasoning depth when the flag is omitted
  ELMEE_TOOL_TIMEOUT_SECS       foreground command timeout in seconds
                                (default 120; finish hooks 600)
  ELMEE_MCP_EXIT_GRACE_MS       how long a hung-up MCP server gets to exit
                                before it is killed (default: 500)
  ELMEE_AGENT_IDLE_SECS         stop a background agent silent this long
                                (default 600; 0 disables the watchdog)
  ELMEE_AGENT_MAX_SECS          wall-clock ceiling on a background agent
                                (default 0: none)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_accepts_openai_levels_and_auto() {
        assert_eq!(parse_reasoning_effort("auto").unwrap(), None);
        assert_eq!(
            parse_reasoning_effort("high").unwrap(),
            Some(elmee_core::provider::ReasoningEffort::High)
        );
        assert!(parse_reasoning_effort("maximum").is_err());
    }
}
