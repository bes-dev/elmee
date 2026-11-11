//! Which files a session has changed, kept across a summary.
//!
//! A summary is asked which files were touched and is least reliable at
//! exactly that: paths are the one thing that cannot be paraphrased. They are
//! also the one thing this crate knows for a fact, since every change to a
//! file is a patch it applied — so the list is read off the calls themselves
//! and handed to the kernel to carry, rather than left to the summary.

use std::collections::BTreeSet;

use crate::tools::patch;

use elmee_core::{
    Middleware, MiddlewareEffect, MiddlewareEvent, PluginFuture, Result,
    message::{Message, Role},
};

const HEADING: &str = "## Kept by the harness: files this session changed";
/// Past this the list stops being something to read, and costs what it saves.
const MOST: usize = 200;

pub struct ChangedFiles;

impl Middleware for ChangedFiles {
    fn on<'a>(&'a mut self, event: MiddlewareEvent<'a>) -> PluginFuture<'a, Result<Vec<MiddlewareEffect>>> {
        Box::pin(async move {
            let MiddlewareEvent::BeforeCompaction { messages } = event else { return Ok(Vec::new()) };
            let files = changed(messages);
            Ok(match files.is_empty() {
                true => Vec::new(),
                false => vec![MiddlewareEffect::Carry(format!(
                    "{HEADING}\n{}",
                    files.iter().take(MOST).map(|path| format!("- {path}")).collect::<Vec<_>>().join("\n")
                ))],
            })
        })
    }
}

/// Every file a patch in `messages` names, with what an earlier summary
/// already listed.
fn changed(messages: &[Message]) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for message in messages {
        if matches!(message.role, Role::System)
            && let Some((_, kept)) = message.content.as_deref().and_then(|text| text.split_once(HEADING))
        {
            files.extend(kept.lines().skip(1).map_while(|line| line.strip_prefix("- ")).map(str::to_owned));
        }
        for call in message.tool_calls.iter().filter(|call| call.function.name == "apply_patch") {
            let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) else {
                continue;
            };
            files.extend(arguments["patch"].as_str().unwrap_or_default().lines().filter_map(|line| {
                [patch::UPDATE, patch::ADD, patch::DELETE, patch::MOVE]
                    .iter()
                    .find_map(|marker| line.strip_prefix(marker))
                    .map(|path| path.trim().to_owned())
            }));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use elmee_core::message::ToolCall;

    #[test]
    fn the_list_is_read_off_the_patches_and_joined_to_what_was_already_kept() {
        let mut patching = Message::assistant_text("editing");
        patching.tool_calls = vec![
            ToolCall::new(
                "1",
                "apply_patch",
                serde_json::json!({ "patch": "*** Begin Patch\n*** Update File: src/a.rs\n-x\n+y\n*** Add File: src/new.rs\n+z\n*** End Patch" })
                    .to_string(),
            ),
            ToolCall::new("2", "exec", r#"{"command":"echo '*** Update File: not/a/patch'"}"#),
        ];
        let earlier = Message::system(format!(
            "# Conversation summary\n\n## Goal\nx\n\n{HEADING}\n- old/b.rs\n\nStill to do"
        ));
        let files = changed(&[earlier, patching]);
        assert_eq!(files.into_iter().collect::<Vec<_>>(), ["old/b.rs", "src/a.rs", "src/new.rs"]);
    }
}
