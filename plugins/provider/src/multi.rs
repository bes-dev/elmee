//! All registry providers behind one `Provider`, so `/model` can switch
//! across providers mid-session. Specs use pi's `provider/model` notation
//! against the names declared in models.json (or the built-in defaults).

use std::collections::BTreeMap;

use super::{Anthropic, AssistantTurn, Chunk, Credential, OpenAiCompatible, Provider, Responses};
use crate::{
    Error, Result,
    registry::{Api, ProviderEntry, Subscription, resolve, window_for},
};
use elmee_core::message::Message;
use elmee_core::provider::ReasoningEffort;

enum Adapter {
    OpenAi(OpenAiCompatible),
    Anthropic(Anthropic),
    Responses(Responses),
}

impl Adapter {
    fn for_entry(entry: &ProviderEntry, context: std::rc::Rc<crate::SessionContext>) -> Self {
        match entry.api {
            Api::OpenAiCompletions => Adapter::OpenAi(OpenAiCompatible::new(
                entry.base_url.clone(),
                entry.api_key.clone(),
                String::new(),
            )),
            Api::AnthropicMessages => Adapter::Anthropic(Anthropic::new(
                entry.base_url.clone(),
                entry.api_key.clone(),
                String::new(),
            )),
            Api::OpenAiResponses => Adapter::Responses(
                Responses::new(
                    entry.base_url.clone(),
                    match entry.subscription {
                        Some(Subscription::Codex) => Credential::Subscription,
                        _ => Credential::Key(entry.api_key.clone()),
                    },
                    String::new(),
                    entry.client_version.clone(),
                )
                .with_context(context),
            ),
        }
    }

    fn set_model(&mut self, model: String) {
        match self {
            Adapter::OpenAi(adapter) => adapter.set_model(model),
            Adapter::Anthropic(adapter) => adapter.set_model(model),
            Adapter::Responses(adapter) => adapter.set_model(model),
        }
    }

    fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        match self {
            Adapter::OpenAi(adapter) => adapter.set_reasoning_effort(effort),
            Adapter::Anthropic(adapter) => adapter.set_reasoning_effort(effort),
            Adapter::Responses(adapter) => adapter.set_reasoning_effort(effort),
        }
    }

    async fn list_models(&self) -> Result<Vec<crate::Model>> {
        match self {
            Adapter::OpenAi(adapter) => adapter.list_models().await,
            Adapter::Anthropic(adapter) => adapter.list_models().await,
            Adapter::Responses(adapter) => adapter.list_models().await,
        }
    }
}

pub struct MultiProvider {
    context: std::rc::Rc<crate::SessionContext>,
    entries: Vec<ProviderEntry>,
    adapters: Vec<Adapter>,
    active: usize,
    /// Declared context windows keyed by `provider/model`, kept so a mid-session
    /// `/model` switch can re-derive the window the same way startup did.
    windows: BTreeMap<String, usize>,
    /// `provider/model` of the active adapter, the key windows are looked up by.
    spec: String,
    context_window: Option<usize>,
    reasoning_effort: Option<ReasoningEffort>,
}

impl MultiProvider {
    pub fn new(entries: Vec<ProviderEntry>, active: usize, model: String) -> Self {
        let context = std::rc::Rc::new(crate::SessionContext::default());
        let mut adapters: Vec<Adapter> =
            entries.iter().map(|entry| Adapter::for_entry(entry, context.clone())).collect();
        let spec = spec(&entries[active].name, &model);
        adapters[active].set_model(model);
        Self {
            context,
            entries,
            adapters,
            active,
            windows: BTreeMap::new(),
            spec,
            context_window: None,
            reasoning_effort: None,
        }
    }

    pub fn context(&self) -> std::rc::Rc<crate::SessionContext> {
        self.context.clone()
    }

    /// Carry the registry's window table, and size the active adapter's reply
    /// budget from it. Every later [`Provider::set_model`] re-derives.
    /// Open the connection to the active endpoint before there is anything to
    /// ask of it. Resolving the name, connecting and the TLS handshake are the
    /// slow part of a first request, and where a person is still typing — or
    /// an editor has only opened a session — they can happen meanwhile: the
    /// shared client keeps the connection for the request that follows.
    ///
    /// Nothing is authenticated and nothing depends on the answer; only an
    /// endpoint far enough away for the handshake to cost anything is called.
    pub fn warm(&self) {
        let url = self.entries[self.active].base_url.clone();
        if url.starts_with("https://") {
            tokio::spawn(async move {
                let _ = crate::http_client().head(url).send().await;
            });
        }
    }

