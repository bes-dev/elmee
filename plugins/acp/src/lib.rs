#![forbid(unsafe_code)]

//! The Agent Client Protocol, whole, in one plugin.
//!
//! elmee's own duplex protocol was replaced by this one rather than joined to
//! it: two hand-written protocols for the same job drift, and the second one
//! was ours to document, version and explain to every embedder. ACP is
//! published, schema'd, and already spoken by editors — including Emacs, which
//! is where this is used.
//!
//! **Speed is why the shape is what it is.** The wire format decides nothing
//! measurable: a JSON line costs microseconds against a model's hundreds of
//! milliseconds. What an embedded agent actually pays for is *starting*, and
//! the protocol is what decides how often that happens. Our old protocol was
//! one process per session, so an editor with four buffers paid four process
//! starts, four plugin graphs and four cold prompt caches. ACP carries a
//! session id on every message, so one warm process serves them all — and the
//! agent the application already assembled becomes the first session, so the
//! first `session/new` costs nothing at all.
//!
//! Everything the protocol needs lives here: framing, dispatch, the session
//! pool and the translation of a turn's events. No other crate learns that
//! ACP exists, and the kernel is untouched — a frontend is exactly the seam
//! the architecture already had for this.

mod ask;
mod content;
mod pool;
mod turn;
mod update;
mod wire;

pub use wire::Served;
use {ask::*, content::*, pool::*, turn::*, wire::*};

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

use elmee_core::{PluginFuture, Result};
use elmee_host::{Driver, RuntimeEvent};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
/// The version this speaks. `LATEST` in the published schema, and what the
/// editors that ship a client negotiate.
const PROTOCOL_VERSION: u16 = 1;
/// How an application makes another agent when the client opens another
/// session. The frontend deliberately cannot build one itself: which plugins
/// an agent is composed of is the composition root's business, and a frontend
/// that knew would be a frontend that has to be edited whenever the product
/// changes.
pub type NewSession =
    Box<dyn Fn(Requested) -> PluginFuture<'static, Result<(Driver, mpsc::UnboundedReceiver<RuntimeEvent>)>>>;
/// Told that a session is over, so the application can run whatever a project
/// hangs on that. It is given the session's id, which for a persisted session
/// is the path of its transcript.
///
/// The protocol has no such notion, and it is not the plugin's to invent one:
/// what happens at the end of a session is the composition root's business,
/// and this is only where the plugin knows it happened.
pub type SessionEnded = Box<dyn Fn(String, PathBuf) -> PluginFuture<'static, ()>>;
/// What the client asked for, in terms an application already understands.
///
/// The protocol's own spelling stops here: a composition root should not have
/// to read an ACP schema to answer this, and if it did, the protocol would
/// have leaked out of its plugin.
pub struct Requested {
    pub cwd: PathBuf,
    /// A conversation to reopen, when the client is resuming one rather than
    /// starting fresh.
    pub session: Option<PathBuf>,
    /// Servers the client wants this session to have, on top of whatever the
    /// machine declares for itself. An editor is often the thing that knows
    /// which ones a project needs.
    pub mcp: Vec<DeclaredServer>,
    pub additional_dirs: Vec<PathBuf>,
    pub system_prompt: Option<SystemPrompt>,
    /// Active conversation copied into a new session, never the source store.
    pub history: Vec<elmee_core::message::Message>,
    pub settings: Option<(String, Option<elmee_core::provider::ReasoningEffort>)>,
}
/// Instructions the client gave for one session: added to the agent's own, or
/// standing in their place.
#[derive(Clone, PartialEq, Eq)]
pub struct SystemPrompt {
    pub replace: bool,
    pub text: String,
}
/// One MCP server a client declared, as a description rather than a transport:
/// a command to start, a URL to reach, or the client itself to ask.
#[derive(Clone, PartialEq, Eq)]
pub struct DeclaredServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    pub served: Option<Served>,
}
/// A way to sign in that the client may offer its user: what to call it, and
/// what to run when it is chosen. The agent does the signing in itself — a
/// browser, a redirect — and the client only waits for it to finish.
pub struct SignIn {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub run: Box<dyn Fn() -> PluginFuture<'static, Result<()>>>,
    pub forget: Box<dyn Fn()>,
    /// Who is signed in, as a person would recognise the account.
    pub account: Box<dyn Fn() -> Option<String>>,
}
#[derive(Clone)]
pub struct Acp {
    new_session: Rc<NewSession>,
    ended: Option<Rc<SessionEnded>>,
    sign_in: Option<Rc<SignIn>>,
}
impl Acp {
    #[must_use]
    pub fn new(new_session: NewSession) -> Self {
        Self { new_session: Rc::new(new_session), ended: None, sign_in: None }
    }

