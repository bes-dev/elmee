//! One turn at a time per session: who may take it, how it is streamed, how it
//! is stopped, and what it answers.

use super::*;

/// Run one turn, streaming it, and answer the request when it ends.
///
/// Spawned rather than awaited so the loop keeps reading: a second session
/// must be able to work while this one thinks, and `session/cancel` has to
/// arrive while the turn it cancels is still running.
///
/// A prompt that names a command the agent's plugins own runs that command
/// instead of reaching the model: an editor puts the advertised commands in a
/// menu, and what the menu sends is `/name` and its arguments as text.
pub(super) fn prompt(server: &mut Connection, id: Option<Value>, params: &Value) {
    let refuse = |code: i64, message: String| {
        if let Some(id) = id.clone() {
            let _ = server.client.out.send(failed(id, code, message));
        }
    };
    let Some(session) = params.get("sessionId").and_then(Value::as_str).map(str::to_owned) else {
        return refuse(INVALID_PARAMS, "session/prompt needs a sessionId".into());
    };
    if params["prompt"].as_array().is_none_or(|blocks| blocks.is_empty()) {
        return refuse(INVALID_PARAMS, "prompt must contain content".into());
    }
    match take(&server.sessions, &session, true) {
        Taken::Gone => refuse(INVALID_PARAMS, format!("no session {session}")),
        Taken::Busy => refuse(INVALID_REQUEST, "this session is already prompting".into()),
        // A turn nobody asked for is standing in the way. The person waited
        // for neither of them, so it is stopped and this takes its place —
        // and what woke it is already in the session, so nothing is lost.
        Taken::Interrupting => {
            let mut pool = server.sessions.borrow_mut();
            if let Some(open) = pool.get_mut(&session) {
                let cwd = open.cwd.clone();
                open.queued = Some((id, prompt_text(params, &cwd)));
                if let Some(cancel) = open.cancel.take() {
                    let _ = cancel.send(());
                }
            }
        }
        Taken::Ready(driver, cancel) => {
            let cwd = driver.borrow().runtime.working_directory().to_path_buf();
            let text = prompt_text(params, &cwd);
            let prompt = match driver.borrow_mut().command(&text) {
                None => text,
                Some(Err(error)) => {
                    release(&server.sessions, &session);
                    return refuse(INVALID_PARAMS, error);
                }
                Some(Ok(outcome)) => {
                    if !outcome.message.is_empty() {
                        server.client.update(&session, update::said("agent_message_chunk", &outcome.message));
                    }
                    match outcome.prompt {
                        Some(prompt) => prompt,
                        None => {
                            release(&server.sessions, &session);
                            if let Some(id) = id {
                                let _ = server.client.out.send(ok(id, json!({ "stopReason": "end_turn" })));
                            }
                            return;
                        }
                    }
                }
            };
            turn(server.sessions.clone(), server.client.clone(), session, driver, cancel, Some(prompt), id);
        }
    }
}

pub(super) enum Taken {
    Ready(Rc<RefCell<Driver>>, tokio::sync::oneshot::Receiver<()>),
    /// Busy with a turn a person asked for.
    Busy,
    /// Busy with one nobody asked for, which a person may take over.
    Interrupting,
    Gone,
}

/// Claim a session for one turn. The claim *is* the cancel channel: holding it
/// is what makes a second prompt a refusal rather than two turns sharing an
/// agent.
pub(super) fn take(sessions: &Sessions, session: &str, asked_for: bool) -> Taken {
    let mut pool = sessions.borrow_mut();
    let Some(open) = pool.get_mut(session) else { return Taken::Gone };
    match claim(open.cancel.is_some(), open.unprompted, asked_for) {
        Claim::Busy => Taken::Busy,
        Claim::Interrupting => Taken::Interrupting,
        Claim::Free => {
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            open.cancel = Some(cancel_tx);
            open.unprompted = !asked_for;
            Taken::Ready(open.driver.clone(), cancel_rx)
        }
    }
}

