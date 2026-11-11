//! What a directory expects of an agent working inside it.
//!
//! Startup reads `AGENTS.md`/`CLAUDE.md` by walking up from the workspace root,
//! so a rule written three directories down is never seen. Reading every one of
//! them up front is not the fix: most are irrelevant to any given task, and the
//! system prompt is the one part of a request the provider caches by prefix —
//! growing it with rules about directories the agent never opens costs tokens
//! on every turn.
//!
//! So they are picked up on arrival instead. When a tool touches a path, the
//! instruction files above it are appended to *that tool's result*: the prompt
//! stays byte-identical, and the local rules land exactly when they start to
//! matter. Each directory is offered once per session, because a rule repeated
//! every time a file in it is read is a rule the model learns to skip.
//!
//! Project-authored files are trusted context (architecture trust zone 2), so
//! they are not fenced as untrusted — but they are bounded, because an
//! instruction file is written by a person and a runaway one would otherwise
//! evict the conversation.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use elmee_core::message::ToolCall;

use crate::Result;

/// Per instruction file. Long enough for any that is meant to be read.
const LIMIT: usize = 8 * 1024;

/// How far above a touched path to look. Further up is a rule about the
/// project rather than the directory, and the project's own file was already
/// read at startup.
const LEVELS: usize = 5;

pub struct Directives {
    root: PathBuf,
    roots: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
}

impl Directives {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = root.canonicalize().unwrap_or(root);
        // The workspace root and everything above it is what startup walked.
        // Repeating those files would spend tokens saying nothing new.
        Self { seen: root.ancestors().map(Path::to_owned).collect(), roots: vec![root.clone()], root }
    }

    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots.extend(roots.into_iter().map(|root| root.canonicalize().unwrap_or(root)));
        self.roots.sort();
        self.roots.dedup();
        self
    }

    /// The instructions this call brought into scope, general first.
    fn arriving(&mut self, call: &ToolCall) -> String {
        let mut files = Vec::new();
        for touched in paths(&call.function.arguments) {
            let Some(directory) = self.directory(&touched) else { continue };
            let mut above = Vec::new();
            for directory in directory.ancestors().take(LEVELS) {
                if self.seen.insert(directory.to_owned())
                    && let Some(file) = crate::context::instruction_file(directory)
                {
                    above.push(file);
                }
            }
            files.extend(above.into_iter().rev());
        }
        let mut text = String::new();
        for file in files {
            // An unreadable instruction file degrades to a warning rather than
            // failing the tool call that happened to walk past it.
            match fs::read_to_string(&file) {
                Ok(content) => text.push_str(&format!(
                    "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    file.display(),
                    elmee_core::text::bound(crate::context::expand_imports(&content, &file, 0), LIMIT, true)
                )),
                Err(error) => eprintln!("elmee: skipping instructions {}: {error}", file.display()),
            }
        }
        text
    }

    /// The directory a tool argument concerns, when it is one this workspace
    /// owns. A path outside the workspace is somebody else's project, and its
    /// rules are not this agent's to follow.
    fn directory(&self, path: &str) -> Option<PathBuf> {
        let path = Path::new(path);
        let path = if path.is_absolute() { path.to_owned() } else { self.root.join(path) };
        let directory = if path.is_dir() { path } else { path.parent()?.to_owned() };
        let directory = directory.canonicalize().ok()?;
        self.roots.iter().any(|root| directory.starts_with(root)).then_some(directory)
    }
}

/// Every path a call named, wherever it sits in the arguments.
///
/// Keyed on the argument name rather than on a list of tools: `read` and any
/// MCP tool following the same convention call it `path`, so they are picked
/// up for free. A patch names its files in its own grammar instead, and a
/// shell command names no directory it is not already running in — which is
/// the workspace root startup already covered.
fn paths(arguments: &str) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        collect(&value, &mut found);
        if let Some(patch) = value["patch"].as_str() {
            found.extend(patched(patch));
        }
    }
    found
}

/// The files a patch acts on, read from its headers.
fn patched(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("*** ")?;
            ["Add File: ", "Update File: ", "Delete File: ", "Move to: "]
                .iter()
                .find_map(|marker| rest.strip_prefix(marker))
                .map(|path| path.trim().to_owned())
        })
        .collect()
}

fn collect(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                match (key.as_str(), value.as_str()) {
                    ("path", Some(path)) => found.push(path.to_owned()),
                    _ => collect(value, found),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, found)),
        _ => {}
    }
}

impl elmee_core::Middleware for Directives {
    fn phase(&self) -> elmee_core::MiddlewarePhase {
        elmee_core::MiddlewarePhase::Context
    }

    fn on<'a>(
        &'a mut self,
        event: elmee_core::MiddlewareEvent<'a>,
    ) -> elmee_core::PluginFuture<'a, Result<Vec<elmee_core::MiddlewareEffect>>> {
        Box::pin(async move {
            let elmee_core::MiddlewareEvent::AfterTool { call, .. } = event else { return Ok(Vec::new()) };
            let arriving = self.arriving(call);
            Ok(match arriving.is_empty() {
                true => Vec::new(),
                false => vec![elmee_core::MiddlewareEffect::AddContext(arriving)],
            })
        })
    }
}