    /// The account this agent can be signed in to from the client, if any.
    #[must_use]
    pub fn sign_in(mut self, sign_in: SignIn) -> Self {
        self.sign_in = Some(Rc::new(sign_in));
        self
    }

    /// What to run when a session ends, if the application has anything.
    #[must_use]
    pub fn on_session_end(mut self, ended: SessionEnded) -> Self {
        self.ended = Some(Rc::new(ended));
        self
    }

    /// Serve one client for as long as it keeps stdin open.
    ///
    /// It is handed a way to build agents rather than an agent already built,
    /// because building one is the expensive thing: an editor asking
    /// `initialize` would otherwise wait for a plugin graph, MCP children and a
    /// model catalogue before learning whether this binary speaks its protocol
    /// at all. Here the handshake is answered from nothing, and the agent for
    /// `cwd` is assembled behind it — so by the time the editor asks for a
    /// session, the usual one is already standing.
    pub async fn serve(&self, cwd: PathBuf) -> Result<i32> {
        serve(self, cwd).await
    }
}
/// One client connection: the sessions it has open and the way back to it.
#[derive(Clone)]
struct Connection {
    sessions: Sessions,
    client: Client,
    next: Rc<Cell<u64>>,
    jobs: Rc<RefCell<tokio::task::JoinSet<()>>>,
    cleanup: Rc<RefCell<tokio::task::JoinSet<()>>>,
    cancellations: Rc<RefCell<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    /// Set once the client has said hello. Every session method is refused
    /// before that, because the client's capabilities decide what is allowed.
    initialized: bool,
}
impl Connection {
    fn session_id(&mut self) -> String {
        self.next.set(self.next.get() + 1);
        format!("elmee-{}-{}", std::process::id(), self.next.get())
    }
}
async fn serve(acp: &Acp, cwd: PathBuf) -> Result<i32> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    // One writer for the whole process: sessions answer concurrently, and two
    // half-written lines interleaved on stdout is a protocol error that no
    // client can recover from.
    let writer = tokio::task::spawn_local(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&message).unwrap_or_default();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut server = Connection {
        sessions: Sessions::default(),
        client: Client::new(out_tx),
        next: Rc::default(),
        jobs: Rc::default(),
        cleanup: Rc::default(),
        initialized: false,
        cancellations: Rc::default(),
    };
    let mut spare: Option<Prewarm> = None;
    let started_in = cwd;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            let _ = server.client.out.send(failed(Value::Null, INVALID_REQUEST, "not JSON"));
            continue;
        };
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str).map(str::to_owned) else {
            // No method means this is an answer to something we asked.
            if let Some(id) = &id {
                server.client.answered(id, &message);
            }
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        dispatch(acp, &mut server, &mut spare, &started_in, id, &method, params).await;
    }

    drop(spare);
    let mut jobs = std::mem::take(&mut *server.jobs.borrow_mut());
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}

    // Stdin closed: the client is gone, and this process has no reason to
    // outlive it. Sessions are cleared by hand rather than left to the pool's
    // own drop, because each session's drain task holds the pool alive — the
    // agent and its MCP children are released only when the pool
    // gives them up first. Questions still in flight are released with them:
    // there is nobody left who could answer one.
    let ending: Vec<(String, PathBuf)> =
        server.sessions.borrow().iter().map(|(id, open)| (id.clone(), open.cwd.clone())).collect();
    server.sessions.borrow_mut().clear();
    server.client.pending.borrow_mut().clear();
    let mut cleanup = std::mem::take(&mut *server.cleanup.borrow_mut());
    while cleanup.join_next().await.is_some() {}
    // After clearing, so the agent and its children are already released and a
    // hook that reads the transcript reads a finished one.
    if let Some(ended) = acp.ended.as_ref() {
        for (session, cwd) in ending {
            ended(session, cwd).await;
        }
    }
    drop(server);
    let _ = writer.await;
    Ok(0)
}
async fn dispatch(
    acp: &Acp,
    server: &mut Connection,
    spare: &mut Option<Prewarm>,
    started_in: &Path,
    id: Option<Value>,
    method: &str,
    params: Value,
) {
    // A notification has no id and takes no answer; a request must always get
    // exactly one, including when it is refused.
    let answer = |server: &Connection, value: Value| {
        if let Some(id) = id.clone() {
            let _ = server.client.out.send(match value.get("__error") {
                Some(error) => failed(
                    id,
                    error["code"].as_i64().unwrap_or(INTERNAL_ERROR),
                    error["message"].as_str().unwrap_or("failed"),
                ),
                None => ok(id, value),
            });
        }
    };
    let error = |code: i64, message: &str| json!({ "__error": { "code": code, "message": message } });
    let field = |name: &str| params.get(name).and_then(Value::as_str);

    match method {
        "initialize" => {
            server.initialized = true;
            // What the client can do decides what a turn may attempt: an agent
            // that asks a question nobody can display has stopped for nothing.
            let capabilities = &params["clientCapabilities"];
            let has = |pointer: &str| capabilities.pointer(pointer).is_some_and(|it| !it.is_null());
            server.client.shows.set(Shows {
                forms: has("/elicitation/form"),
                compaction: has("/session/compaction"),
                notices: has("/session/notices"),
                subagents: has("/subagents"),
            });
            answer(server, initialize(&params, acp.sign_in.as_deref()));
            // Answered first, built second. The editor has its capabilities
            // and this process spends the wait assembling the agent it is
            // most likely to be asked for.
            if spare.is_none() {
                *spare = Some(prewarm(acp, started_in.to_path_buf()));
            }
        }
        // v2 of the schema spells these `auth/*`. Signing in waits on a person
        // and a browser, so it is answered when it ends and the connection
        // goes on being read meanwhile.
        "authenticate" | "auth/login" => {
            match acp.sign_in.clone().filter(|known| field("methodId") == Some(known.id)) {
                // Editors ask with whatever they remembered, on connect and on
                // reconnect; a method this agent does not have is nothing to do,
                // not a reason for the editor to decide it failed to connect.
                None => answer(server, json!({})),
                Some(sign_in) => {
                    let (out, id) = (server.client.out.clone(), id.clone());
                    server.jobs.borrow_mut().spawn_local(async move {
                        let outcome = (sign_in.run)().await;
                        if let Some(id) = id {
                            let _ = out.send(match outcome {
                                Ok(()) => ok(id, json!({})),
                                Err(reason) => failed(id, INTERNAL_ERROR, reason),
                            });
                        }
                    });
                }
            }
        }
        "logout" | "auth/logout" => {
            if let Some(sign_in) = &acp.sign_in {
                (sign_in.forget)();
            }
            answer(server, json!({}));
        }
        // Signed in is a fact; signed out is not one, because whether an
        // account is needed at all depends on the model a session runs on.
        "auth/status" => answer(
            server,
            match acp.sign_in.as_ref().map(|sign_in| (sign_in.account)()) {
                None => json!({ "state": "not_required" }),
                Some(None) => json!({ "state": "unknown" }),
                Some(Some(label)) => {
                    json!({ "state": "authenticated", "accounts": [{ "label": label, "active": true }] })
                }
            },
        ),
        "$/cancel_request" => {
            if let Some(cancel) = params
                .get("requestId")
                .and_then(|id| server.cancellations.borrow_mut().remove(&id.to_string()))
            {
                let _ = cancel.send(());
            }
        }
        _ if !server.initialized => answer(server, error(INVALID_REQUEST, "initialize first")),
        "session/new" => {
            let Some(cwd) = field("cwd").map(resolved) else {
                return answer(server, error(INVALID_PARAMS, "session/new needs an absolute cwd"));
            };
            let asked = additional_dirs(&params).and_then(|dirs| Ok((dirs, system_prompt(&params)?)));
            let (additional_dirs, system_prompt) = match asked {
                Ok(asked) => asked,
                Err(message) => return answer(server, error(INVALID_PARAMS, message)),
            };
            start_open(
                acp,
                server,
                spare,
                Requested {
                    cwd,
                    session: None,
                    mcp: declared(&params, &server.client),
                    additional_dirs,
                    system_prompt,
                    history: Vec::new(),
                    settings: None,
                },
                id,
                false,
            );
        }
        // `load` hands the conversation back as updates first; `resume` only
        // reopens it, for a client that kept its own copy.
        "session/load" | "session/resume" => {
            let (cwd, file) = (field("cwd").map(resolved), field("sessionId").map(PathBuf::from));
            let (Some(cwd), Some(file)) = (cwd, file) else {
                return answer(
                    server,
                    error(INVALID_PARAMS, &format!("{method} needs a cwd and a sessionId")),
                );
            };
            let existing = server.sessions.borrow().get(&file.to_string_lossy().to_string()).map(|open| {
                let compatible = same(&open.cwd, &cwd)
                    && params.get("mcpServers").is_none_or(|_| declared(&params, &server.client) == open.mcp)
                    && params.get("additionalDirectories").is_none_or(|_| {
                        additional_dirs(&params).is_ok_and(|dirs| dirs == open.additional_dirs)
                    });
                (compatible, open.driver.try_borrow().is_ok())
            });
            if let Some((compatible, idle)) = existing {
                if !compatible {
                    return answer(
                        server,
                        error(INVALID_PARAMS, "open session workspace or MCP configuration differs"),
                    );
                }
                if !idle {
                    return answer(server, error(INVALID_REQUEST, "this session is busy"));
                }
                let session = file.to_string_lossy();
                if method == "session/load" {
                    replay(server, &session);
                }
                answer(
                    server,
                    json!({"configOptions":config_options(server,&session),"_meta":mesh_identity(server,&session)}),
                );
                return;
            }
            // Refused rather than started empty: a client that asks for a
            // conversation and is given a blank one has been told a lie it
            // cannot detect.
            if !file.is_file() {
                return answer(server, error(INVALID_PARAMS, &format!("no session at {}", file.display())));
            }
            let asked = additional_dirs(&params).and_then(|dirs| Ok((dirs, system_prompt(&params)?)));
            let (additional_dirs, system_prompt) = match asked {
                Ok(asked) => asked,
                Err(message) => return answer(server, error(INVALID_PARAMS, message)),
            };
            let request = Requested {
                cwd,
                session: Some(file.clone()),
                mcp: declared(&params, &server.client),
                additional_dirs,
                system_prompt,
                history: Vec::new(),
                settings: None,
            };
            start_open(acp, server, spare, request, id, method == "session/load");
        }
        "session/fork" => {
            let pool = server.sessions.borrow();
            let Some(parent) = field("sessionId").and_then(|session| pool.get(session)) else {
                return answer(server, error(INVALID_PARAMS, "fork needs an open parent session"));
            };
            let Ok(driver) = parent.driver.try_borrow() else {
                return answer(server, error(INVALID_REQUEST, "cannot fork a running session"));
            };
            let request = Requested {
                cwd: field("cwd").map(resolved).unwrap_or_else(|| parent.cwd.clone()),
                session: None,
                mcp: if params.get("mcpServers").is_none() {
                    parent.mcp.clone()
                } else {
                    declared(&params, &server.client)
                },
                system_prompt: match system_prompt(&params) {
                    Ok(given) => given.or_else(|| parent.system_prompt.clone()),
                    Err(message) => return answer(server, error(INVALID_PARAMS, message)),
                },
                additional_dirs: if params.get("additionalDirectories").is_none() {
                    parent.additional_dirs.clone()
                } else {
                    match additional_dirs(&params) {
                        Ok(dirs) => dirs,
                        Err(message) => return answer(server, error(INVALID_PARAMS, message)),
                    }
                },
                history: driver.agent.messages().to_vec(),
                settings: Some((parent.model.clone(), driver.reasoning_effort())),
            };
            drop(driver);
            drop(pool);
            start_open(acp, server, spare, request, id, false);
        }
        "session/set_config_option" => {
            let (session, value) = (field("sessionId").unwrap_or_default(), field("value"));
            let config_id = field("configId").unwrap_or_default();
            if !matches!(config_id, "model" | "reasoning_effort") {
                return answer(server, error(INVALID_PARAMS, "unknown config option"));
            }
            let Some(value) = value else {
                return answer(server, error(INVALID_PARAMS, "value is required"));
            };
            let effort = match (config_id, value) {
                ("reasoning_effort", "auto") => Some(None),
                ("reasoning_effort", value) => match elmee_core::provider::ReasoningEffort::parse(value) {
                    Some(effort) => Some(Some(effort)),
                    None => return answer(server, error(INVALID_PARAMS, "invalid reasoning effort")),
                },
                _ => None,
            };
            let mut pool = server.sessions.borrow_mut();
            let Some(open) = pool.get_mut(session) else {
                return answer(server, error(INVALID_PARAMS, &format!("no session {session}")));
            };
            // A turn in flight owns the agent; the switch waits for the person
            // to ask again rather than for the model to finish.
            let Ok(mut driver) = open.driver.try_borrow_mut() else {
                return answer(server, error(INVALID_REQUEST, "this session is busy"));
            };
            if config_id == "model" {
                driver.set_model(value);
                open.model = value.to_owned();
            } else {
                driver.set_reasoning_effort(effort.flatten());
                open.reasoning_effort = value.to_owned();
            }
            let options = open.config_options();
            drop(driver);
            drop(pool);
            answer(server, json!({ "configOptions": options }));
        }
        "session/prompt" => prompt(server, id, &params),
        "_session/steering" => {
            let Some(session) = field("sessionId") else {
                return answer(server, error(INVALID_PARAMS, "sessionId is required"));
            };
            if params.get("prompt").and_then(Value::as_array).is_none_or(|blocks| {
                blocks.is_empty()
                    || blocks.iter().any(|block| {
                        !matches!(block["type"].as_str(), Some("text" | "resource" | "resource_link"))
                    })
            }) {
                return answer(server, error(INVALID_PARAMS, "steering needs text or embedded context"));
            }
            let idle = params.pointer("/_meta/steering/idleBehavior");
            if idle.is_some_and(|value| value != "promptRequired") {
                return answer(server, error(INVALID_PARAMS, "unknown steering idleBehavior"));
            }
            let pool = server.sessions.borrow();
            let Some(open) = pool.get(session) else {
                return answer(server, error(INVALID_PARAMS, "unknown session"));
            };
            let text = prompt_text(&params, &open.cwd);
            if text.is_empty() {
                return answer(server, error(INVALID_PARAMS, "steering is empty"));
            }
            if open.cancel.is_some() {
                open.steerer.steer(&text);
                answer(server, json!({ "outcome": "injected" }));
            } else if idle.is_some() {
                answer(server, json!({ "outcome": "promptRequired", "reason": "noRunningTurn" }));
            } else {
                drop(pool);
                prompt(server, None, &params);
                answer(server, json!({ "outcome": "startedNewTurn" }));
            }
        }
        "session/cancel" => {
            if let Some(session) = field("sessionId")
                && let Some(open) = server.sessions.borrow_mut().get_mut(session)
                && let Some(cancel) = open.cancel.take()
            {
                let _ = cancel.send(());
            }
        }
        "session/close" => {
            let named = field("sessionId").map(str::to_owned);
            let mut closed = named.as_ref().and_then(|session| server.sessions.borrow_mut().remove(session));
            if let Some(session) = &named {
                server.client.close(session);
            }
            if let Some((Some(id), _)) = closed.as_mut().and_then(|open| open.queued.take()) {
                let _ = server.client.out.send(ok(id, json!({"stopReason":"cancelled"})));
            }
            // Dropping the agent is what releases its MCP children and its
            // browser, which is the point of closing rather than forgetting.
            let cwd = closed.as_ref().map(|open| open.cwd.clone());
            let driver = closed.as_ref().map(|open| Rc::downgrade(&open.driver));
            drop(closed);
            answer(server, json!({}));
            // Answered first: what a project hangs on the end of a session is
            // its own business and may take as long as it likes, and a client
            // waiting to close a buffer should not be holding the line for it.
            if let (Some(ended), Some(session), Some(cwd)) = (acp.ended.as_ref(), named, cwd) {
                let ended = ended.clone();
                let mut cleanup = server.cleanup.borrow_mut();
                while cleanup.try_join_next().is_some() {}
                cleanup.spawn_local(async move {
                    if let Some(driver) = driver {
                        while driver.strong_count() > 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    ended(session, cwd).await
                });
            }
        }
        "session/delete" => {
            let Some(file) = field("sessionId") else {
                return answer(server, error(INVALID_PARAMS, "session/delete needs a sessionId"));
            };
            if server.sessions.borrow().contains_key(file) {
                return answer(server, error(INVALID_REQUEST, "close the session before deleting it"));
            }
            answer(
                server,
                match elmee_host::session::delete(Path::new(file)) {
                    Ok(()) => json!({}),
                    Err(reason) => error(INVALID_PARAMS, &reason.to_string()),
                },
            );
        }
        "session/set_title" => {
            let (Some(session), Some(title)) = (field("sessionId"), field("title")) else {
                return answer(
                    server,
                    error(INVALID_PARAMS, "session/set_title needs a sessionId and a title"),
                );
            };
            // An open session is renamed through its agent, which holds the
            // file; one that is only on disk is renamed where it lies.
            let named = match server.sessions.borrow().get(session) {
                Some(open) => match open.driver.try_borrow_mut() {
                    Ok(mut driver) => driver.agent.set_title(title.to_owned()),
                    Err(_) => return answer(server, error(INVALID_REQUEST, "this session is busy")),
                },
                None => elmee_host::session::rename(Path::new(session), title.to_owned()),
            };
            match named {
                Ok(()) => {
                    server
                        .client
                        .update(session, json!({ "sessionUpdate": "session_info_update", "title": title }));
                    answer(server, json!({ "title": title }));
                }
                Err(reason) => answer(server, error(INVALID_PARAMS, &reason.to_string())),
            }
        }
        "session/status" => {
            let status = match field("sessionId") {
                Some(session) if server.sessions.borrow().contains_key(session) => "open",
                Some(session) if Path::new(session).is_file() => "saved",
                _ => return answer(server, error(INVALID_PARAMS, "unknown session")),
            };
            answer(server, json!({ "status": status }));
        }
        // The conversation as the tree it is: what was asked on the line it
        // stands on, and the lines a rewind left behind. Moving to another
        // line answers with the tree as it is afterwards.
        "_session/history" | "_session/checkout" => {
            let pool = server.sessions.borrow();
            let Some((session, open)) =
                field("sessionId").and_then(|session| Some((session, pool.get(session)?)))
            else {
                return answer(server, error(INVALID_PARAMS, "unknown session"));
            };
            let Ok(mut driver) = open.driver.try_borrow_mut() else {
                return answer(server, error(INVALID_REQUEST, "this session is busy"));
            };
            if method == "_session/checkout" {
                let moved = params["leaf"]
                    .as_u64()
                    .ok_or_else(|| "leaf is required".to_owned())
                    .and_then(|leaf| driver.agent.checkout(leaf).map_err(|reason| reason.to_string()));
                if let Err(reason) = moved {
                    return answer(server, error(INVALID_PARAMS, &reason));
                }
                server.client.update(session, json!({ "sessionUpdate": "plan", "entries": [] }));
            }
            answer(server, history(&driver));
        }
        "_session/rewind" => rewind(server, id, &params),
        "session/list" => {
            answer(
                server,
                match listed(server, field("cwd").map(resolved), started_in) {
                    Ok(sessions) => json!({ "sessions": sessions }),
                    Err(reason) => error(INVALID_REQUEST, &reason.to_string()),
                },
            );
        }
        _ => answer(server, error(METHOD_NOT_FOUND, method)),
    }
}
/// Building an agent may wait on MCP. It must never own the stdin reader:
/// cancellation and control messages for other sessions remain answerable.
fn start_open(
    acp: &Acp,
    server: &Connection,
    spare: &mut Option<Prewarm>,
    request: Requested,
    id: Option<Value>,
    replay_history: bool,
) {
    let warm = spare.take_if(|warm| warm.matches(&request));
    let (acp, mut opening) = (acp.clone(), server.clone());
    let mut jobs = server.jobs.borrow_mut();
    while jobs.try_join_next().is_some() {}
    if jobs.len() >= 32 {
        if let Some(id) = id {
            let _ = server.client.out.send(failed(id, -32001, "too many session setup requests"));
        }
        return;
    }
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let key = id.as_ref().map(Value::to_string);
    if let Some(key) = &key {
        if server.cancellations.borrow().contains_key(key) {
            if let Some(id) = id {
                let _ = server.client.out.send(failed(id, INVALID_REQUEST, "duplicate request id"));
            }
            return;
        }
        server.cancellations.borrow_mut().insert(key.clone(), cancel);
    }
    jobs.spawn_local(async move {
        let fresh = request.session.is_none();
        let mut warm = warm;
        let outcome = tokio::select! {
            result = open(&acp, &mut opening, &mut warm, request) => result,
            _ = cancelled, if key.is_some() => {
                if let Some(key) = &key { opening.cancellations.borrow_mut().remove(key); }
                if let Some(id) = id { let _ = opening.client.out.send(failed(id, -32800, "request cancelled")); }
                return;
            }
        };
        if let Some(key) = &key { opening.cancellations.borrow_mut().remove(key); }
        let result = match outcome {
            Ok(session) => {
                if replay_history { replay(&opening, &session); }
                let mut result = json!({
                    "configOptions": config_options(&opening, &session),
                    "_meta": mesh_identity(&opening, &session),
                });
                if fresh { result["sessionId"] = json!(session); }
                if let Some(id) = id {
                    let _ = opening.client.out.send(ok(id, result));
                }
                advertise_commands(&opening, &session);
                warn(&opening, &session);
                return;
            }
            Err(reason) => reason,
        };
        if let Some(id) = id {
            let _ = opening.client.out.send(failed(id, INTERNAL_ERROR, result));
        }
    });
}

/// What this agent is and what it will accept.
///
/// The version is negotiated down, never up: a client that speaks an older
/// protocol is answered in its own, and one that speaks a newer gets ours,
/// which is what the specification asks for.
fn initialize(params: &Value, sign_in: Option<&SignIn>) -> Value {
    let theirs = params.get("protocolVersion").and_then(Value::as_u64).unwrap_or(u64::from(PROTOCOL_VERSION));
    json!({
        "protocolVersion": theirs.min(u64::from(PROTOCOL_VERSION)),
        "agentInfo": { "name": "elmee", "version": env!("CARGO_PKG_VERSION") },
        "_meta": { "steering": { "supported": true }, "rewind": { "supported": true } },
        "authMethods": sign_in
            .map(|method| json!({ "id": method.id, "name": method.name, "description": method.description }))
            .into_iter()
            .collect::<Vec<_>>(),
        "agentCapabilities": {
            // A session is a file, so its path is its id: a client may hand
            // one back and get the conversation it left, replayed as updates.
            "loadSession": true,
            "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
            // `acp` is a server the client itself is, asked over this
            // connection instead of started or dialled.
            "mcpCapabilities": { "http": true, "sse": false, "acp": true },
            "sessionCapabilities": {
                "close": {}, "list": {}, "delete": {}, "resume": {}, "fork": {}, "title": {}, "status": {},
                "additionalDirectories": {}, "subagents": {},
                "systemPrompt": { "mode": { "override": {} } },
            },
            "auth": if sign_in.is_some() { json!({ "status": {}, "logout": {} }) } else { json!({ "status": {} }) },
            // No modes: this agent has one way to behave, and advertising a
            // switch that does nothing would make a client draw a control
            // that changes nothing. The model is the one real choice, and it
            // is a config option.
        },
    })
}

#[cfg(test)]
mod tests;
