//! The model's questions, put to the person at the other end as a form.

use super::*;

/// Put the model's questions to the person at the other end.
///
/// One form with a field per question, because that is the shape a client can
/// draw without knowing anything about us: a title, a description, and the
/// options we offered. A client with nowhere to show it, or a person who
/// closes it, is a cancelled question rather than a stuck turn.
pub(super) async fn elicit(
    client: &Client,
    session: &str,
    questions: &[elmee_core::interaction::Question],
    foreground: bool,
) -> elmee_core::InteractionResponse {
    if !client.shows.get().forms {
        return elmee_core::InteractionResponse::Cancel;
    }
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let field = format!("q{index}");
        properties.insert(
            field.clone(),
            json!({
                "type": "string",
                "title": question.header,
                "description": question.question,
                "enum": question.options.iter().map(|option| &option.label).collect::<Vec<_>>(),
                "enumDescriptions": question
                    .options
                    .iter()
                    .map(|option| &option.description)
                    .collect::<Vec<_>>(),
            }),
        );
        required.push(field);
    }
    let message = questions.iter().map(|question| question.question.as_str()).collect::<Vec<_>>().join("\n");
    let answered = client
        .ask(
            "elicitation/create",
            json!({
                "message": message,
                "mode": {
                    "form": {
                        "scope": { "sessionId": session },
                        "requestedSchema": {
                            "type": "object",
                            "properties": properties,
                            "required": required,
                        },
                    }
                },
            }),
            foreground,
        )
        .await;

    let Some(answered) = answered else { return elmee_core::InteractionResponse::Cancel };
    if answered.get("action").and_then(Value::as_str) != Some("accept") {
        return elmee_core::InteractionResponse::Cancel;
    }
    // The loop expects one answer per question, in order, and treats anything
    // else as a cancellation — so a partial form is not half-answered.
    let content = answered.get("content").cloned().unwrap_or(Value::Null);
    let answers: Vec<Vec<String>> = (0..questions.len())
        .filter_map(|index| {
            content.get(format!("q{index}")).and_then(Value::as_str).map(|answer| vec![answer.to_owned()])
        })
        .collect();
    match answers.len() == questions.len() {
        true => elmee_core::InteractionResponse::Answers(answers),
        false => elmee_core::InteractionResponse::Cancel,
    }
}
