//! The sessions one connection has open: building an agent for each, what is
//! answerable about it without touching it, and the conversations on disk.

use super::*;

/// One open session: everything answerable about it without touching the
/// agent, plus the agent itself.
///
/// The metadata is copied out rather than read through the driver, because a
/// running turn holds the driver for as long as the model takes. A client
/// asking what sessions exist while one of them thinks must not wait for it.
pub(super) struct Session {
    pub(super) driver: Rc<RefCell<Driver>>,
    pub(super) cwd: PathBuf,
    pub(super) steerer: elmee_host::Steerer,
    pub(super) mcp: Vec<DeclaredServer>,
    pub(super) additional_dirs: Vec<PathBuf>,
    pub(super) system_prompt: Option<SystemPrompt>,
    /// The model this runs on, and the ones the client may switch it to.
    pub(super) model: String,
    pub(super) models: Vec<String>,
    pub(super) reasoning_effort: String,
    /// Set while a prompt is in flight, so `session/cancel` has something to
    /// stop and a second prompt has something to be refused against. Cancelling
    /// is a message rather than an abort, because an aborted task answers
    /// nothing and every request must be answered exactly once.
    pub(super) cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Whether the running turn is one nobody asked for — a subagent's report
    /// arriving on its own. A person typing outranks it.
    pub(super) unprompted: bool,
    /// A prompt that arrived while such a turn was running, waiting for it to
    /// give the session back.
    pub(super) queued: Option<(Option<Value>, String)>,
    /// The question of what else this could run on, while it is still out. It
    /// ends with the session: an endpoint that never answers must not hold a
    /// closed session's process open.
    pub(super) _listing: Option<Listing>,
}

pub(super) struct Listing(pub(super) tokio::task::JoinHandle<()>);