    pub fn with_windows(mut self, windows: BTreeMap<String, usize>) -> Self {
        self.windows = windows;
        self.apply_window();
        self
    }

    /// The active model's context window, re-derived on every model switch —
    /// what a frontend reads after `/model` to recompute its compaction budget.
    pub fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    /// Look the active spec's window up again and re-size the adapter's reply
    /// budget, so `max_tokens` follows a switch instead of staying at whatever
    /// the previous model earned.
    fn apply_window(&mut self) {
        self.context_window = window_for(&self.windows, &self.spec);
        if let Adapter::Anthropic(adapter) = &mut self.adapters[self.active] {
            adapter.set_context_window(self.context_window);
        }
    }
}

impl elmee_core::Plugin for MultiProvider {
    fn id(&self) -> &'static str {
        "provider"
    }

    fn declare(self, plugins: &mut elmee_core::Registrar) -> Result<()> {
        plugins.provide_service(self.context.clone());
        plugins.provide_service(std::rc::Rc::new(elmee_core::ModelService::new(self)));
        Ok(())
    }
}

/// Canonical display spec for a provider/model pair.
pub fn spec(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// Whether asking this endpoint what it serves is worth a request.
///
/// Two questions, and they are different. *Can* we ask: a subscription counts,
/// which is why this no longer looks only for an API key — the one endpoint
/// that keeps a real catalogue was the one never asked for it, and a list
/// compiled into the binary was the only answer there could be, however stale.
/// *Should* we ask: declaring models is how a person says which ones they
/// want, so a provider with a list of its own is taken at its word, and one
/// with none is asked. `discover` overrides that reading in either direction —
/// for a gateway serving hundreds beside three that matter, or for a curated
/// list that should still pick up what appeared this week.
fn has_live_catalogue(entry: &ProviderEntry) -> bool {
    let reachable =
        entry.api == Api::OpenAiCompletions || entry.api_key.is_some() || entry.subscription.is_some();
    reachable && entry.discover.unwrap_or(entry.declared.is_empty())
}

#[elmee_core::async_trait(?Send)]
impl Provider for MultiProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        const ATTEMPTS: usize = 3;
        let started = std::time::Instant::now();
        let mut first_text = None;
        for attempt in 0..ATTEMPTS {
            let mut emitted = false;
            let offset = started.elapsed().as_millis() as u64;
            let result = {
                let mut observe = |chunk: Chunk<'_>| {
                    emitted = true;
                    if matches!(chunk, Chunk::Text(text) if !text.is_empty()) {
                        first_text.get_or_insert(started.elapsed().as_millis() as u64);
                    }
                    on_text(chunk);
                };
                match &self.adapters[self.active] {
                    Adapter::OpenAi(adapter) => adapter.complete(messages, tools, &mut observe).await,
                    Adapter::Anthropic(adapter) => adapter.complete(messages, tools, &mut observe).await,
                    Adapter::Responses(adapter) => adapter.complete(messages, tools, &mut observe).await,
                }
            };
            match result {
                Ok(mut turn) => {
                    let metrics = turn.metrics.get_or_insert_with(|| elmee_core::provider::RequestMetrics {
                        attempts: 1,
                        ..Default::default()
                    });
                    metrics.attempts += attempt as u32;
                    metrics.total_ms = Some(started.elapsed().as_millis() as u64);
                    metrics.first_text_ms = first_text;
                    metrics.first_tool_ms = metrics.first_tool_ms.map(|ms| ms + offset);
                    metrics.headers_ms = metrics.headers_ms.map(|ms| ms + offset);
                    return Ok(turn);
                }
                Err(error) if emitted => {
                    return Err(Error::Provider(format!(
                        "provider failed after emitting partial output; request was not retried: {error}"
                    )));
                }
                Err(error) if attempt + 1 < ATTEMPTS && retryable(&error) => {
                    tokio::time::sleep(backoff(&error, attempt)).await;
                }
                Err(error) => return Err(self.explain_refusal(error)),
            }
        }
        unreachable!("the retry loop always returns")
    }

    fn set_model(&mut self, model_spec: String) {
        let (index, model) = resolve(&self.entries, &model_spec);
        self.spec = spec(&self.entries[index].name, &model);
        self.adapters[index].set_model(model);
        self.active = index;
        self.apply_window();
    }

    fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.reasoning_effort = effort;
        for adapter in &mut self.adapters {
            adapter.set_reasoning_effort(effort);
        }
    }

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }

    fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    /// Every declared model plus whatever each reachable endpoint reports,
    /// deduplicated, in `provider/model` specs. An unsigned Anthropic endpoint
    /// is skipped because its inevitable 401 tells us nothing.
    ///
    /// What the endpoints said about their windows is written down on the way
    /// past. Startup must not need the network — a coding agent that cannot
    /// begin on a plane is one nobody keeps — so the number a catalogue knows
    /// has to be remembered the one time it is asked for, and read from disk
    /// every time after.
    async fn list_models(&self) -> Result<Vec<crate::Model>> {
        let mut all: Vec<crate::Model> = Vec::new();
        // Every endpoint is asked at once and given the same short while to
        // answer: one that is switched off costs a picker its own entries, not
        // everyone else's and not ten seconds of a connect timeout each.
        let asked = self.entries.iter().zip(&self.adapters).map(|(entry, adapter)| async move {
            if !has_live_catalogue(entry) {
                return Ok(Vec::new());
            }
            match tokio::time::timeout(CATALOGUE_WAIT, adapter.list_models()).await {
                Ok(answer) => answer,
                Err(_) => Err(Error::Http(format!("no answer in {}s", CATALOGUE_WAIT.as_secs()))),
            }
        });
        let answers = futures_util::future::join_all(asked).await;
        let mut silent = Vec::new();
        for (entry, answer) in self.entries.iter().zip(answers) {
            let mut models: Vec<crate::Model> =
                entry.declared.iter().map(|model| crate::Model::new(model, None)).collect();
            match answer {
                Ok(live) => {
                    for model in live {
                        match models.iter_mut().find(|known| known.id == model.id) {
                            Some(known) => {
                                known.context_window = known.context_window.or(model.context_window)
                            }
                            None => models.push(model),
                        }
                    }
                }
                // The declared list still answers; a silent empty catalogue
                // reads as "no models" when the truth is "not authorized".
                //
                // Only where the diagnostic cannot land on a screen someone
                // else is drawing: this runs while the model picker holds
                // the terminal in raw mode, and a line written underneath it
                // interleaves with its own output character by character.
                Err(error) => {
                    silent.push(entry.name.as_str());
                    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                        eprintln!("warning: model catalogue from {} unavailable: {error}", entry.name);
                    }
                }
            }
            all.extend(models.into_iter().map(|model| crate::Model {
                id: spec(&entry.name, &model.id),
                context_window:
                    model.context_window.or_else(|| window_for(&self.windows, &spec(&entry.name, &model.id))),
            }));
        }
        crate::registry::remember_models(&all, &silent);
        Ok(all)
    }
}