/// Give a session back, and say whether someone typed while it was taken.
pub(super) fn release(sessions: &Sessions, session: &str) -> Option<(Option<Value>, String)> {
    let mut pool = sessions.borrow_mut();
    let open = pool.get_mut(session)?;
    open.cancel = None;
    open.unprompted = false;
    open.queued.take()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Claim {
    Free,
    Busy,
    Interrupting,
}

/// Who may have a session, given what it is already doing.
///
/// One turn at a time, as the protocol says — with one exception that the
/// protocol does not have to know about: a turn nobody asked for, started
/// because a subagent reported or a peer wrote, gives way to a person. They
/// waited for neither of them, and what woke it is already in the session, so
/// stopping it loses nothing.
pub(super) const fn claim(busy: bool, unprompted: bool, asked_for: bool) -> Claim {
    match (busy, unprompted && asked_for) {
        (false, _) => Claim::Free,
        (true, true) => Claim::Interrupting,
        (true, false) => Claim::Busy,
    }
}

/// Run one turn, streaming it, and answer the request that asked for it.
///
/// Spawned rather than awaited so the loop keeps reading: another session must
/// be able to work while this one thinks, and `session/cancel` has to arrive
/// while the turn it cancels is still running.
///
/// `prompt` is `None` for a turn nobody asked for — a subagent's report
/// arriving between prompts. Those stream their updates like any other and
/// answer no one, which is exactly what a notification is for.
pub(super) fn turn(
    sessions: Sessions,
    client: Client,
    session: String,
    driver: Rc<RefCell<Driver>>,
    cancel: tokio::sync::oneshot::Receiver<()>,
    prompt: Option<String>,
    id: Option<Value>,
) {
    let name = session.clone();
    let prompted = prompt.is_some();
    let started = std::time::Instant::now();
    let (cwd, window, before) = {
        let driver = driver.borrow();
        (
            driver.runtime.working_directory().to_path_buf(),
            driver.agent.context_window(),
            driver.agent.tokens(),
        )
    };
    // The event closure must cross a `Send` bound, so it carries the sender
    // and nothing else: the rest of the client handle is this thread's.
    let updates = client.out.clone();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let recorded = requests.clone();
    // The summary being written, by the id the client was given for it.
    let compacting = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let (summarizing, shown) = (compacting.clone(), client.shows.get().compaction);
    tokio::task::spawn_local(async move {
        let mut on_event = move |event: elmee_core::AgentEvent| {
            let update = match &event {
                elmee_core::AgentEvent::ModelEnd { ok, duration_ms, metrics, .. } => {
                    recorded
                        .lock()
                        .unwrap()
                        .push(json!({ "ok": ok, "durationMs": duration_ms, "metrics": metrics }));
                    update::of(&cwd, window, &event)
                }
                elmee_core::AgentEvent::Compacting { text } if shown => {
                    let mut id = summarizing.lock().unwrap();
                    Some(update::summarizing(
                        id.get_or_insert_with(|| elmee_core::unique("compaction")),
                        text,
                    ))
                }
                elmee_core::AgentEvent::Compacted { summary, .. } if shown => {
                    let id = summarizing.lock().unwrap().take();
                    let id = id.unwrap_or_else(|| elmee_core::unique("compaction"));
                    Some(update::compaction(&id, "completed", Some(summary)))
                }
                _ => update::of(&cwd, window, &event),
            };
            if let Some(update) = update {
                let _ = updates.send(json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": { "sessionId": name, "update": update },
                }));
            }
        };
        let stopped = {
            // Held across the await deliberately: this borrow *is* the lock
            // that keeps one session to one turn, and the pool refuses a
            // second prompt before it is ever taken.
            #[allow(clippy::await_holding_refcell_ref)]
            let running = async { driver.borrow_mut().turn(prompt, &mut on_event).await };
            tokio::pin!(running);
            tokio::select! {
                outcome = &mut running => outcome.map(|_| None),
                // Dropping the turn future here is what stops the work: the
                // borrow it holds on the agent is released with it, and the
                // session is answerable again the moment this returns.
                _ = cancel => Ok(Some("cancelled")),
            }
        };
        // The guard ending a turn is the protocol's "too many requests", and
        // a client shows it differently from an answer.
        let stopped = stopped.map(|reason| {
            reason.unwrap_or(if driver.borrow().agent.stopped_by_guard() {
                "max_turn_requests"
            } else {
                "end_turn"
            })
        });
        // A summary that was being written when the turn ended never arrived.
        if let Some(id) = compacting.lock().unwrap().take() {
            client.update(&session, update::compaction(&id, "failed", None));
        }
        // A turn nobody asked for has nobody to answer, so what went wrong in
        // it is said where a person can see it.
        if let (None, Err(error)) = (&id, &stopped) {
            client.notice(&session, "error", &error.to_string());
        }
        // The first prompt names the conversation.
        let current = sessions.borrow().get(&session).is_some_and(|open| Rc::ptr_eq(&open.driver, &driver));
        if current
            && prompted
            && let Ok(driver) = driver.try_borrow()
            && driver.agent.prompts().len() == 1
            && let Some(title) = title_of(&driver)
        {
            client.update(&session, json!({ "sessionUpdate": "session_info_update", "title": title }));
        }
        let waiting = if current {
            client.end(&session);
            release(&sessions, &session)
        } else {
            None
        };
        {
            // What the turn cost rides in `_meta`, where the protocol puts
            // what it does not model: a parent that delegated this turn
            // accounts for it, and an editor ignores it.
            let after = driver.borrow().agent.tokens();
            let spent = json!({ "elmee": {
                "inputTokens": after.0.saturating_sub(before.0),
                "outputTokens": after.1.saturating_sub(before.1),
                "durationMs": started.elapsed().as_millis() as u64,
                "requests": *requests.lock().unwrap(),
            } });
            if let Some(id) = id {
                let _ = client.out.send(match stopped {
                    Ok(reason) => ok(id, json!({ "stopReason": reason, "_meta": spent })),
                    Err(error @ elmee_core::Error::SignIn(_)) => failed(id, AUTH_REQUIRED, error),
                    Err(error) => failed(id, INTERNAL_ERROR, error),
                });
            }
        }
        // Someone typed while this was running and is still waiting.
        if let Some((id, text)) = waiting
            && let Taken::Ready(driver, cancel) = take(&sessions, &session, true)
        {
            turn(sessions.clone(), client.clone(), session, driver, cancel, Some(text), id);
        }
    });
}

