//! Terminal slash commands: the registry, and what typing one does.
//!
//! The list and the routing sit together on purpose — a command that exists in
//! one and not the other is the failure this arrangement makes obvious.
//!
//! The registry is built-ins plus the skills of the project and the user: a
//! skill is invoked as `/name`, with `$ARGUMENTS` replaced by whatever
//! follows the name. Discovery and expansion belong to
//! `elmee-plugin-command`; this module only presents and routes them.

use std::path::PathBuf;

pub use elmee_plugin_command::Command;

use super::app::{Action, App, Cell, Panel, Picker, PickerKind};
use super::worker::Cmd;

pub fn builtin() -> Vec<Command> {
    let built = |name: &str, description: &str| Command {
        name: name.to_owned(),
        description: description.to_owned(),
        hint: None,
        template: None,
    };
    // A command that cannot run bare says what it wants, so the menu
    // completes it and waits rather than sending half a command.
    let wanting = |name: &str, hint: &str, description: &str| Command {
        hint: Some(hint.to_owned()),
        ..built(name, description)
    };
    // Workflow commands are prompts, not engine features: they ride the same
    // template path as user-defined commands, so there is nothing to maintain
    // beyond the words. A user file of the same name shadows them.
    let prompt = |name: &str, description: &str, template: &str| Command {
        name: name.to_owned(),
        description: description.to_owned(),
        hint: None,
        template: Some(template.to_owned()),
    };
    vec![
        built("help", "list all commands"),
        prompt(
            "init",
            "write an AGENTS.md describing this project",
            "Explore this project — its build, test, and lint commands, layout, and conventions — \
             then write a concise AGENTS.md at its root for future coding agents. Record only what \
             is not obvious from reading the code, and verify every command you document. \
             If AGENTS.md already exists, improve it in place. $ARGUMENTS",
        ),
        wanting("next", "PROMPT", "queue a prompt for after this turn instead of steering into it"),
        built("clear", "clear context and return queued follow-ups to the editor (history stays)"),
        built("compact", "summarize the context to shrink it"),
        built("model", "list models and switch (arrows + Enter)"),
        wanting("reasoning", "LEVEL", "how hard the model thinks, where the endpoint lets you say"),
        built("usage", "token usage for this session"),
        built("agents", "show background agents and project peers"),
        built("runs", "show long commands still running"),
        built("copy", "copy the last reply to the clipboard"),
        built("export", "write the transcript to a Markdown file"),
        built("new", "start a fresh session file"),
        built("resume", "pick a past session of this directory (or /resume PATH)"),
        built("rewind", "go back to an earlier prompt: the conversation, the files, or both"),
        built("tree", "switch between the branches rewinding has left"),
        built("session", "show the session file path"),
        built("mcp", "show connected MCP servers and the tools they offer"),
        built("quit", "exit elmee"),
    ]
}

/// Built-ins plus custom command plugins; project files win over user files.
pub fn load(cwd: &std::path::Path, plugins: Vec<elmee_core::CommandSpec>) -> Vec<Command> {
    let mut commands = builtin();
    commands.extend(plugins.into_iter().map(|spec| Command {
        name: spec.name.to_owned(),
        description: spec.description.to_owned(),
        hint: None,
        template: None,
    }));
    elmee_plugin_command::load(cwd, commands)
}