/// How long an endpoint has to say what it serves before a picker goes on
/// without it.
const CATALOGUE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Wait the way the provider asked, or double the guess if it did not say.
///
/// Capped, because a server is free to answer with an hour and a caller is not
/// obliged to hold a terminal open for it — past the cap the failure is worth
/// more than the wait.
fn backoff(error: &Error, attempt: usize) -> std::time::Duration {
    const CAP: u64 = 60;
    match error.retry_after() {
        Some(seconds) => std::time::Duration::from_secs(seconds.min(CAP)),
        None => std::time::Duration::from_millis(250 * (1 << attempt)),
    }
}

/// Worth trying again: the connection failed, or the server said it was busy
/// rather than that the request was wrong.
impl MultiProvider {
    /// Say what to do about a provider that refuses the model.
    ///
    /// A subscription endpoint serves what it chooses to the client that is
    /// asking, and answers anything else with a sentence about its own product
    /// rather than about what would work. Naming the models this provider does
    /// declare turns a dead end into a choice, and a person who has just been
    /// told their model is "not supported" is entitled to that much.
    fn explain_refusal(&self, error: Error) -> Error {
        let Error::Refused { status: 400, body, .. } = &error else { return error };
        if !body.contains("not supported") && !body.contains("does not exist") {
            return error;
        }
        let entry = &self.entries[self.active];
        if entry.declared.is_empty() {
            return error;
        }
        let declared: Vec<String> =
            entry.declared.iter().map(|model| format!("{}/{model}", entry.name)).collect();
        Error::Provider(format!(
            "{error}\nThis provider declares: {}.\nA subscription answers an unusable sign-in by \
             talking about the model, so if those are refused too, sign in again before believing \
             it.",
            declared.join(", ")
        ))
    }
}

