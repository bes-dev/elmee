#![forbid(unsafe_code)]

//! Composition root for the installable coding agent.

mod command;
mod config;
#[cfg(feature = "agents")]
mod notify;

use config::Config;
use elmee_core::{AgentBuilder, Result};
use elmee_host::{Driver, Runtime, RuntimeEvent, host_tools};
use elmee_plugin_project::{context, hooks::Hooks};
use elmee_plugin_provider::{MultiProvider, registry};
use elmee_plugin_tools::tools::ToolSet;

#[tokio::main(flavor = "current_thread")]
pub async fn run() -> i32 {
    if std::env::args_os().skip(1).any(|arg| arg == "--version" || arg == "-V") {
        println!("elmee {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    if std::env::args_os().skip(1).any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", config::HELP);
        return 0;
    }
    let local = tokio::task::LocalSet::new();
    let code = local
        .run_until(async {
            match start().await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("elmee: {error}");
                    1
                }
            }
        })
        .await;
    // The TUI worker task still owns the agent — and through it the MCP
    // children. `process::exit` here skipped every destructor
    // and orphaned them. Draining the local set lets those tasks end and drop
    // what they hold; bounded, so one stuck child cannot hold the exit open —
    // on expiry the set is dropped instead, which still runs the destructors.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), local).await;
    code
}

/// What elmee was asked to do, in the order the answer is decided.
///
/// A subcommand answers and exits. Everything else is a run: assemble an agent
/// out of the command line and the machine's configuration, then hand it to a
/// frontend, which decides only how long to stay.
async fn start() -> Result<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(answer) = command::dispatch(&argv).await {
        return answer;
    }
    let config = Config::from_env_args()?;
    // ACP is served before anything is assembled. An editor asking whether this
    // binary speaks its protocol must not wait for a plugin graph, and the
    // agent it will ask for is built behind the handshake instead.
    if config.acp {
        return serve_acp(config).await;
    }
    launch(assemble_with(config).await?).await
}

/// The embedding surface: one warm process, one agent per session the editor
/// opens, each composed exactly the way a terminal session would have been.
async fn serve_acp(config: Config) -> Result<i32> {
    let cwd = config.cwd.clone();
    let prepared = std::cell::Cell::new(false);
    let acp = elmee_frontend_acp::Acp::new(Box::new(move |request| {
        let mut config = config.clone();
        config.cwd = request.cwd;
        config.additional_dirs.extend(request.additional_dirs);
        config.additional_dirs.sort();
        config.additional_dirs.dedup();
        match request.system_prompt {
            Some(given) if given.replace => config.system_prompt = Some(given.text),
            Some(given) => {
                config.append_system_prompt = Some(
                    config
                        .append_system_prompt
                        .into_iter()
                        .chain([given.text])
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                );
            }
            None => {}
        }
        if let Some((model, effort)) = request.settings {
            config.model = Some(model);
            config.reasoning_effort = effort;
        }
        if prepared.replace(true) && request.session.is_none() && config.session.is_some() {
            config.session = Some(match elmee_host::session::fresh(&config.cwd) {
                Ok(path) => path,
                Err(error) => return Box::pin(async move { Err(error) }),
            });
        }
        // A client reopening a conversation names the file it left; a client
        // starting one gets whatever the command line asked for.
        if request.session.is_some() {
            config.session = request.session;
        }
        // Servers the editor knows this project needs, on top of the machine's.
        config.mcp_servers = request
            .mcp
            .into_iter()
            .map(|server| elmee_plugin_mcp::Declared {
                name: server.name,
                command: server.command,
                args: server.args,
                env: server.env,
                url: server.url,
                headers: server.headers,
                channel: server.served.map(|served| {
                    std::rc::Rc::new(move |method, params| served.ask(method, params))
                        as elmee_plugin_mcp::Channel
                }),
            })
            .collect();
        Box::pin(async move { compose(assemble_with(config).await?).await })
    }))
    // A session ends when the editor closes it or the connection does. What a
    // project wants done then — distil the transcript, file a note, tell
    // something — is a hook, and the plugin has no business knowing that.
    .sign_in(elmee_frontend_acp::SignIn {
        id: "codex",
        name: "Sign in to Codex",
        description: "Opens a browser to sign in with the ChatGPT account whose subscription pays for Codex.",
        run: Box::new(|| {
            Box::pin(async {
                let open = elmee_plugin_provider::auth::codex_login::open_browser;
                elmee_plugin_provider::auth::codex_login::login(&mut |url| open(url)).await.map(drop)
            })
        }),
        forget: Box::new(|| {
            elmee_plugin_provider::auth::forget();
        }),
        account: Box::new(|| {
            let account = elmee_plugin_provider::auth::stored()?;
            Some(if account.email.is_empty() { "ChatGPT".to_owned() } else { account.email })
        }),
    })
    .on_session_end(Box::new(|session, cwd| {
        Box::pin(async move {
            let transcript = std::path::PathBuf::from(&session);
            after_session(&cwd, transcript.is_file().then_some(transcript.as_path())).await;
        })
    }));
    acp.serve(cwd).await
}

