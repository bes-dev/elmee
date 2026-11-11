//! What the model is offered, and how its arguments are read.
//!
//! Kept apart from the carrying out. A description is a contract with the
//! model and changes for reasons of prompting; an implementation changes for
//! reasons of behaviour. Nothing here executes anything.

use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn definitions() -> Vec<Value> {
    // A description is the one place a rule about a tool travels with the tool
    // that it is about, so the mechanics live here rather than in a system
    // prompt that is paid for whether or not the tool is ever offered. It is
    // still paid for on every request, so each says its fact once.
    const SPECS: &[(&str, &str, &str)] = &[
        // `read` stays a typed tool although `exec` could `cat`. It pages by
        // line with offset and limit, it stamps what was read so a later patch
        // can refuse to overwrite somebody else's work, and the terminal
        // highlights it by extension. Searching and listing are not like that:
        // a schema with three fields is strictly less than `rg`, so those are
        // the shell's, where the model's own knowledge of the tools counts.
        (
            "read",
            "Read a UTF-8 text file. Long output is bounded and the cut is marked; page through \
             the rest with offset and limit rather than reading the file again. When several \
             independent files are needed, return their read calls together in one response",
            r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1}},"required":["path"]}"#,
        ),
        (
            "view_image",
            "Attach a local PNG, JPEG, GIF or WebP image, at most 5 MiB, so an image-capable model \
             can see it; read returns only text. Resize a larger image or render a PDF page with \
             system tools first",
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
        ),
        (
            "apply_patch",
            "Create, change, rename and delete files as one patch. The whole patch applies or \
             nothing does, so a rename or a signature change belongs in a single call. Hunks apply \
             from top to bottom. Context and removed lines match exactly; use `@@` followed by an \
             unique exact complete line from the file to start a hunk after that line. A first hunk that \
             occurs more than once needs an anchor; later hunks use the first match after the \
             preceding one. To undo your own work, patch it back — never git reset --hard or git \
             checkout --, which throw away everyone else's too. The patch is one string:\n\
             *** Begin Patch\n\
             *** Add File: path/to/new.rs\n\
             +every line of the new file, each prefixed with +\n\
             *** Update File: path/to/existing.rs\n\
             *** Move to: path/to/renamed.rs\n\
             @@ optional unique exact complete line before this hunk\n\
             \x20unchanged context line\n\
             -line to remove\n\
             +line to add\n\
             *** Delete File: path/to/gone.rs\n\
             *** End Patch",
            r#"{"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"]}"#,
        ),
        (
            "exec",
            "Run a shell command in the working directory.\n\
             Explore with ordinary shell tools — rg, rg --files, ls, cat, jq, pipelines — there is \
             no separate search or listing tool. If a program is missing, use the next best one \
             without comment: rg is absent from many containers, grep is not. Output is bounded and \
             the cut is marked — read the rest rather than reasoning from the part you were handed, \
             and add no separators like echo ====, which are noise in the user's terminal.\n\
             Never write a file here: cat >, a heredoc or a throwaway script bypasses the checks \
             that make an edit safe, so use apply_patch. A formatter or codemod rewriting in bulk \
             is fine. Backticks and $() still execute, so prefer explicit paths you have checked \
             over globs and substitutions, and never aim a recursive or destructive command at a \
             home, root or workspace directory.\n\
             Every command is a run with an id. One still going when the wait ends keeps running \
             and you are told when it ends, so never sleep or block for minutes: follow it with \
             write_run. Its stdin is a pipe, so a command that asks a question waits for an answer",
            r#"{"type":"object","properties":{"command":{"type":"string"},"yield_ms":{"type":"integer","minimum":0,"description":"How long to wait before returning. 0 returns as soon as the run starts: use it for work longer than a couple of minutes (a build, a test suite, a training job) and for anything you will keep and talk to (a REPL, a database shell). Omitted waits the default"},"max_output_bytes":{"type":"integer","minimum":1,"description":"Cap on the output this call returns, tail first"}},"required":["command"]}"#,
        ),
        (
            "write_run",
            "Follow a run, typing to it first if there is anything to say. Without input it \
             collects what a command still going has printed since you last looked, so calling it \
             again makes progress instead of repeating. With input, the text goes to the run's \
             standard input first — an answer to a prompt, a line for a REPL — and what it says \
             back comes with it. Only a run this session started can be typed to",
            r#"{"type":"object","properties":{"id":{"type":"string"},"input":{"type":"string","description":"Omitted or empty writes nothing and only collects"},"newline":{"type":"boolean","default":true},"yield_ms":{"type":"integer","minimum":0,"description":"How long to watch before returning. A poll waits long, to be woken by new output; a write waits briefly for its reply. Raise it when the answer is slow"}},"required":["id"]}"#,
        ),
        (
            "stop_run",
            "Stop a run and its whole process group. Use force only when a normal stop was ignored",
            r#"{"type":"object","properties":{"id":{"type":"string"},"force":{"type":"boolean","default":false}},"required":["id"]}"#,
        ),
    ];
    SPECS
        .iter()
        .map(|(name, description, schema)| {
            tool(name, description, serde_json::from_str(schema).expect("valid built-in tool schema"))
        })
        .collect()
}

/// Build a model-facing tool definition. Hosts use this to declare the tools
/// they answer through `Interactor::host_tool`, which the core never executes
/// itself.
pub fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function", "function": {
        "name": name, "description": description, "parameters": parameters
    }})
}

macro_rules! args {
    ($($name:ident { $($field:ident: $kind:ty),* $(,)? })*) => {$ (
        #[derive(Deserialize)]
        pub(super) struct $name { $( pub(super) $field: $kind, )* }
    )*};
}

args! {
    ReadArgs { path: String, offset: Option<usize>, limit: Option<usize> }
    ImageArgs { path: String }
    PatchArgs { patch: String }
    ExecArgs { command: String, yield_ms: Option<u64>, max_output_bytes: Option<u64> }
    WriteRunArgs { id: String, input: Option<String>, newline: Option<bool>, yield_ms: Option<u64> }
    StopRunArgs { id: String, force: Option<bool> }
}
