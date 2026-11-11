use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::message::ToolCall;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundEvent {
    pub id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<OptionItem>,
    #[serde(default, rename = "multiSelect", alias = "multi_select")]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionItem {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionRequest {
    Questions { id: String, questions: Vec<Question> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum InteractionResponse {
    Answers(Vec<Vec<String>>),
    Cancel,
}

pub trait Interactor {
    fn request(&mut self, request: InteractionRequest) -> impl Future<Output = InteractionResponse> + Send;

    fn host_tool(&mut self, _call: &ToolCall) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }

    /// Whether this frontend owns the call. Kept separate from execution so
    /// the loop can safely prepare an authorized plugin batch without running
    /// later host effects ahead of earlier calls.
    fn handles_host_tool(&self, _call: &ToolCall) -> bool {
        false
    }

    /// Try to refute a claim that a standing objective is met.
    ///
    /// `Some(reason)` blocks the claim; `None` lets it through, and means
    /// either that nothing could be found against it or that this host has no
    /// way to check — the two are deliberately the same answer, because
    /// neither is a reason to refuse.
    ///
    /// It lives on this boundary because checking a claim well means reading
    /// the project with fresh eyes, and a fresh pair of eyes is another agent,
    /// which is the host's business rather than the loop's.
    fn refute(&mut self, _objective: &str, _evidence: &str) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }

    fn background_events(&mut self) -> Vec<BackgroundEvent> {
        Vec::new()
    }

    fn ack_background(&mut self, _ids: &[String]) {}
}

pub struct RejectInteractions;

impl Interactor for RejectInteractions {
    async fn request(&mut self, _: InteractionRequest) -> InteractionResponse {
        InteractionResponse::Cancel
    }
}