/// Run whatever the project hangs on the end of a session.
///
/// Failure is reported and nothing more. The other hooks gate something that
/// has not happened yet and may refuse it; this one is told about something
/// already done, and an exit code that could hold the process open would be a
/// gate on leaving.
async fn after_session(cwd: &std::path::Path, session: Option<&std::path::Path>) {
    let hooks = match Hooks::discover(cwd.to_path_buf()) {
        Ok(hooks) => hooks,
        Err(error) => return eprintln!("elmee: {error}"),
    };
    if let Err(error) = hooks.after_session(session).await {
        eprintln!("elmee: after_session hook: {error}");
    }
}

/// Everything a turn needs, decided before any of it runs.
struct Assembled {
    provider: MultiProvider,
    config: Config,
    system_prompt: String,
    model_aliases: elmee_plugin_provider::registry::ModelAliases,
    /// Where `web_search` goes, when anywhere.
    search: Option<elmee_plugin_search::Backend>,
}

/// A command line and a machine's configuration, turned into everything a turn
/// needs: which model, through which transport, with what in its prompt.
/// The same assembly for a directory chosen at run time, which is what an
/// editor does when it opens a second session against another project.
/// What a local model's window is taken to be when nothing says: what small
/// runtimes commonly load with, and enough for the fixed part of a request.
const ASSUMED_LOCAL_WINDOW: usize = 32_000;

async fn assemble_with(mut config: Config) -> Result<Assembled> {
    if config.prompt.as_deref() == Some("-") {
        use tokio::io::AsyncReadExt;
        let mut buffer = String::new();
        tokio::io::stdin().read_to_string(&mut buffer).await?;
        config.prompt = Some(buffer.trim().to_owned());
    }
    let registry_search = elmee_plugin_search::configured()?;
    let selected = registry::select(&config.cwd, config.model.clone(), elmee_host::last_model())?;
    config.model = Some(selected.spec);
    // A one-shot run asks at once; anything else has a moment first.
    if config.prompt.is_none() {
        selected.provider.warm();
    }
    // The selected model decides the compaction threshold; an explicit
    // --compact-chars, which is any value but the default, wins over it.
    if config.compact_chars == elmee_core::DEFAULT_COMPACT_CHARS {
        // A model running here that does not say how large its context is
        // usually has a small one, and a runtime that truncates silently
        // instead of refusing would cut the system prompt off the front. So
        // the guess is low and said aloud; a refusal, where there is one,
        // still corrects it downwards.
        let window = selected.context_window.or_else(|| {
            selected.local.then(|| {
                let spec = config.model.as_deref().unwrap_or_default();
                eprintln!(
                    "elmee: {spec} does not say how large its context is; assuming {ASSUMED_LOCAL_WINDOW} \
                     tokens. Declare contextWindow for it in ~/.elmee/models.json to use all of it."
                );
                ASSUMED_LOCAL_WINDOW
            })
        });
        config.compact_chars = elmee_core::compact_chars_for_window(window);
    }
    let mut system_prompt = match &config.system_prompt {
        Some(prompt) => prompt.clone(),
        // A spawned agent has a depth; it gets no user memory.
        None => {
            let mut skills = elmee_plugin_project::skills::discover(&config.cwd)?;
            if let Some(allowed) = &config.skills {
                elmee_plugin_project::skills::only_named(&mut skills, allowed);
            }
            context::system_prompt_for(&config.cwd, config.agent_depth > 0, skills, config.compact_chars)?
        }
    };
    if !config.additional_dirs.is_empty() {
        system_prompt.push_str("\n\nAdditional workspace directories (use their absolute paths):\n");
        for directory in &config.additional_dirs {
            system_prompt.push_str(&format!("{}\n", directory.display()));
        }
    }
    if let Some(extra) = &config.append_system_prompt {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(extra);
    }
    Ok(Assembled {
        provider: selected.provider,
        config,
        system_prompt,
        model_aliases: selected.aliases,
        search: registry_search,
    })
}