/// Go back to before a prompt, off the reader's thread: putting files back is
/// disk work, and the session is claimed for it exactly as a turn claims it.
pub(super) fn rewind(server: &Connection, id: Option<Value>, params: &Value) {
    use elmee_host::session::Restore;
    let refuse = |code: i64, message: &str| {
        if let Some(id) = id.clone() {
            let _ = server.client.out.send(failed(id, code, message));
        }
    };
    let restore = match params["restore"].as_str() {
        None | Some("everything") => Restore::Everything,
        Some("conversation") => Restore::Conversation,
        Some("files") => Restore::Files,
        Some(_) => return refuse(INVALID_PARAMS, "restore is everything, conversation or files"),
    };
    let (Some(session), Some(prompt)) = (params["sessionId"].as_str(), params["promptId"].as_u64()) else {
        return refuse(INVALID_PARAMS, "_session/rewind needs a sessionId and a promptId");
    };
    let driver = match take(&server.sessions, session, true) {
        Taken::Ready(driver, _) => driver,
        Taken::Gone => return refuse(INVALID_PARAMS, "unknown session"),
        Taken::Busy | Taken::Interrupting => return refuse(INVALID_REQUEST, "this session is busy"),
    };
    let (sessions, client, session) = (server.sessions.clone(), server.client.clone(), session.to_owned());
    tokio::task::spawn_local(async move {
        // Held across the await as a turn holds it: the claim above is what
        // keeps everyone else out.
        #[allow(clippy::await_holding_refcell_ref)]
        let rewinding = async { driver.borrow_mut().rewind(prompt, restore).await };
        let outcome = rewinding.await;
        if sessions.borrow().get(&session).is_some_and(|open| Rc::ptr_eq(&open.driver, &driver)) {
            release(&sessions, &session);
        }
        let answer = match outcome {
            Ok(prompt) => {
                if prompt.is_some() {
                    client.update(&session, json!({ "sessionUpdate": "plan", "entries": [] }));
                }
                let mut tree = history(&driver.borrow());
                tree["prompt"] = json!(prompt);
                Ok(tree)
            }
            Err(reason) => Err(reason),
        };
        if let Some(id) = id {
            let _ = client.out.send(match answer {
                Ok(tree) => ok(id, tree),
                Err(reason) => failed(id, INVALID_PARAMS, reason),
            });
        }
    });
}