impl Drop for Listing {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Session {
    /// The runtime choices this agent exposes in the shape a client draws
    /// selectors from. Modes are deliberately absent: the model and its
    /// reasoning depth are actual request parameters, not behavior presets.
    pub(super) fn config_options(&self) -> Value {
        let mut ids: Vec<&str> = self.models.iter().map(String::as_str).collect();
        if !ids.contains(&self.model.as_str()) {
            ids.insert(0, &self.model);
        }
        json!([{
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": self.model,
            "options": ids.iter().map(|id| json!({ "value": id, "name": id })).collect::<Vec<_>>(),
        }, {
            "id": "reasoning_effort",
            "name": "Reasoning effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": self.reasoning_effort,
            "options": std::iter::once("auto")
                .chain(elmee_core::provider::ReasoningEffort::VALUES)
                .map(|value| json!({ "value": value, "name": value }))
                .collect::<Vec<_>>(),
        }])
    }
}

pub(super) type Sessions = Rc<RefCell<HashMap<String, Session>>>;

pub(super) type Built = (Driver, mpsc::UnboundedReceiver<RuntimeEvent>);

/// An agent being built for a directory nobody has asked for yet.
pub(super) struct Prewarm {
    pub(super) cwd: PathBuf,
    pub(super) building: tokio::task::JoinHandle<Result<Built>>,
}

impl Prewarm {
    pub(super) fn matches(&self, request: &Requested) -> bool {
        same(&self.cwd, &request.cwd)
            && request.session.is_none()
            && request.mcp.is_empty()
            && request.additional_dirs.is_empty()
            && request.system_prompt.is_none()
            && request.history.is_empty()
            && request.settings.is_none()
    }
}

impl Drop for Prewarm {
    fn drop(&mut self) {
        self.building.abort();
    }
}

/// The directory as this machine knows it, not as the client spelled it.
///
/// Everything a project owns — its mesh, its inboxes, its session files — is
/// keyed by the working directory, and a terminal keys it by the path the
/// kernel reports, which has no symlinks left in it. A client sends the path a
/// person typed. On macOS `/tmp` alone is enough to make those two differ, and
/// then an editor's session and a terminal's in the same directory are two
/// projects that cannot see each other.
pub(super) fn resolved(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    path.canonicalize().unwrap_or(path)
}

/// The same directory written two ways is one directory. A client hands over
/// the path a person typed; this process resolved its own at startup, and on
/// macOS a temporary directory alone is enough to make those differ.
pub(super) fn same(left: &Path, right: &Path) -> bool {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    left == right || resolve(left) == resolve(right)
}

pub(super) fn prewarm(acp: &Acp, cwd: PathBuf) -> Prewarm {
    let building = (acp.new_session)(Requested {
        cwd: cwd.clone(),
        session: None,
        mcp: Vec::new(),
        additional_dirs: Vec::new(),
        system_prompt: None,
        history: Vec::new(),
        settings: None,
    });
    Prewarm { cwd, building: tokio::task::spawn_local(building) }
}

pub(super) async fn open(
    acp: &Acp,
    server: &mut Connection,
    spare: &mut Option<Prewarm>,
    mut request: Requested,
) -> Result<String> {
    let cwd = request.cwd.clone();
    let history = std::mem::take(&mut request.history);
    let mcp = request.mcp.clone();
    let additional_dirs = request.additional_dirs.clone();
    let system_prompt = request.system_prompt.clone();
    // The one being built is only the right answer for its own directory; a
    // client that opens somewhere else gets a fresh agent, and the warm one is
    // left standing for whoever asks for the directory it belongs to.
    // A session being resumed is a different agent from the one warming up:
    // it carries a file, and taking the warm one would throw its history away.
    let (mut driver, events) = match spare.take_if(|warm| history.is_empty() && warm.matches(&request)) {
        Some(mut warm) => (&mut warm.building)
            .await
            .map_err(|error| elmee_core::Error::Config(format!("preparing an agent failed: {error}")))??,
        None => (acp.new_session)(request).await?,
    };
    if !history.is_empty() {
        let path = driver.agent.session_path().map(Path::to_path_buf);
        driver.agent.replace_session(elmee_host::session::fork_history(history, path).await?);
    }
    let model = driver.runtime.model();
    let reasoning_effort =
        driver.reasoning_effort().map_or_else(|| "auto".to_owned(), |effort| effort.as_str().to_owned());
    let driver = Rc::new(RefCell::new(driver));
    // The id a client is handed must be the thing it can hand back. A session
    // that has a file *is* that file: anything else means a client saves an id,
    // asks for it later, and is given an empty conversation under a name that
    // was never a session. An ephemeral run has no file, so it gets a name that
    // is honestly only good while this process lives.
    let id = driver
        .borrow()
        .agent
        .session_path()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| server.session_id());
    if server.sessions.borrow().contains_key(&id) {
        return Err(elmee_core::Error::Config("session is already open".into()));
    }
    // A session's runtime events are drained for as long as it is open. A
    // question from one of its children is refused rather than left waiting:
    // an editor has no place to put it, and a child that is never answered
    // hangs until its own watchdog gives up.
    let responder = driver.borrow().runtime.responder();
    let steerer = driver.borrow().runtime.steerer();
    let client = server.client.clone();
    let asking = id.clone();
    let sessions_for_wake = server.sessions.clone();
    let owner = Rc::downgrade(&driver);
    tokio::task::spawn_local(async move {
        let mut events = events;
        while let Some(event) = events.recv().await {
            if !sessions_for_wake
                .borrow()
                .get(&asking)
                .is_some_and(|open| std::rc::Weak::ptr_eq(&Rc::downgrade(&open.driver), &owner))
            {
                break;
            }
            // Something arrived for an agent nobody is prompting: a subagent
            // finished and reported itself, or a peer wrote to this project.
            // A terminal starts a turn for that, and an editor must not be the
            // frontend where a delegated result waits for someone to type.
            let (request, origin) = match event {
                RuntimeEvent::Wake => {
                    if let Taken::Ready(driver, cancel) = take(&sessions_for_wake, &asking, false) {
                        if driver.borrow().has_work() {
                            turn(
                                sessions_for_wake.clone(),
                                client.clone(),
                                asking.clone(),
                                driver,
                                cancel,
                                None,
                                None,
                            );
                        } else {
                            release(&sessions_for_wake, &asking);
                        }
                    }
                    continue;
                }
                // A child is a session of its own on this connection: said to
                // exist on its parent's, then heard in its own words, which
                // are already the protocol's.
                RuntimeEvent::Child { session, name, event } => {
                    let state = |state: &str| json!({ "sessionUpdate": "subagent_update", "sessionId": session, "state": { "state": state } });
                    match event {
                        _ if !client.shows.get().subagents => {}
                        elmee_host::ChildEvent::Opened => {
                            let mut opened = state("running");
                            opened["title"] = json!(name);
                            client.update(&asking, opened);
                        }
                        elmee_host::ChildEvent::Update(update) => client.update(&session, update),
                        elmee_host::ChildEvent::Ended => client.update(&asking, state("idle")),
                    }
                    continue;
                }
                RuntimeEvent::Interaction { request, origin } => (request, origin),
                RuntimeEvent::Snapshot { .. } => continue,
            };
            let elmee_core::InteractionRequest::Questions { id, questions } = &request;
            let (id, questions) = (id.clone(), questions.clone());
            let (client, responder, session) = (client.clone(), responder.clone(), asking.clone());
            // Each question waits on its own task: a person deciding must not
            // stop this session from reporting what it is doing meanwhile.
            tokio::task::spawn_local(async move {
                let answer = elicit(&client, &session, &questions, origin.is_none()).await;
                responder.respond(&id, answer);
            });
        }
    });
    // The picker opens on what this machine already knows — what its files
    // declare and what the endpoints said last time — and the endpoints are
    // asked behind the answer, unless they were asked a moment ago. A
    // catalogue is a network round trip each, and one that is switched off
    // must cost a list its entries, never a person the wait to start typing.
    let models = elmee_host::session::known_models(&cwd);
    let listing = {
        let (known, at) = (models.clone(), cwd.clone());
        let (sessions, client, session) = (server.sessions.clone(), server.client.clone(), id.clone());
        Listing(tokio::task::spawn_local(async move {
            let Some(Ok(models)) = elmee_host::session::fresh_models(&at).await else { return };
            if models.is_empty() || models == known {
                return;
            }
            if let Some(open) = sessions.borrow_mut().get_mut(&session) {
                open.models = models;
                client.update(
                    &session,
                    json!({ "sessionUpdate": "config_option_update", "configOptions": open.config_options() }),
                );
            }
        }))
    };
    server.sessions.borrow_mut().insert(
        id.clone(),
        Session {
            driver,
            cwd,
            steerer,
            mcp,
            additional_dirs,
            system_prompt,
            model,
            models,
            reasoning_effort,
            cancel: None,
            unprompted: false,
            queued: None,
            _listing: Some(listing),
        },
    );
    Ok(id)
}