/// One assembled agent, ready to be driven. Separated from the frontend choice
/// because ACP asks for another one whenever an editor opens another session,
/// and building an agent must not mean deciding how a person talks to it.
async fn compose(
    assembled: Assembled,
) -> Result<(Driver, tokio::sync::mpsc::UnboundedReceiver<elmee_host::RuntimeEvent>)> {
    let Assembled { provider, mut config, system_prompt, model_aliases, search } = assembled;
    if config.max_tokens > 0 && config.token_ledger.is_none() {
        config.token_ledger = Some(match &config.session {
            Some(session) => session.with_extension("tokens"),
            None => elmee_plugin_paths::project_state_directory(&config.cwd)
                .join("budgets")
                .join(format!("{}.tokens", std::process::id())),
        });
    }
    let hooks = Hooks::discover(config.cwd.clone())?;
    let mut tools = ToolSet::discover(config.cwd.clone())?;
    let explain = std::env::args_os().any(|arg| arg == "--explain-plugins");
    // The resolved allowlist is the one authority for every tool namespace.
    let offered =
        |name: &str| config.tools.as_ref().is_none_or(|allowed| allowed.iter().any(|tool| tool == name));
    // Background agents are a frontend capability; a bare one-shot run has no
    // runtime to answer them, so they are only offered when one exists. What
    // they need said rides in their own descriptions, so nothing else here has
    // to remember to say it.
    if config.acp || config.prompt.is_none() {
        let host = host_tools(model_aliases.roster().as_deref());
        let any_offered =
            host.iter().any(|definition| definition["function"]["name"].as_str().is_some_and(offered));
        if any_offered {
            tools.add_host_tools(host);
        }
    }
    let search = offered("web_search").then(|| search.map(elmee_plugin_search::SearchPlugin::new)).flatten();
    let session = if explain {
        elmee_core::session::Session::memory()
    } else {
        elmee_plugin_session::open(config.session.clone())?
    };
    let mut builder = AgentBuilder::new(system_prompt)
        .limits(config.max_steps, config.compact_chars)
        .interactive(config.acp || config.prompt.is_none());
    provider.context().set(session.path());
    builder.plugin(provider)?;
    builder.plugin(tools)?;
    if let Some(search) = search {
        builder.plugin(search)?;
    }
    builder.plugin(hooks)?;
    builder.plugin(
        elmee_plugin_tools::WorkspacePlugin::new(config.cwd.clone())
            .with_roots(config.additional_dirs.clone())
            .with_snapshots(),
    )?;
    builder.plugin(elmee_plugin_project::MemoryPlugin::new(config.cwd.clone()))?;
    // Instructions for a directory arrive with the first tool result that
    // touches it, so the cached system prompt never has to carry rules the
    // task may never reach.
    builder.plugin(
        elmee_plugin_project::Directives::new(config.cwd.clone()).with_roots(config.additional_dirs.clone()),
    )?;
    // A one-shot run that was given no goal cannot acquire one, so the tool
    // that ends a goal would be schema sent with every request for nothing.
    if offered(elmee_plugin_goal::DONE_TOOL) && (config.prompt.is_none() || config.goal.is_some()) {
        builder.plugin(elmee_plugin_goal::GoalPlugin::default())?;
    }
    if offered("web_fetch") {
        builder.plugin(elmee_plugin_web::WebPlugin)?;
    }
    builder.plugin(session)?;
    if let Some(path) = config.token_ledger.clone() {
        builder.plugin(elmee_plugin_ledger::FileTokenLedger::new(path))?;
    }
    builder.plugin(
        elmee_plugin_snapshot::Snapshots::new(&config.cwd).with_roots(config.additional_dirs.clone()),
    )?;
    // MCP discovery may start user-declared processes to obtain their dynamic
    // schemas. Construct it only after every inert contribution has succeeded;
    // it is then the final atomic contribution before graph resolution.
    if config.tools.as_ref().is_none_or(|allowed| allowed.iter().any(|tool| tool.starts_with("mcp__"))) {
        builder.plugin(
            elmee_plugin_mcp::McpPlugin::for_mode(!explain, config.cwd.clone(), config.mcp_servers.clone())
                .await?,
        )?;
    }
    if let Some(allowed) = &config.tools {
        builder.allow_tools(allowed)?;
    }
    let mut agent = builder.build()?;
    if explain {
        println!("{}", agent.composition());
        std::process::exit(0);
    }
    agent.set_token_budget(config.max_tokens);
    agent.require_verification(config.require_verification);
    agent.set_reasoning_effort(config.reasoning_effort);
    if let Some(objective) = config.goal.clone() {
        agent.plugin_command(elmee_plugin_goal::COMMAND, &objective)?;
    }

    // Every mode gets the same runtime and the same driver. What differs is
    // only how long the frontend stays: one turn, until the caller closes the
    // pipe, or until a person quits. Behaviour inside a turn must not depend
    // on which of those was chosen.
    let (runtime, events) = Runtime::new(
        &elmee_host::Options {
            cwd: config.cwd.clone(),
            name: config.name.clone(),
            model: config.model.clone().unwrap_or_default(),
            depth: config.agent_depth,
            require_verification: config.require_verification,
            max_tokens: config.max_tokens,
            token_ledger: config.token_ledger.clone(),
        },
        model_aliases,
    )?;
    let driver = Driver::new(agent, runtime);
    // This machine's model from now on. A run started with `-m` is a choice
    // like any other, so it settles the next bare run the same way `/model`
    // does. A delegated run is excluded: it uses whatever model it was handed,
    // which is nobody's choice, so it does not get to speak for the next one.
    if config.agent_depth == 0
        && let Some(spec) = &config.model
    {
        elmee_host::remember_model(spec);
    }
    Ok((driver, events))
}