impl elmee_core::Plugin for Directives {
    fn id(&self) -> &'static str {
        "project-directives"
    }

    fn declare(self, plugins: &mut elmee_core::Registrar) -> Result<()> {
        plugins.provide_middleware(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elmee_core::Middleware;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("elmee-directives-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    async fn after(directives: &mut Directives, arguments: &str) -> String {
        let call = ToolCall::new("1", "read", arguments.to_owned());
        let effects =
            directives.on(elmee_core::MiddlewareEvent::AfterTool { call: &call, output: "", ok: true }).await;
        effects
            .unwrap()
            .into_iter()
            .map(|effect| match effect {
                elmee_core::MiddlewareEffect::AddContext(text) => text,
                _ => String::new(),
            })
            .collect()
    }

    #[tokio::test]
    async fn explicit_additional_roots_bring_their_directory_instructions_into_scope() {
        let main = scratch("extra-main");
        let extra = scratch("extra-root");
        fs::write(extra.join("AGENTS.md"), "follow the extra root rules").unwrap();
        fs::write(extra.join("file.rs"), "fn main() {}").unwrap();
        let arguments = serde_json::json!({"path":extra.join("file.rs")}).to_string();
        assert!(after(&mut Directives::new(&main), &arguments).await.is_empty());
        let mut directives = Directives::new(&main).with_roots(vec![extra.clone()]);
        assert!(after(&mut directives, &arguments).await.contains("follow the extra root rules"));
        assert!(after(&mut directives, &arguments).await.is_empty());
        fs::remove_dir_all(main).unwrap();
        fs::remove_dir_all(extra).unwrap();
    }

    /// The whole point: a rule three directories down is learned when the
    /// agent opens a file there, exactly once, and the prompt never moves.
    #[tokio::test]
    async fn a_nested_rule_arrives_with_the_tool_result_and_only_once() {
        let root = scratch("nested");
        fs::create_dir_all(root.join("crates/parser/src")).unwrap();
        fs::write(root.join("AGENTS.md"), "the project rule").unwrap();
        fs::write(root.join("crates/parser/AGENTS.md"), "the parser rule").unwrap();
        fs::write(root.join("crates/parser/src/lexer.rs"), "fn main() {}").unwrap();
        let prompt = crate::context::system_prompt(&root).unwrap();

        let mut directives = Directives::new(&root);
        // Nothing has been touched yet, and the root file is already in the
        // prompt, so reading beside it says nothing new.
        assert_eq!(after(&mut directives, r#"{"path":"AGENTS.md"}"#).await, "");

        let arrived = after(&mut directives, r#"{"path":"crates/parser/src/lexer.rs"}"#).await;
        assert!(arrived.contains("the parser rule"), "{arrived}");
        assert!(!arrived.contains("the project rule"), "startup already sent it: {arrived}");
        assert!(arrived.contains("crates/parser/AGENTS.md"), "{arrived}");

        // A second file in the same subtree repeats nothing.
        assert_eq!(after(&mut directives, r#"{"path":"crates/parser/src/other.rs"}"#).await, "");
        assert_eq!(after(&mut directives, r#"{"path":"crates/parser"}"#).await, "");

        // The mechanism appends to tool results; the prompt is what the
        // provider caches, and it must be untouched by any of this.
        assert_eq!(crate::context::system_prompt(&root).unwrap(), prompt);
        fs::remove_dir_all(root).unwrap();
    }

    /// An instruction file is written by a person and nothing bounds its
    /// length; a tool result that carried all of it would evict the work.
    /// A path outside the workspace is another project's business.
    #[tokio::test]
    async fn a_long_file_is_bounded_and_a_foreign_directory_is_not_read() {
        let root = scratch("bounds");
        fs::create_dir_all(root.join("deep")).unwrap();
        fs::write(root.join("deep/CLAUDE.md"), "x".repeat(LIMIT * 2)).unwrap();
        fs::write(root.join("deep/file.rs"), "").unwrap();

        let mut directives = Directives::new(&root);
        let arrived = after(&mut directives, r#"{"path":"deep/file.rs"}"#).await;
        assert!(arrived.len() < LIMIT + 512, "{} bytes", arrived.len());
        assert!(arrived.contains("output truncated"), "{arrived}");

        let outside = scratch("bounds-outside");
        fs::write(outside.join("AGENTS.md"), "somebody else's rule").unwrap();
        fs::write(outside.join("file.rs"), "").unwrap();
        let escape = format!(r#"{{"path":"{}"}}"#, outside.join("file.rs").display());
        assert_eq!(after(&mut directives, &escape).await, "");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    /// Every edit in a batch is a directory the agent arrived in, and the
    /// nested `path` keys are the only thing this needs to know about the
    /// tool that carried them.
    #[tokio::test]
    async fn a_patch_picks_up_every_directory_it_touched() {
        let root = scratch("batch");
        for name in ["one", "two"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("AGENTS.md"), format!("rule {name}")).unwrap();
            fs::write(root.join(name).join("file.rs"), "").unwrap();
        }
        let mut directives = Directives::new(&root);
        // A patch names its files in its own grammar, not under a `path` key.
        let patch = "*** Begin Patch\n*** Update File: one/file.rs\n*** Add File: two/new.rs\n*** End Patch";
        let arguments = serde_json::json!({ "patch": patch }).to_string();
        let arrived = after(&mut directives, &arguments).await;
        assert!(arrived.contains("rule one") && arrived.contains("rule two"), "{arrived}");
        assert_eq!(after(&mut directives, &arguments).await, "");
        fs::remove_dir_all(root).unwrap();
    }
}
