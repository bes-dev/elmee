#![forbid(unsafe_code)]

//! Slash commands, independent of the frontend that presents them.
//!
//! A skill is what a person may invoke by name: `/name arguments` sends the
//! skill's instructions as the prompt, with the arguments filled in. The
//! model may also pick the same skill up on its own, from the list in its
//! system prompt — one file, reached from either side. A frontend supplies
//! its own built-ins and chooses how commands are displayed.

use std::path::Path;

#[derive(Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub hint: Option<String>,
    pub template: Option<String>,
}

/// Add the skills of a project and its user to frontend-provided built-ins. A
/// skill may replace a built-in prompt of the same name, but never a
/// frontend mechanism.
pub fn load(cwd: &Path, mut commands: Vec<Command>) -> Vec<Command> {
    for skill in elmee_plugin_project::skills::discover(cwd).unwrap_or_default() {
        let existing = commands.iter().position(|command| command.name == skill.name);
        if existing.is_some_and(|index| commands[index].template.is_none()) {
            continue;
        }
        let Ok(body) = skill.body() else { continue };
        // It opens with what the person typed, which is what a list of
        // conversations will call this one; then the skill is named and
        // placed, because it speaks of files beside itself and a prompt has
        // no directory of its own.
        let directory = skill.path.parent().unwrap_or(Path::new(".")).display();
        let command = Command {
            template: Some(format!(
                "/{name} $ARGUMENTS\n\nFollow the skill `{name}` (in {directory}):\n\n{body}",
                name = skill.name
            )),
            name: skill.name,
            description: elmee_core::text::clip(&skill.description, 80),
            hint: skill.hint,
        };
        match existing {
            Some(index) => commands[index] = command,
            None => commands.push(command),
        }
    }
    commands
}

/// Fill positional arguments and trusted shell substitutions in a prompt.
///
/// Arguments a template has no place for are not dropped: they are what the
/// person asked for, and are said after it.
pub async fn expand(template: &str, arguments: &str, cwd: &Path) -> String {
    let mut text = expand_in(template, arguments, cwd).await;
    if !arguments.is_empty() && !template.contains("$ARGUMENTS") && !template.contains("$1") {
        text.push_str(&format!("\n\n{arguments}"));
    }
    text
}

async fn expand_in(template: &str, arguments: &str, cwd: &Path) -> String {
    let words = arguments.split_whitespace().collect::<Vec<_>>();
    let text = fill_positions(&template.replace("$ARGUMENTS", arguments), &words);
    let mut expanded = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find("!`") {
        let Some(end) = rest[start + 2..].find('`') else { break };
        let command = &rest[start + 2..start + 2 + end];
        expanded.push_str(&rest[..start]);
        expanded.push_str(&run_substitution(command, cwd).await);
        rest = &rest[start + 3 + end..];
    }
    expanded.push_str(rest);
    expanded
}

/// Replace `$N` with the N-th word, matching the whole digit run: `$10` with
/// one argument stays literal instead of becoming the first word plus a
/// stray `0`, and any absent position survives as itself.
fn fill_positions(text: &str, words: &[&str]) -> String {
    let mut filled = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        let end = at + 1 + rest[at + 1..].bytes().take_while(u8::is_ascii_digit).count();
        match rest[at + 1..end].parse::<usize>().ok().and_then(|n| words.get(n.checked_sub(1)?)) {
            Some(word) => {
                filled.push_str(&rest[..at]);
                filled.push_str(word);
            }
            None => filled.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    filled.push_str(rest);
    filled
}

/// How long a `` !`cmd` `` block may run. A hanging substitution used to be a
/// blocking `std::process` wait on the frontend's own thread — a frozen TUI.
const SUBSTITUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

async fn run_substitution(command: &str, cwd: &Path) -> String {
    let mut process = tokio::process::Command::new("sh");
    elmee_plugin_process::env::apply(process.as_std_mut());
    process.arg("-c").arg(command).current_dir(cwd);
    match tokio::time::timeout(SUBSTITUTION_TIMEOUT, elmee_plugin_process::scope(&mut process).output()).await
    {
        Err(_) => format!("[{command}: timed out after {}s]", SUBSTITUTION_TIMEOUT.as_secs()),
        Ok(Err(error)) => format!("[{command}: {error}]"),
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text.truncate(text.trim_end().len());
            // Bounded at the size any other tool result may be, because this
            // goes straight into the prompt.
            elmee_plugin_tools::tools::truncate(text, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn arguments_and_shell_blocks_expand() {
        let template = "Review $1 and $ARGUMENTS: !`printf tested`";
        assert_eq!(expand(template, "the diff", Path::new(".")).await, "Review the and the diff: tested");
    }

    #[tokio::test]
    async fn multi_digit_positions_are_not_partially_replaced() {
        let arguments = (1..=12).map(|number| format!("a{number}")).collect::<Vec<_>>().join(" ");
        assert_eq!(expand("$12 $10 $1", &arguments, Path::new(".")).await, "a12 a10 a1");
        // An absent position stays literal instead of being corrupted by the
        // present one it starts with: `$10` with one argument is not `<arg>0`.
        assert_eq!(expand("$10 keeps $2 and $0", "only", Path::new(".")).await, "$10 keeps $2 and $0");
    }

    #[tokio::test]
    async fn a_skill_is_a_command_that_opens_with_what_was_typed() {
        let root = std::env::temp_dir().join(format!("elmee-skill-command-{}", std::process::id()));
        let directory = root.join(".elmee/skills/ship");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: ship\ndescription: Ship it\nargument-hint: <version>\n---\nRun scripts/release.sh\n",
        )
        .unwrap();
        let commands = load(&root, Vec::new());
        let ship = commands.iter().find(|command| command.name == "ship").expect("the skill, as a command");
        assert_eq!((ship.description.as_str(), ship.hint.as_deref()), ("Ship it", Some("<version>")));
        let prompt = expand(ship.template.as_deref().unwrap(), "1.2", &root).await;
        assert!(prompt.starts_with("/ship 1.2\n"), "it opens with what was typed: {prompt}");
        assert!(prompt.contains("skills/ship)") && prompt.contains("Run scripts/release.sh"), "{prompt}");
        assert_eq!(
            expand("Fixed words.", "extra", &root).await,
            "Fixed words.\n\nextra",
            "nothing typed is dropped"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