fn retryable(error: &Error) -> bool {
    match error {
        // A body that broke is as transient as a connection that never
        // opened — a pooled socket the far end had already closed, a proxy
        // dropping a stream — and retrying costs nothing here, because the
        // caller only reaches this when nothing has been emitted yet. There is
        // no output to duplicate, which is the only reason not to try again.
        // The typed reqwest error stays at the HTTP boundary; what crosses it
        // is reqwest's own prose, which names each of these conditions.
        Error::Http(error) => ["connect", "timed out", "request or response body", "decod"]
            .iter()
            .any(|needle| error.contains(needle)),
        Error::Refused { status, .. } => matches!(status, 500 | 502 | 503 | 504),
        Error::Exhausted { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {

    /// The subscription is the one endpoint that publishes a real, changing
    /// catalogue, and it was the one endpoint never asked — it carries OAuth
    /// credentials rather than an API key, and an API key was all this looked
    /// for. The list compiled into the binary was then the only answer possible.
    #[test]
    fn a_subscription_is_asked_what_it_serves() {
        let entry = |api_key, subscription| ProviderEntry {
            name: "p".into(),
            api: Api::OpenAiResponses,
            base_url: String::new(),
            api_key,
            subscription,
            declared: Vec::new(),
            client_version: None,
            discover: None,
        };
        assert!(has_live_catalogue(&entry(None, Some(crate::registry::Subscription::Codex))));
        assert!(has_live_catalogue(&entry(Some("k".into()), None)));
        // Nothing to authenticate with is still nothing to ask with.
        assert!(!has_live_catalogue(&entry(None, None)));
    }

    /// Listing models is a decision about which ones are wanted, and a gateway
    /// serving hundreds beside the three that matter would otherwise bury them.
    #[test]
    fn a_declared_list_is_taken_at_its_word_unless_discovery_is_asked_for() {
        let entry = |declared: &[&str], discover| ProviderEntry {
            name: "p".into(),
            api: Api::OpenAiCompletions,
            base_url: String::new(),
            api_key: None,
            subscription: None,
            declared: declared.iter().map(|model| (*model).to_owned()).collect(),
            client_version: None,
            discover,
        };
        // Declaring nothing is how a local runtime whose models change weekly
        // stays usable at all.
        assert!(has_live_catalogue(&entry(&[], None)));
        assert!(!has_live_catalogue(&entry(&["qwen"], None)), "a curated list is not a starting point");
        // And either reading can be said outright.
        assert!(has_live_catalogue(&entry(&["qwen"], Some(true))));
        assert!(!has_live_catalogue(&entry(&[], Some(false))));
    }
    use std::io::{Read, Write};

    use super::*;

    fn entry(name: &str, api: Api, base_url: String, declared: &[&str]) -> ProviderEntry {
        ProviderEntry {
            name: name.into(),
            api,
            base_url,
            api_key: Some("k".into()),
            subscription: None,
            declared: declared.iter().map(|model| (*model).to_owned()).collect(),
            client_version: None,
            discover: None,
        }
    }

    /// `/model` used to change only the model: the window — and every budget
    /// derived from it — stayed at whatever the startup model earned.
    #[test]
    fn a_model_switch_rederives_the_context_window() {
        let entries = vec![entry("p", Api::OpenAiCompletions, String::new(), &["m1", "m2"])];
        let mut provider = MultiProvider::new(entries, 0, "m1".into())
            .with_windows(BTreeMap::from([("p/m1".to_owned(), 272_000), ("p/m2".to_owned(), 8_000)]));
        assert_eq!(provider.context_window(), Some(272_000));
        provider.set_model("p/m2".into());
        assert_eq!(provider.context_window(), Some(8_000));
        provider.set_model("p/uncatalogued".into());
        assert_eq!(provider.context_window(), None, "an unknown model has no window to claim");
    }

    /// The Anthropic adapter asks for `max_tokens` sized to its model's window;
    /// after a switch it must ask for the new model's budget, not the old one's.
    #[tokio::test]
    async fn a_switch_to_anthropic_resizes_its_reply_budget() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 16_384];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]).into_owned();
            let body = r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            request
        });

        let entries = vec![
            entry("p", Api::OpenAiCompletions, String::new(), &["m1"]),
            entry("a", Api::AnthropicMessages, format!("http://{address}"), &["m2"]),
        ];
        let mut provider = MultiProvider::new(entries, 0, "m1".into())
            .with_windows(BTreeMap::from([("a/m2".to_owned(), 8_000)]));
        provider.set_model("a/m2".into());
        provider.complete(&[Message::user("hi")], &[], &mut |_| {}).await.unwrap();

        let request = server.join().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["max_tokens"].as_u64(), Some(elmee_core::output_budget(8_000) as u64), "{body}");
    }

    #[test]
    fn a_provider_that_says_when_to_come_back_is_believed_up_to_a_point() {
        let asked =
            |seconds| backoff(&Error::Exhausted { retry_after: Some(seconds), message: String::new() }, 0);
        assert_eq!(asked(30).as_secs(), 30);
        assert_eq!(asked(9999).as_secs(), 60, "capped, not obeyed");
        // Nothing said: the doubling guess, which is all there ever was.
        let silent = Error::Refused { status: 503, retry_after: None, body: String::new() };
        assert_eq!(backoff(&silent, 0).as_millis(), 250);
        assert_eq!(backoff(&silent, 2).as_millis(), 1000);
    }

    #[test]
    fn retries_only_transient_provider_statuses() {
        let refused = |status| Error::Refused { status, retry_after: None, body: String::new() };
        assert!(retryable(&refused(503)));
        assert!(retryable(&Error::Exhausted { retry_after: None, message: String::new() }));
        assert!(!retryable(&refused(401)), "a request that was wrong stays wrong");
        assert!(!retryable(&Error::Provider("invalid response shape".into())));
    }

    #[tokio::test]
    async fn transient_failures_are_retried_before_any_output() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let _ = stream.read(&mut request);
                if attempt < 2 {
                    stream
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                        .unwrap();
                } else {
                    let body = r#"{"choices":[{"message":{"role":"assistant","content":"recovered"}}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
        });
        let entries = vec![ProviderEntry {
            name: "fault".into(),
            api: Api::OpenAiCompletions,
            base_url: format!("http://{address}"),
            api_key: None,
            subscription: None,
            declared: vec!["model".into()],
            client_version: None,
            discover: None,
        }];
        let provider = MultiProvider::new(entries, 0, "model".into());
        let turn = provider.complete(&[], &[], &mut |_| {}).await.unwrap();
        assert_eq!(turn.content.as_deref(), Some("recovered"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_broken_stream_is_not_retried_after_partial_output() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: not-json\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(700);
            let mut extra = 0;
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        extra += 1;
                        let _ = stream
                            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            extra
        });
        let entries = vec![ProviderEntry {
            name: "fault".into(),
            api: Api::OpenAiCompletions,
            base_url: format!("http://{address}"),
            api_key: None,
            subscription: None,
            declared: vec!["model".into()],
            client_version: None,
            discover: None,
        }];
        let provider = MultiProvider::new(entries, 0, "model".into());
        let mut streamed = String::new();
        let error = provider
            .complete(&[], &[], &mut |chunk| {
                if let Chunk::Text(text) = chunk {
                    streamed.push_str(text)
                }
            })
            .await
            .unwrap_err();
        assert_eq!(streamed, "partial");
        assert!(error.to_string().contains("partial output"), "{error}");
        assert_eq!(server.join().unwrap(), 0, "partial output must make the request non-retryable");
    }
}
