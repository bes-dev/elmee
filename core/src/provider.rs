//! The port a model answers through. Adapters live in `elmee-plugin-provider`; the
//! loop sees only this.

use crate::{
    Result,
    message::{Message, ToolCall},
};

#[derive(Debug, serde::Deserialize)]
pub struct AssistantTurn {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub thinking: Vec<serde_json::Value>,
    /// (input, output) tokens this request reported; zeros when unavailable.
    #[serde(default)]
    pub tokens: (u64, u64),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<RequestMetrics>,
}

/// Provider-neutral observations for one request. Serialize/auth are local
/// durations; headers/first text/first tool/total are elapsed milliseconds since
/// the request began. Unavailable observations are absent, never zeroed.
/// These contain no prompts, credentials, or provider response headers.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct RequestMetrics {
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serialize_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_text_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_tool_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

impl AssistantTurn {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            tokens: (0, 0),
            metrics: None,
        }
    }
}

/// A piece of a reply as it arrives.
///
/// Reasoning is separated from the answer because they are watched
/// differently: the answer is what the person asked for and is kept, while the
/// reasoning is what there is to look at during the half-minute when a
/// thinking model has produced nothing else. Collapsing the two would put a
/// model's working notes in its answer; dropping the reasoning leaves a
/// spinner and an empty screen, which is what this used to do.
pub enum Chunk<'a> {
    Text(&'a str),
    Reasoning(&'a str),
}

/// A model an endpoint reports, and what it says about the size of its context.
///
/// The window travels with the name because only the endpoint knows it, and a
/// name learned without one has to be compacted on a guess. Core carries both
/// and interprets neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub context_window: Option<usize>,
}

impl Model {
    pub fn new(id: impl Into<String>, context_window: Option<usize>) -> Self {
        Self { id: id.into(), context_window }
    }
}

/// How much inference-time reasoning a capable model should use.
///
/// `None` at the provider boundary means "auto": omit the wire parameter and
/// let the endpoint/model choose its default. The named levels follow the
/// OpenAI vocabulary, which is also understood by compatible gateways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    pub const VALUES: [&'static str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

#[crate::async_trait(?Send)]
pub trait Provider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn>;

    /// Switch the model for subsequent requests.
    fn set_model(&mut self, model: String);

    /// Set inference-time reasoning depth. Providers without such a wire
    /// option may ignore it; `None` restores the endpoint's model default.
    fn set_reasoning_effort(&mut self, _effort: Option<ReasoningEffort>) {}

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        None
    }

    /// How much context the active model accepts, when the adapter knows. Core
    /// only carries the number; what a window implies for compaction is the
    /// application's policy, so nothing here interprets it.
    fn context_window(&self) -> Option<usize> {
        None
    }

    /// What this endpoint says it serves.
    async fn list_models(&self) -> Result<Vec<Model>>;
}
