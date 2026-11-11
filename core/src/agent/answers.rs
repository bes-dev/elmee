//! The calls the agent answers itself.
//!
//! Most tools are the tool set's: they touch the project and need nothing but
//! their arguments. These need something only the loop has — the person at the
//! other end, the live plan, or completion policy — so they are intercepted
//! before the tool set sees them. That is the whole rule for what belongs here.

use super::{Agent, AgentEvent, Message, Session};
use crate::{
    Result,
    evidence::VerificationLevel,
    interaction::{InteractionRequest, InteractionResponse, Interactor},
    message::ToolCall,
};

const ASK_USER: &str = "ask_user";
const PLAN: &str = "plan";

/// Model-facing declarations for operations implemented by the loop itself.
/// Keeping schema and execution together makes their kernel ownership honest.
pub(super) fn definitions(interactive: bool) -> Vec<serde_json::Value> {
    let mut tools = vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": PLAN,
                "description": "Record the steps of a long piece of work. Call it with the whole plan each time; it replaces the previous one. Exactly one step is `doing` at a time, and a step becomes `done` only when it is wholly finished. When a command proves a step is done, put that command in `proof`.",
                "parameters": {
                    "type": "object",
                    "properties": {"steps": {"type": "array", "minItems": 1, "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "doing", "done"]},
                            "proof": {"type": "string"}
                        },
                        "required": ["step"]
                    }}},
                    "required": ["steps"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": ASK_USER,
                "description": "Ask the user one to four focused questions; use only when the answer materially changes the work",
                "parameters": {
                    "type": "object",
                    "properties": {"questions": {"type": "array", "minItems": 1, "maxItems": 4, "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "header": {"type": "string"},
                            "options": {"type": "array", "minItems": 2, "maxItems": 4, "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "description": {"type": "string"}
                                },
                                "required": ["label", "description"]
                            }},
                            "multiSelect": {"type": "boolean"}
                        },
                        "required": ["question", "header", "options"]
                    }}},
                    "required": ["questions"]
                }
            }
        }),
    ];
    if !interactive {
        tools.retain(|tool| tool["function"]["name"] != ASK_USER);
    }
    tools
}

impl Agent {
    pub(super) fn answers(&self, call: &ToolCall) -> bool {
        self.plugins.completion_claim(call).is_some()
            || (matches!(call.function.name.as_str(), ASK_USER | PLAN)
                && self.plugins.has_tool(&call.function.name))
    }

    /// Record what the agent is working through, and check what can be checked
    /// about it. The plan lives here; the event is how anyone else finds out.
    fn plan<F: FnMut(AgentEvent)>(&mut self, call: &ToolCall, on_event: &mut F) -> String {
        let steps = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
            .ok()
            .and_then(|args| serde_json::from_value::<Vec<crate::plan::Step>>(args["steps"].clone()).ok())
            .unwrap_or_default();
        match crate::plan::apply(steps, &self.evidence) {
            Err(error) => format!("Error: {error}"),
            Ok((steps, report)) => {
                self.plan = steps.clone();
                on_event(AgentEvent::Plan { steps });
                report
            }
        }
    }

    /// `Some` when this is one of the calls the loop answers itself.
    pub(super) async fn answer<F, I>(
        &mut self,
        call: &ToolCall,
        on_event: &mut F,
        interactor: &mut I,
    ) -> Option<String>
    where
        F: FnMut(AgentEvent) + Send,
        I: Interactor,
    {
        if let Some(claim) = self.plugins.completion_claim(call) {
            return Some(self.claim_completion(call, &claim, interactor).await);
        }
        if matches!(call.function.name.as_str(), ASK_USER | PLAN)
            && !self.plugins.has_tool(&call.function.name)
        {
            return None;
        }
        Some(match call.function.name.as_str() {
            ASK_USER => ask_user(call, interactor).await,
            PLAN => self.plan(call, on_event),
            _ => return None,
        })
    }

