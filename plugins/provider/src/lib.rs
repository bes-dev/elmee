#![forbid(unsafe_code)]

//! Model-provider adapters and model selection for Elmee.
//!
//! Adapters translate core's provider-neutral messages and streaming events to
//! OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages. The model
//! registry resolves names, endpoints, context windows and credential sources;
//! [`MultiProvider`] installs the selected implementation as core's typed model
//! service. Wire protocols and authentication therefore evolve without changes
//! to the agent loop.

mod anthropic;
mod context;
mod multi;
mod openai;
mod responses;

pub mod auth;
pub mod registry;

pub use anthropic::Anthropic;
pub use context::SessionContext;
pub use multi::{MultiProvider, spec};
pub use openai::OpenAiCompatible;
pub use responses::{Credential, Responses};

pub use elmee_core::provider::{AssistantTurn, Chunk, Model, Provider};
pub use elmee_core::{Error, Result};

trait OrHttp<T> {
    fn or_http(self) -> Result<T>;
}

impl<T> OrHttp<T> for std::result::Result<T, reqwest::Error> {
    fn or_http(self) -> Result<T> {
        self.map_err(|error| Error::Http(error.to_string()))
    }
}

/// Feed each newline-delimited event of a streaming response to `handle`, and
/// stop when it says the stream is finished.
///
/// Both adapters read Server-Sent Events; only what a line *means* differs, so
/// only that belongs to them. The framing — accumulating chunks, splitting on
/// newlines, keeping the partial tail, and refusing a line that never ends —
/// was written out twice, identically, which is one place too many for the
/// code that decides how much memory an unbounded response may take.
///
/// Stopping is the handler's to say, and it matters more than it looks. This
/// used to read until the *connection* ended, long after the terminal event had
/// arrived and the turn was fully assembled — so an HTTP/2 reset, a proxy
/// dropping the socket, or a chunked body with no terminating chunk turned a
/// complete answer into "error decoding response body", and the retry logic
/// then refused to retry because output had already been emitted. What happens
/// to a connection after the model has finished speaking is not the model's
/// answer.
pub(crate) async fn read_events<H>(mut response: reqwest::Response, mut handle: H) -> Result<()>
where
    H: FnMut(&[u8]) -> Result<std::ops::ControlFlow<()>>,
{
    /// A response that never sends a newline must still not grow forever.
    const MAX_EVENT: usize = 16 * 1024 * 1024;
    let mut bytes = Vec::with_capacity(8192);
    while let Some(chunk) = response.chunk().await.or_http()? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_EVENT {
            return Err(crate::Error::Provider("SSE event exceeded 16 MiB without a line break".into()));
        }
        let mut consumed = 0;
        while let Some(end) = bytes[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + end;
            if handle(&bytes[consumed..end])?.is_break() {
                return Ok(());
            }
            consumed = end + 1;
        }
        if consumed > 0 {
            bytes.drain(..consumed);
        }
    }
    // A trailing terminal line is valid SSE even without its final newline.
    if !bytes.is_empty() && handle(&bytes)?.is_break() {
        return Ok(());
    }
    Err(Error::Provider("provider stream ended before its terminal event".into()))
}

/// A response worth reading, and whether it arrives as events.
///
/// Every adapter did this identically — check the status, carry the headers and
/// the body into the refusal, then read the content type — and identical code
/// in three places is three chances to decide differently what "the provider
/// said no" means.
pub(crate) async fn accepted(response: reqwest::Response) -> Result<(reqwest::Response, bool)> {
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        let headers = response.headers().clone();
        return Err(refused(status, &headers, &response.text().await.or_http()?));
    }
    let streaming = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    Ok((response, streaming))
}

/// Shared HTTP client. No total timeout: local models and long thinking phases
/// stream for a long time; the read timeout is per chunk and only trips on a
/// dead connection.
///
/// One for the process, handed out as clones of the same pool. Building one is
/// the expensive part of an adapter — it loads the trust roots — and an agent
/// builds an adapter per declared provider per session; sharing it also means
/// a second session in a warm process reuses the connection the first opened.
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(600))
                .build()
                .expect("HTTP client with static configuration")
        })
        .clone()
}