pub fn route(app: &mut App, line: &str) -> Action {
    let line = line.trim();
    if line.is_empty() {
        return Action::None;
    }
    // `!` is the person using their own terminal: it runs, its output is
    // printed, and none of it reaches the model. That is the whole of the
    // notation — a `/` command may hand the agent something, and this one
    // cannot, which is the distinction worth having a separate character for.
    if let Some(command) = line.strip_prefix('!') {
        let command = command.trim();
        if command.is_empty() {
            app.cells.push(Cell::Info(
                "! runs a command here; its output stays out of the agent's context".into(),
            ));
            return Action::None;
        }
        if command == "cd" || command.starts_with("cd ") {
            app.cells.push(Cell::Error(
                "the working directory is fixed for the session: every tool, session file and \
                 checkpoint is keyed by it. Start elmee elsewhere with --cwd."
                    .into(),
            ));
            return Action::None;
        }
        app.cells.push(Cell::Shell { command: command.to_owned(), output: None, ok: true });
        return Action::Shell(command.to_owned());
    }
    let Some(rest) = line.strip_prefix('/') else {
        return submit_prompt(app, line.to_owned(), line);
    };
    let (name, arguments) = match rest.split_once(char::is_whitespace) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest, ""),
    };
    // A turn in flight owns the agent, so a command that needs it would sit
    // unanswered until the turn ended and then act on a moment that had
    // passed — a list opening over whatever was being typed by then. Said at
    // once instead; what only reads is answered below without the agent.
    if app.busy && matches!(name, "model" | "reasoning" | "tree" | "mcp" | "rewind" | "resume" | "new") {
        app.cells
            .push(Cell::Info(format!("/{name} needs the agent, which is busy — Esc Esc stops the turn")));
        return Action::None;
    }
    match name {
        "quit" | "exit" => Action::Quit,
        "help" => {
            let help = help_text(app);
            app.panel = Some(Panel::new("commands", help));
            Action::None
        }
        "session" => {
            app.panel = Some(Panel::new("session", app.session.clone()));
            Action::None
        }
        // The other half of pressing Enter mid-turn: this one waits.
        "next" if !arguments.is_empty() => {
            app.cells.push(Cell::User(arguments.to_owned()));
            app.queued.push_back(arguments.to_owned());
            app.cells.push(Cell::Info("queued — sent after the current turn".to_owned()));
            Action::None
        }
        "next" => {
            app.cells.push(Cell::Error("usage: /next PROMPT".to_owned()));
            Action::None
        }
        "model" => {
            if arguments.is_empty() {
                // Up at once with every model this machine already knows of.
                // The endpoints are asked behind it, and only when they were
                // not asked a moment ago: opening a list to look at it is not
                // a reason to wait on the network each time.
                let mut known = elmee_host::session::known_models(&app.cwd);
                if !known.contains(&app.model) {
                    known.insert(0, app.model.clone());
                }
                let items = known.into_iter().map(|model| (model.clone(), model)).collect();
                app.picker = Some(Picker::new("model", items, PickerKind::Model, Some(app.model.clone())));
                Action::RefreshModels
            } else {
                Action::Worker(Cmd::SetModel { spec: arguments.to_owned() })
            }
        }
        "reasoning" => {
            let effort = match arguments {
                "auto" => None,
                value => match elmee_core::provider::ReasoningEffort::parse(value) {
                    Some(effort) => Some(effort),
                    None => {
                        app.cells.push(Cell::Error(
                            "usage: /reasoning auto|none|minimal|low|medium|high|xhigh".to_owned(),
                        ));
                        return Action::None;
                    }
                },
            };
            Action::Worker(Cmd::SetReasoningEffort(effort))
        }
        "usage" => {
            app.panel = Some(Panel::new("usage", usage_report(app)));
            Action::None
        }
        "agents" => {
            app.panel = Some(Panel::new("agents", agents_report(app)));
            Action::None
        }
        "runs" => {
            app.panel = Some(Panel::new("background runs", elmee_host::session::runs(&app.cwd)));
            Action::None
        }
        "copy" => {
            let last = app.cells.iter().rev().find_map(|cell| match cell {
                Cell::Assistant { text, .. } => Some(text.clone()),
                _ => None,
            });
            if let Some(text) = last {
                Action::Copy(text)
            } else {
                app.cells.push(Cell::Info("nothing to copy yet".to_owned()));
                Action::None
            }
        }
        "export" => Action::Export(if arguments.is_empty() {
            "elmee-transcript.md".to_owned()
        } else {
            arguments.to_owned()
        }),
        "new" => Action::New,
        "resume" => {
            if arguments.is_empty() {
                Action::ResumeList
            } else {
                Action::Worker(Cmd::OpenSession(PathBuf::from(arguments)))
            }
        }
        "rewind" => Action::Worker(Cmd::RewindList),
        // Clearing the context but leaving the conversation on screen reads as
        // amnesia: the agent no longer knows what the user can still see.
        "clear" => {
            // A previously queued follow-up must not refill the cleared
            // context automatically when the current turn reports Done.
            // Preserve the user's words for explicit resubmission.
            if !app.queued.is_empty() {
                app.restore_queued();
                app.cells.push(Cell::Info("queued follow-ups returned to the editor; not sent".into()));
            }
            app.clear_screen = true;
            Action::Worker(Cmd::Clear)
        }
        "compact" => Action::Worker(Cmd::Compact),
        "tree" => Action::Worker(Cmd::Branches),
        "mcp" => Action::Worker(Cmd::Mcp),
        _ => match app.commands.iter().find(|command| command.name == name).cloned() {
            Some(Command { template: Some(template), .. }) => {
                app.cells.push(Cell::User(line.to_owned()));
                Action::Expand { template, arguments: arguments.to_owned() }
            }
            Some(Command { template: None, .. }) => {
                if app.busy {
                    app.cells.push(Cell::Info(format!("/{name} runs when the current turn ends")));
                }
                Action::Worker(Cmd::Plugin { name: name.to_owned(), arguments: arguments.to_owned() })
            }
            _ => {
                app.cells.push(Cell::Error(format!("unknown command: /{name} — /help lists commands")));
                Action::None
            }
        },
    }
}