type Events = tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>;

#[cfg(feature = "tui")]
async fn interactive(driver: Driver, config: &Config, events: Events) -> Result<i32> {
    let options = elmee_frontend_tui::Options {
        model: config.model.clone().unwrap_or_default(),
        cwd: config.cwd.clone(),
        persistent_session: config.session.is_some(),
        compact_chars: (config.compact_chars > 0).then_some(config.compact_chars),
    };
    elmee_frontend_tui::run(driver, &options, events).await
}

#[cfg(not(feature = "tui"))]
async fn interactive(_driver: Driver, _config: &Config, _events: Events) -> Result<i32> {
    Err(elmee_core::Error::Config(
        "this build has no terminal interface: pass a PROMPT, or --acp to serve an editor".into(),
    ))
}

async fn launch(assembled: Assembled) -> Result<i32> {
    let base = assembled.config.clone();
    let (driver, events) = compose(assembled).await?;

    // Said once, before the first prompt: discovering mid-task that two tools
    // do not exist costs a turn and reads to the model like a broken project.
    if let Some(warning) = elmee_plugin_tools::tools::missing_ripgrep() {
        eprintln!("elmee: {warning}");
    }
    // The frontend decides only how long to stay: one turn, or until a
    // person quits.
    let code = match base.prompt.clone() {
        Some(prompt) => {
            let mut driver = driver;
            elmee_frontend_headless::run(&mut driver, prompt, base.json, events).await?
        }
        None => interactive(driver, &base, events).await?,
    };
    after_session(&base.cwd, base.session.as_deref()).await;
    Ok(code)
}