    /// Accept or refuse a claim that the objective is met.
    ///
    /// The claim is the agent's, the verdict is the project's: whatever
    /// `before_finish` checks run here, and a failing check leaves the
    /// continuation active with the failure as the answer. Nothing else in the loop can be
    /// talked out of that.
    pub(super) async fn claim_completion<I: Interactor>(
        &mut self,
        call: &ToolCall,
        claim: &crate::CompletionClaim,
        interactor: &mut I,
    ) -> String {
        // The project's own check first: it is a command, and cheaper than a
        // second model. It is also the only floor a human declared, so nothing
        // below may overrule it.
        if let Err(error) =
            self.plugins.dispatch(crate::MiddlewareEvent::BeforeFinish { answer: &claim.evidence }).await
        {
            return format!("Error: the completion check refused this claim: {error}");
        }
        // Then a fresh reader, whose job is to find what is not met. Claiming
        // and judging must not be the same context, or the audit is the
        // claimant's own reasoning wearing a different hat.
        if let Some(reason) = interactor.refute(&claim.context, &claim.evidence).await {
            return format!("Error: a check of this claim refused it: {reason}");
        }
        let tree = self.tree_hash().await;
        self.evidence.record(
            "independent_refutation",
            &claim.evidence,
            None,
            tree,
            "no refutation found",
            VerificationLevel::External,
        );
        match self.plugins.accept_completion(call) {
            true => "Objective recorded as achieved.".into(),
            // A claim made after the loop already gave up still counts: the
            // wrap-up turn is the agent's last chance to be right.
            false => "Error: no standing objective to finish".into(),
        }
    }
}

pub(super) async fn ask_user<I: Interactor>(call: &crate::message::ToolCall, interactor: &mut I) -> String {
    #[derive(serde::Deserialize)]
    struct Input {
        questions: Vec<crate::interaction::Question>,
    }
    let input = match serde_json::from_str::<Input>(&call.function.arguments) {
        Ok(input)
            if (1..=4).contains(&input.questions.len())
                && input.questions.iter().all(|question| (2..=4).contains(&question.options.len())) =>
        {
            input
        }
        Ok(_) => return "Error: ask_user requires 1-4 questions with 2-4 options each".into(),
        Err(error) => return format!("Error: invalid ask_user input: {error}"),
    };
    match interactor
        .request(InteractionRequest::Questions { id: call.id.clone(), questions: input.questions.clone() })
        .await
    {
        InteractionResponse::Answers(answers) if answers.len() == input.questions.len() => {
            let answers = input
                .questions
                .iter()
                .zip(answers)
                .map(|(question, answer)| (question.question.clone(), serde_json::json!(answer)))
                .collect::<serde_json::Map<_, _>>();
            serde_json::json!({"answers": answers}).to_string()
        }
        _ => "User cancelled the question".into(),
    }
}

/// Tools answered by the frontend and by the interaction layer report failure
/// in prose, by convention; the built-in set reports it properly, so this is
/// the only place a result has to be read to know how it went.
pub(super) fn reported_failure(output: &str) -> bool {
    output.starts_with("Error:")
}

pub(super) fn emit_tool<F: FnMut(AgentEvent)>(
    call: &crate::message::ToolCall,
    output: String,
    on_event: &mut F,
    session: &mut Session,
) -> Result<()> {
    let started = std::time::Instant::now();
    let (id, name) = (call.id.clone(), call.function.name.clone());
    let arguments = call.function.arguments.clone();
    on_event(AgentEvent::ToolStart { id: id.clone(), name: name.clone(), arguments });
    on_event(AgentEvent::ToolEnd {
        id: id.clone(),
        name,
        ok: !reported_failure(&output),
        duration_ms: started.elapsed().as_millis(),
        output: output.clone(),
    });
    session.push(Message::tool(id, output)).map(|_| ())
}