pub(super) fn config_options(server: &Connection, session: &str) -> Value {
    server.sessions.borrow().get(session).map(Session::config_options).unwrap_or_default()
}

/// Where this session's mailbox is, so a parent elmee that started this
/// process as a subagent can reach it mid-task. Any other client ignores it.
pub(super) fn mesh_identity(server: &Connection, session: &str) -> Value {
    let mesh = server.sessions.borrow().get(session).map(|open| open.driver.borrow().runtime.mesh_identity());
    match mesh {
        Some(mesh) if !mesh.is_null() => json!({ "elmee": { "mesh": mesh } }),
        // An agent that holds no mailbox says nothing rather than naming none.
        _ => json!({}),
    }
}

/// What a person named the conversation, or the first thing asked of it,
/// which is how they tell one from another in a list.
pub(super) fn title_of(driver: &Driver) -> Option<String> {
    if let Some(title) = driver.agent.title() {
        return Some(title.to_owned());
    }
    let first = driver.agent.prompts().into_iter().next()?.text;
    Some(elmee_core::text::clip(first.trim(), 80))
}

/// The conversation as a tree: the prompts on the line it stands on, each
/// with whether the files can be put back to before it, and every line.
pub(super) fn history(driver: &Driver) -> Value {
    let prompts: Vec<Value> = driver
        .agent
        .prompts()
        .into_iter()
        .map(|asked| json!({ "id": asked.id, "text": asked.text, "checkpoint": asked.checkpoint.is_some() }))
        .collect();
    let branches: Vec<Value> = driver
        .agent
        .branches()
        .into_iter()
        .map(|line| json!({ "leaf": line.leaf, "prompts": line.prompts, "last": line.last, "active": line.active }))
        .collect();
    json!({ "prompts": prompts, "branches": branches })
}