/// What the session has spent, from what the frontend has already been told.
fn usage_report(app: &App) -> String {
    let thousands = |count: u64| match count {
        0..1_000 => count.to_string(),
        _ => format!("{:.1}k", count as f64 / 1_000.0),
    };
    let window = match app.compact_chars {
        Some(limit) if limit > 0 => {
            format!(" · {}% of what fits before compaction", (app.context_chars * 100).div_ceil(limit))
        }
        _ => String::new(),
    };
    format!(
        "tokens  {} in · {} out\ncontext {} chars{window}",
        thousands(app.tokens.0),
        thousands(app.tokens.1),
        thousands(app.context_chars as u64),
    )
}

/// Who answers to a name in this project, from the last view of it.
fn agents_report(app: &App) -> String {
    let mut lines = vec![format!("@{}", app.identity)];
    for agent in &app.agents {
        let mine = if agent.task.is_some() { " " } else { "peer " };
        // Named only where it differs from ours: three reviewers on three
        // models are unreadable without it, and a project where everything
        // runs on one model gains nothing from a column repeating it.
        let elsewhere = agent
            .task
            .as_ref()
            .map(|task| task.model.as_str())
            .filter(|model| !model.is_empty() && *model != app.model)
            .map_or_else(String::new, |model| format!("on {model} · "));
        let detail = agent.task.as_ref().map_or(agent.model.as_str(), |task| task.detail.as_str());
        lines.push(format!("  {mine}@{} · {} · {elsewhere}{detail}", agent.name, agent.status));
    }
    if lines.len() == 1 {
        lines.push("  nobody else in this project".into());
    }
    lines.join("\n")
}

fn submit_prompt(app: &mut App, prompt: String, shown: &str) -> Action {
    app.cells.push(Cell::User(shown.to_owned()));
    // Talking to an agent that is working is the same act as talking to one
    // that is not, so it is the same key. The message reaches it at the next
    // step rather than interrupting the one in flight; `/next` is for the
    // other intent, and Esc is still how you stop it.
    if app.busy {
        app.cells.push(Cell::Info("steering — the agent sees this at its next step".to_owned()));
        return Action::Steer(prompt);
    }
    Action::Worker(Cmd::Prompt(prompt))
}

