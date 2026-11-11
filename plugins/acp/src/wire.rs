//! The JSON-RPC wire as this side uses it: the way back to the client, and
//! the shapes of an answer and a refusal.

use super::*;

/// The client, as the agent side reaches it: somewhere to send notifications,
/// and somewhere to put a question and wait for the answer.
///
/// Cloneable and shared, because the thing that needs to ask is a session's own
/// background task rather than the request loop.
#[derive(Clone)]
pub(super) struct Client {
    pub(super) out: mpsc::UnboundedSender<Value>,
    /// Requests this agent has put to the client, waiting for their answers.
    /// A JSON-RPC id is only ever ours or theirs, never both, so ours are
    /// numbered from a counter nobody else touches.
    pub(super) pending: Rc<RefCell<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>,
    pub(super) next_request: Rc<Cell<u64>>,
    pub(super) shows: Rc<Cell<Shows>>,
    pub(super) scopes: Rc<RefCell<HashMap<u64, (String, bool)>>>,
}

/// What the client said it can show. Each of these is something the protocol
/// forbids sending to a client that did not ask for it.
#[derive(Clone, Copy, Default)]
pub(super) struct Shows {
    /// A form put to a person. Without it a question is answered as
    /// cancelled rather than left hanging.
    pub(super) forms: bool,
    pub(super) compaction: bool,
    pub(super) notices: bool,
    pub(super) subagents: bool,
}

/// An MCP server the client itself is, asked over this connection: an editor
/// offering its own tools has no process to start and no port to open.
#[derive(Clone)]
pub struct Served {
    pub(super) id: String,
    pub(super) client: Client,
}

impl PartialEq for Served {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Served {}

impl Served {
    /// Put one MCP request to the client and hand back what its server said.
    pub fn ask(&self, method: String, params: Value) -> PluginFuture<'static, Result<Value>> {
        let (client, server) = (self.client.clone(), self.id.clone());
        Box::pin(async move {
            let request = json!({
                "serverId": server,
                "requestId": elmee_core::unique("mcp"),
                "method": method,
                "params": params,
            });
            let answer = client.ask("mcp/message", request, false).await.unwrap_or_default();
            answer.get("result").cloned().ok_or_else(|| {
                elmee_core::Error::Provider(format!(
                    "MCP {method}: {}",
                    answer.get("error").unwrap_or(&answer)
                ))
            })
        })
    }
}

impl Client {
    pub(super) fn new(out: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            out,
            pending: Rc::default(),
            next_request: Rc::default(),
            shows: Rc::default(),
            scopes: Rc::default(),
        }
    }

    /// Something a person should know that is not part of the conversation.
    pub(super) fn notice(&self, session: &str, severity: &str, title: &str) {
        if self.shows.get().notices && !title.is_empty() {
            self.update(session, json!({ "sessionUpdate": "notice", "severity": severity, "title": title }));
        }
    }

    /// A turn ended: the questions it was waiting on have nobody to answer
    /// to. A child's question outlives the turn that happened to be running.
    pub(super) fn end(&self, session: &str) {
        self.cancel_scopes(session, false);
    }

    /// The session is gone, and every question put on its behalf with it.
    pub(super) fn close(&self, session: &str) {
        self.cancel_scopes(session, true);
    }
    fn cancel_scopes(&self, session: &str, all: bool) {
        let ids: Vec<u64> = self
            .scopes
            .borrow()
            .iter()
            .filter_map(|(id, (scope, foreground))| (scope == session && (all || *foreground)).then_some(*id))
            .collect();
        for id in ids {
            self.scopes.borrow_mut().remove(&id);
            self.pending.borrow_mut().remove(&id);
        }
    }
    pub(super) fn notify(&self, method: &str, params: Value) {
        let _ = self.out.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub(super) fn update(&self, session: &str, update: Value) {
        self.notify("session/update", json!({ "sessionId": session, "update": update }));
    }

    /// Put a request to the client and wait for its answer. `None` if the
    /// client answered with an error or went away.
    pub(super) async fn ask(&self, method: &str, params: Value, foreground: bool) -> Option<Value> {
        let id = self.next_request.get() + 1;
        self.next_request.set(id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.borrow_mut().insert(id, tx);
        if method == "elicitation/create"
            && let Some(session) =
                params.pointer("/mode/form/scope/sessionId").and_then(Value::as_str).map(str::to_owned)
        {
            self.scopes.borrow_mut().insert(id, (session, foreground));
        }
        let sent = self
            .out
            .send(json!({ "jsonrpc": "2.0", "id": request_id(id), "method": method, "params": params }));
        if sent.is_err() {
            self.pending.borrow_mut().remove(&id);
            self.scopes.borrow_mut().remove(&id);
            return None;
        }
        let scoped = self.scopes.borrow().contains_key(&id);
        let result = rx.await.ok();
        let owned = self.scopes.borrow_mut().remove(&id).is_some();
        if scoped && !owned { None } else { result }
    }

    /// Deliver an answer to whatever asked for it. Ids we did not issue are
    /// ignored: a client is free to talk to itself.
    pub(super) fn answered(&self, id: &Value, message: &Value) {
        let Some(id) = id.as_str().and_then(|id| id.strip_prefix(REQUEST_PREFIX)) else { return };
        let Ok(id) = id.parse::<u64>() else { return };
        if let Some(waiter) = self.pending.borrow_mut().remove(&id) {
            let _ = waiter.send(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// Our own request ids are strings with a prefix, so a client that echoes
/// numbers of its own can never collide with one of ours.
pub(super) const REQUEST_PREFIX: &str = "elmee-req-";

pub(super) fn request_id(id: u64) -> String {
    format!("{REQUEST_PREFIX}{id}")
}

pub(super) fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub(super) fn failed(id: Value, code: i64, message: impl std::fmt::Display) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.to_string() } })
}

/// JSON-RPC's own codes, plus the one the protocol adds for "the client has
/// not said hello yet".
pub(super) const INVALID_REQUEST: i64 = -32600;

pub(super) const METHOD_NOT_FOUND: i64 = -32601;

pub(super) const INVALID_PARAMS: i64 = -32602;

pub(super) const INTERNAL_ERROR: i64 = -32603;
/// The protocol's own code for "sign in first": a client that sees it offers
/// the methods `initialize` advertised.
pub(super) const AUTH_REQUIRED: i64 = -32000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ending_a_parent_turn_keeps_background_child_questions_until_session_close() {
        let (out, _) = mpsc::unbounded_channel();
        let client = Client::new(out);
        let (foreground, mut foreground_answer) = tokio::sync::oneshot::channel();
        let (child, mut child_answer) = tokio::sync::oneshot::channel();
        client.pending.borrow_mut().extend([(1, foreground), (2, child)]);
        client.scopes.borrow_mut().extend([(1, ("parent".into(), true)), (2, ("parent".into(), false))]);
        client.end("parent");
        assert!(matches!(
            foreground_answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(child_answer.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
        client.close("parent");
        assert!(matches!(child_answer.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)));
    }
}