/// A refusal, with what the provider said about it.
///
/// Discarding `Retry-After` and guessing is how a 429 becomes three refusals
/// inside two seconds, because a rate limit measured in minutes does not care
/// about a backoff measured in milliseconds.
pub fn refused(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> crate::Error {
    // Delta-seconds only; the HTTP-date form would need a date parser std does
    // not offer, so it is deliberately left unparsed.
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok());
    // The adapter is where the wire is understood: a 429 crosses into core as
    // the neutral exhaustion fact, never as a status core would have to read.
    if status.as_u16() == 429 {
        return crate::Error::Exhausted { retry_after, message: body.to_owned() };
    }
    if overflowed(body) {
        return crate::Error::Overflow(body.to_owned());
    }
    crate::Error::Refused { status: status.as_u16(), retry_after, body: body.to_owned() }
}

/// Whether a refusal says the request was larger than the model's context.
///
/// No status means this — hosted APIs answer 400, local runtimes 400, 413 or
/// 500 — so it is the wording that is read, as each of them spells it. This
/// is how a local model whose window nobody declared gets one: the loop makes
/// room and asks again instead of failing the session.
fn overflowed(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    [
        "context length",
        "context window",
        "context size",
        "context_length_exceeded",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "exceeds the available context",
        "greater than the context",
        "n_ctx",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// A stream that answered 200 and refused in prose. Vendor wording is
/// classified here so exhaustion still pauses instead of failing the session.
pub(crate) fn stream_refusal(detail: String) -> crate::Error {
    let lower = detail.to_ascii_lowercase();
    if ["rate_limit", "rate limit", "quota", "usage_limit", "usage limit"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return crate::Error::Exhausted { retry_after: None, message: detail };
    }
    if overflowed(&detail) {
        return crate::Error::Overflow(detail);
    }
    crate::Error::Provider(detail)
}

/// Parse the `{"data": [{"id": ...}]}` shape both key-authenticated endpoints
/// use for /models.
///
/// A window is read when the endpoint volunteers one. OpenAI itself does not,
/// but the endpoints people point this adapter at do: a local runtime knows
/// exactly what it loaded, and a gateway in front of many models is the only
/// thing that knows which. Four spellings are in the wild and all four mean
/// the same number.
pub(crate) fn model_ids(body: &serde_json::Value) -> Vec<crate::Model> {
    body.get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(serde_json::Value::as_str)?;
            Some(crate::Model::new(id, declared_window(entry)))
        })
        .collect()
}

/// The size of a model's context as an endpoint spells it, if it says at all.
pub(crate) fn declared_window(entry: &serde_json::Value) -> Option<usize> {
    ["context_window", "context_length", "max_context_length", "max_model_len"]
        .iter()
        .find_map(|name| entry.get(name).and_then(serde_json::Value::as_u64))
        .map(|window| window as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each runtime says "too large" its own way and under its own status; a
    /// refusal about anything else must not be mistaken for one.
    #[test]
    fn a_request_too_large_for_the_context_is_recognised_however_it_is_said() {
        let said = |status: u16, body: &str| {
            refused(reqwest::StatusCode::from_u16(status).unwrap(), &reqwest::header::HeaderMap::new(), body)
        };
        for (status, body) in [
            (400, r#"{"error":{"message":"the request exceeds the available context size","n_ctx":4096}}"#),
            (400, "This model's maximum context length is 8192 tokens"),
            (400, r#"{"error":{"code":"context_length_exceeded"}}"#),
            (413, "prompt is too long: 250000 tokens > 200000 maximum"),
            (500, "Trying to keep the first 5000 tokens when context window is 4096"),
        ] {
            assert!(matches!(said(status, body), elmee_core::Error::Overflow(_)), "{body}");
        }
        assert!(matches!(said(400, "invalid tool schema"), elmee_core::Error::Refused { .. }));
        assert!(matches!(
            stream_refusal("context size exceeded: n_ctx".into()),
            elmee_core::Error::Overflow(_)
        ));
    }
}