/// What a client can pick from: the sessions open here, then the ones on disk
/// for the directory it asked about — or, asked about nothing, the one this
/// process started in. An ephemeral session has no file and is only listed
/// while it is open, which is the whole of what is true about it.
pub(super) fn listed(server: &Connection, cwd: Option<PathBuf>, started_in: &Path) -> Result<Vec<Value>> {
    let asked = cwd.clone().unwrap_or_else(|| started_in.to_path_buf());
    // What is on disk first, newest first: only conversations in which
    // something was asked, each with when it was last written to.
    let mut sessions: Vec<Value> = elmee_host::session::past(&asked, usize::MAX)?
        .into_iter()
        .filter(|past| past.prompts > 0)
        .map(|past| {
            json!({
                "sessionId": past.path.to_string_lossy(),
                "cwd": asked,
                "title": elmee_core::text::clip(past.subject.trim(), 80),
                "updatedAt": elmee_core::text::utc(past.modified),
                "_meta": { "elmee": { "prompts": past.prompts } },
            })
        })
        .collect();
    // Then what is open and has no file to speak for it: a session kept
    // nowhere, or one nothing has been said in yet.
    for (id, open) in server.sessions.borrow().iter() {
        if cwd.as_ref().is_none_or(|cwd| same(cwd, &open.cwd))
            && !sessions.iter().any(|kept| kept["sessionId"] == id.as_str())
        {
            let title = open.driver.try_borrow().ok().and_then(|driver| title_of(&driver));
            sessions.push(json!({ "sessionId": id, "cwd": open.cwd, "title": title }));
        }
    }
    Ok(sessions)
}

/// Hand a resumed conversation back to the client, so an editor reopening a
/// session shows what was said rather than an empty panel. The protocol asks
/// for this as ordinary updates, which is also the only shape a client already
/// knows how to draw.
pub(super) fn replay(server: &Connection, session: &str) {
    let driver = server.sessions.borrow().get(session).map(|open| open.driver.clone());
    let Some(driver) = driver else { return };
    let Ok(driver) = driver.try_borrow() else { return };
    for update in update::replay(driver.runtime.working_directory(), driver.agent.messages()) {
        server.client.update(session, update);
    }
    if let Some(title) = title_of(&driver) {
        server.client.update(session, json!({ "sessionUpdate": "session_info_update", "title": title }));
    }
}

/// The commands this agent offers, sent once when a session opens so an editor
/// can put them in its own menu instead of asking the user to know them.
pub(super) fn advertise_commands(server: &Connection, session: &str) {
    let driver = server.sessions.borrow().get(session).map(|open| open.driver.clone());
    let Some(driver) = driver else { return };
    let Ok(driver) = driver.try_borrow() else { return };
    let commands: Vec<Value> = driver
        .commands()
        .into_iter()
        .map(|(name, description)| json!({ "name": name, "description": description }))
        .collect();
    if !commands.is_empty() {
        server.client.update(
            session,
            json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands }),
        );
    }
}

/// What went wrong while the session was put together — a server the client
/// declared that would not start — is on stderr and in a report nobody asked
/// for. A person is told, where the client can show it.
pub(super) fn warn(server: &Connection, session: &str) {
    let driver = server.sessions.borrow().get(session).map(|open| open.driver.clone());
    let Some(driver) = driver.filter(|_| server.client.shows.get().notices) else { return };
    let Ok(driver) = driver.try_borrow() else { return };
    for wrong in driver.agent.extension_report().lines().filter_map(|line| line.strip_prefix("! ")) {
        server.client.notice(session, "warning", wrong);
    }
}
