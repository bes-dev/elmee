//! One turn's events, in the words the protocol uses.
//!
//! Kept beside the transport rather than in the tool plugins that produce the
//! events: what a `tool_call` looks like on the wire is this protocol's
//! business, and a tool that had to know would be a tool coupled to a frontend.

use std::path::Path;

use elmee_core::{
    AgentEvent,
    message::{Message, MessageOrigin, Role},
};
use elmee_plugin_tools::tools::patch::{self, Change, Kind};
use serde_json::{Value, json};

/// The classification an editor uses to decide how to draw a tool call. Ours
/// are known by name, because the name is what the model was offered and the
/// only thing every tool has. A patch says what kind of edit it is.
fn kind(tool: &str, changes: &[Change]) -> &'static str {
    match tool {
        "read" | "view_image" => "read",
        "apply_patch" if changes.iter().all(|change| change.kind == Kind::Delete) => "delete",
        "apply_patch" if changes.iter().any(|change| change.move_to.is_some()) => "move",
        "apply_patch" => "edit",
        "exec" | "write_run" | "stop_run" => "execute",
        "web_search" => "search",
        "web_fetch" => "fetch",
        "plan" | "goal_done" => "think",
        _ => "other",
    }
}

/// A path as the protocol wants it: absolute, which the model's are not.
fn absolute(cwd: &Path, path: &str) -> Value {
    json!({ "path": cwd.join(path) })
}

/// What a patch does, as an editor draws it: for each hunk, the text before
/// and after. The hunk is the region the model chose to show, and drawing it
/// is what makes an edit reviewable without opening the file.
fn diffs(cwd: &Path, changes: &[Change]) -> Vec<Value> {
    changes
        .iter()
        .flat_map(|change| {
            let path = cwd.join(&change.path);
            change.hunks.iter().map(move |hunk| {
                json!({
                    "type": "diff",
                    "path": path,
                    "oldText": (change.kind != Kind::Add).then(|| hunk.old.join("\n")),
                    "newText": hunk.new.join("\n"),
                })
            })
        })
        .collect()
}

/// A line a person can read in a list, since the editor shows this and not the
/// arguments. The command itself for a shell run, the path for a file, the
/// tool's own name when nothing better is in the arguments.
fn title(tool: &str, arguments: &Value) -> String {
    let field = |key: &str| arguments.get(key).and_then(Value::as_str);
    match field("command").or_else(|| field("path")).or_else(|| field("query")) {
        Some(subject) => format!("{tool} {}", elmee_core::text::clip(subject, 200)),
        None => tool.to_owned(),
    }
}

/// A call as it starts: what it is, where it happens, and for an edit, what
/// it changes.
pub(crate) fn start(cwd: &Path, id: &str, name: &str, arguments: &str) -> Value {
    let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let changes = match name {
        "apply_patch" => {
            parsed["patch"].as_str().and_then(|patch| patch::parse(patch).ok()).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let locations: Vec<Value> = match name {
        "read" | "view_image" => {
            parsed["path"].as_str().map(|path| absolute(cwd, path)).into_iter().collect()
        }
        _ => changes.iter().map(|change| absolute(cwd, &change.path)).collect(),
    };
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "name": name,
        "title": title(name, &parsed),
        "kind": kind(name, &changes),
        "status": "in_progress",
        "locations": locations,
        "content": diffs(cwd, &changes),
        "rawInput": parsed,
    })
}

/// A call as it ends: whether it worked, and what it said.
pub(crate) fn end(id: &str, ok: bool, output: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        // A command that ran and exited non-zero is a failure to whoever is
        // watching, though the tool that ran it did its job.
        "status": if ok && elmee_core::text::exit_status(output).is_none_or(|status| status == 0) {
            "completed"
        } else {
            "failed"
        },
        "content": [{ "type": "content", "content": { "type": "text", "text": output } }],
    })
}

/// One more piece of the summary a compaction is writing.
pub(crate) fn summarizing(id: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": id,
        "content": { "type": "text", "text": text },
    })
}

/// How a compaction ended, and what stands in the conversation's place.
pub(crate) fn compaction(id: &str, status: &str, summary: Option<&str>) -> Value {
    let mut update = json!({ "sessionUpdate": "compaction_update", "compactionId": id, "status": status });
    if let Some(summary) = summary {
        update["summary"] = json!([{ "type": "text", "text": summary }]);
    }
    update
}

pub(crate) fn said(update: &str, text: &str) -> Value {
    json!({ "sessionUpdate": update, "content": { "type": "text", "text": text } })
}

/// What an editor is told about one step of a turn.
///
/// `None` for the events that have no counterpart: the protocol has no notion
/// of a model request starting, and saying nothing is better than inventing a
/// notification a client would have to learn to ignore. `window` is the
/// model's context size, which is what turns a token count into a gauge.
pub(crate) fn of(cwd: &Path, window: Option<usize>, event: &AgentEvent) -> Option<Value> {
    Some(match event {
        AgentEvent::Text { text } => said("agent_message_chunk", text),
        AgentEvent::Reasoning { text } => said("agent_thought_chunk", text),
        AgentEvent::ToolStart { id, name, arguments } => start(cwd, id, name, arguments),
        AgentEvent::ToolEnd { id, ok, output, .. } => end(id, *ok, output),
        AgentEvent::Plan { steps } => json!({
            "sessionUpdate": "plan",
            "entries": steps
                .iter()
                .map(|step| json!({
                    "content": step.step,
                    "priority": "medium",
                    "status": match step.status {
                        elmee_core::plan::Status::Pending => "pending",
                        elmee_core::plan::Status::Doing => "in_progress",
                        elmee_core::plan::Status::Done => "completed",
                    },
                    // The protocol has no slot for a citation, and dropping it
                    // would lose the one thing that makes "done" mean anything
                    // here. `_meta` is where the spec puts what it does not model.
                    "_meta": { "elmee": { "proof": step.proof, "proven": step.proven } },
                }))
                .collect::<Vec<_>>(),
        }),
        // The prompt the model was just charged for is what is in its context.
        AgentEvent::ModelEnd { input_tokens, metrics, duration_ms, ok, .. } => json!({
            "sessionUpdate": "usage_update",
            "used": input_tokens,
            "size": window?,
            "_meta": { "elmee": { "request": metrics, "durationMs": duration_ms, "ok": ok } },
        }),
        AgentEvent::ModelStart { .. } | AgentEvent::Compacting { .. } | AgentEvent::Compacted { .. } => {
            return None;
        }
    })
}

/// A conversation handed back, as the updates that would have produced it.
///
/// Only what a person actually typed comes back in their voice: a turn's
/// history also carries messages the harness wrote in the user's role — an
/// identity stamp, a peer's report — and replaying those would put words in
/// their mouth. A system prompt is not something anybody said.
pub(crate) fn replay(cwd: &Path, messages: &[Message]) -> Vec<Value> {
    let mut updates = Vec::new();
    for message in messages {
        let text = message.content.as_deref().filter(|text| !text.is_empty());
        match message.role {
            Role::User if message.origin == MessageOrigin::User => {
                updates.extend(text.map(|text| said("user_message_chunk", text)));
            }
            Role::Assistant => {
                updates.extend(text.map(|text| said("agent_message_chunk", text)));
                for call in &message.tool_calls {
                    updates.push(start(cwd, &call.id, &call.function.name, &call.function.arguments));
                }
            }
            Role::Tool => {
                if let Some(id) = &message.tool_call_id {
                    updates.push(end(id, true, text.unwrap_or_default()));
                }
            }
            _ => {}
        }
    }
    updates
}