fn help_text(app: &App) -> String {
    let mut text = String::from("Commands:\n");
    for command in &app.commands {
        let hint = command.hint.as_deref().map_or_else(String::new, |hint| format!(" {hint}"));
        let named = format!("/{}{hint}", command.name);
        text.push_str(&format!("  {named:<20} {}\n", command.description));
    }
    text.push_str(
        "Your own commands are skills: NAME/SKILL.md under .elmee/skills/ or\n\
         .agents/skills/ (this project, or the same in your home directory).\n\
         /NAME sends the skill as the prompt, and the agent may also pick it up\n\
         on its own. $ARGUMENTS is everything typed after the name, $1..$9 the\n\
         words of it; where a skill has no place for them they follow it. Its\n\
         --- block may set description: and argument-hint:. A !`command` in it\n\
         runs, and its output is placed in the prompt.\n\
         `! command` runs it here, in your own authority, and prints what it says —\n\
         none of it reaches the model. What it exports stays set for everything\n\
         elmee starts afterwards, the agent's own commands included, which is what\n\
         makes it the same terminal rather than a subshell. (Inside a command file,\n\
         `` !`cmd` `` is the opposite: it runs and its output is spliced into the\n\
         prompt on purpose.)\n\
         Keys: Enter send · Ctrl+J newline · Up/Down history · Ctrl+R search history\n\
         Esc closes a report like this one · /clear scrolls the screen away\n\
         Tab complete /command or @file · Ctrl+O tool output\n\
         Esc Esc clears the line; on an empty line it stops the turn, and with\n\
         nothing running it opens /rewind · Ctrl+C abort/quit\n\
         /rewind goes back to before a prompt — the conversation, the files as\n\
         they were then, or both — and hands the prompt back to edit. What it\n\
         leaves behind stays a branch, and /tree returns to it.\n\
         Up/Down move between lines, then to the line end, then through history\n\
         Scroll with your terminal — finished lines are ordinary scrollback",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use elmee_plugin_command::expand;
    use std::fs;

    #[test]
    fn clear_returns_queued_work_without_sending_it_after_the_current_turn() {
        use crate::worker::Out;
        for busy in [false, true] {
            let mut app = App::new("model".into(), "session".into(), builtin(), std::env::temp_dir(), false);
            app.busy = busy;
            app.queued.push_back("old first".into());
            app.queued.push_back("old second".into());
            app.editor.replace("unsent draft");
            assert!(matches!(route(&mut app, "/clear"), Action::Worker(Cmd::Clear)));
            assert!(app.queued.is_empty());
            assert_eq!(app.editor.text(), "old first\nold second\nunsent draft");
            assert!(app.apply(Out::Done { error: None, aborted: false }).is_none());
            // A persistence failure must not silently resume old follow-ups.
            assert!(app.apply(Out::Info("context clear could not be confirmed".into())).is_none());
            assert_eq!(app.editor.text(), "old first\nold second\nunsent draft");
        }
    }

    #[tokio::test]
    async fn expands_arguments_and_runs_shell_blocks() {
        let cwd = std::env::temp_dir();
        let template = "Review $ARGUMENTS against:\n!`printf tested`";
        assert_eq!(expand(template, "the diff", &cwd).await, "Review the diff against:\ntested");

        let positional = expand("Move $1 to $2 (missing: $3)", "old.rs new.rs", &cwd).await;
        assert_eq!(positional, "Move old.rs to new.rs (missing: $3)");
    }

    #[test]
    fn a_skill_replaces_a_builtin_prompt_but_never_a_builtin_mechanism() {
        let root = std::env::temp_dir().join(format!("elmee-cmd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (name, body) in [
            ("audit", "---\ndescription: Audit the diff\n---\nAudit: $ARGUMENTS\n"),
            ("init", "House init rules: $ARGUMENTS\n"),
            ("clear", "should not shadow the builtin\n"),
        ] {
            fs::create_dir_all(root.join(".elmee/skills").join(name)).unwrap();
            fs::write(root.join(".elmee/skills").join(name).join("SKILL.md"), body).unwrap();
        }

        let commands = load(&root, Vec::new());
        let audit = commands.iter().find(|command| command.name == "audit").unwrap();
        assert_eq!(audit.description, "Audit the diff");
        assert!(audit.template.as_deref().unwrap().contains("$ARGUMENTS"));

        let init: Vec<_> = commands.iter().filter(|command| command.name == "init").collect();
        assert_eq!(init.len(), 1); // replaced in place, not duplicated
        assert!(init[0].template.as_deref().unwrap().contains("House init rules"));

        let clear: Vec<_> = commands.iter().filter(|command| command.name == "clear").collect();
        assert!(clear.len() == 1 && clear[0].template.is_none()); // the mechanism survives
        fs::remove_dir_all(root).unwrap();
    }
}
