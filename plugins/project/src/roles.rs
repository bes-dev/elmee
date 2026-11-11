//! What an agent is, as against what it runs on.
//!
//! A role is a saved set of launch arguments: the instructions the agent
//! carries, the permission it starts under, and which of the project's skills
//! and tools it is offered. It is deliberately not a new concept anywhere else
//! in the program — `--role reviewer` is expanded into ordinary flags before
//! arguments are parsed, and after that nothing knows roles exist. That is the
//! whole mechanism, and it is why a role can set anything a flag can set: a
//! flag added later becomes something a role can name, with no code here.
//!
//! It holds no model. What an agent is belongs to the project and travels in
//! the repository; what it runs on belongs to the machine and lives in
//! `~/.elmee/models.json`. The same reviewer should ride a frontier model on
//! one desk and a local one on another, and folding the two together is how
//! "role" came to mean "model" in the first place.
//!
//! Because the expansion is spliced in place of the flag, anything typed after
//! it wins: `--role reviewer --permission-mode full` is `full`, since parsing
//! is a loop where the last assignment stands. A role sets defaults; the
//! command line overrides them.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Where roles may live, nearest first: the project, then the user. The same
/// rule skills use, for the same reason — a project's own answer beats a
/// habit carried between projects.
fn roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".elmee/roles")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".elmee/roles"));
    }
    roots
}

/// Two roles every project has without writing a file, because delegation
/// needs them and a project with no `.elmee/roles/` must still be able to
/// delegate.
///
/// `research` once claimed read-only by setting a permission mode, and then
/// by withholding the shell — which left it a reader that could not run `rg`
/// to find what it was asked about. Nothing inside elmee bounds an agent, and
/// the working tree is snapshotted before every turn, so the promise a
/// researcher keeps is the one in its instructions: it is offered the shell
/// and told what not to do with it, and it is not offered the editor at all.
///
/// It names only tools that always exist. `--tools` is checked against the
/// composition, so a built-in role that mentioned `web_search` would fail to
/// launch wherever no search backend is configured.
fn built_in(name: &str) -> Option<&'static str> {
    match name {
        "research" => Some(
            "---\ntools: read, exec, plan\n---\n\
             You are here to find out, not to change. Read, search with rg, run \
             what answers a question, and report; write nothing and install \
             nothing, because the agent that asked will act on what you found.\n",
        ),
        "workspace" => Some("---\n---\n"),
        _ => None,
    }
}

/// Turn `--role NAME` into the flags it stands for, in place.
///
/// Everything else in `argv` is passed through untouched, including a second
/// `--role`: naming two is a mistake worth reporting rather than a merge with
/// invented precedence.
pub fn expand(cwd: &Path, argv: Vec<String>) -> Result<Vec<String>> {
    if !argv.iter().any(|arg| arg == "--role") {
        // A role called `default` is what a session is when nobody says
        // otherwise: how a project narrows the skills and tools every agent
        // in it starts with. Spliced in first, so anything typed still wins.
        let named = roots(cwd).into_iter().any(|root| root.join("default.md").is_file());
        let mut expanded = if named { flags(cwd, "default")? } else { Vec::new() };
        expanded.extend(argv);
        return Ok(expanded);
    }
    let mut expanded = Vec::with_capacity(argv.len() + 8);
    let mut seen = false;
    let mut rest = argv.into_iter();
    while let Some(arg) = rest.next() {
        if arg != "--role" {
            expanded.push(arg);
            continue;
        }
        let name = rest.next().ok_or_else(|| Error::Config("missing value for --role".into()))?;
        if std::mem::replace(&mut seen, true) {
            return Err(Error::Config("only one --role: name the one this agent is".into()));
        }
        expanded.extend(flags(cwd, &name)?);
    }
    Ok(expanded)
}

/// The flags one role names.
fn flags(cwd: &Path, name: &str) -> Result<Vec<String>> {
    let text = read(cwd, name)?;
    let (front, body) = split(&text);
    let mut flags = Vec::new();
    let mut replace = false;
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(['\'', '"']).to_owned();
        match key.trim() {
            "skills" => flags.extend(["--skills".to_owned(), list(&value)]),
            "tools" => flags.extend(["--tools".to_owned(), list(&value)]),
            "prompt" => replace = value == "replace",
            // `description` is for whoever reads the file, and an unknown key
            // is a typo the role's author will see in the diff. Neither is
            // worth failing a launch over.
            _ => {}
        }
    }
    let body = body.trim();
    if !body.is_empty() {
        let flag = if replace { "--system-prompt" } else { "--append-system-prompt" };
        flags.extend([flag.to_owned(), body.to_owned()]);
    }
    Ok(flags)
}

fn read(cwd: &Path, name: &str) -> Result<String> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err(Error::Config(format!("{name:?} is not a role name")));
    }
    for root in roots(cwd) {
        if let Ok(text) = std::fs::read_to_string(root.join(format!("{name}.md"))) {
            return Ok(text);
        }
    }
    built_in(name).map(str::to_owned).ok_or_else(|| {
        // Failing loudly is the point: a typo must not quietly hand the agent
        // the whole project instead of the slice the role names.
        Error::Config(format!(
            "no role {name:?} in .elmee/roles or ~/.elmee/roles; built-in roles: research, workspace"
        ))
    })
}

/// Frontmatter and body, the same shape a skill has.
fn split(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---\n") else { return ("", text) };
    match rest.split_once("\n---") {
        Some((front, body)) => (front, body.trim_start_matches(['-', '\n'])),
        None => (rest, ""),
    }
}

/// `[a, b]` or `a, b` — both are what people write, and neither is worth a
/// YAML parser.
fn list(value: &str) -> String {
    value
        .trim_matches(['[', ']'])
        .split(',')
        .map(|item| item.trim().trim_matches(['\'', '"']))
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("elmee-roles-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join(".elmee/roles")).unwrap();
        path
    }

    /// The whole mechanism: a role becomes flags, and what was typed after it
    /// still wins because parsing takes the last assignment.
    #[test]
    fn a_role_becomes_flags_and_the_command_line_still_wins() {
        let cwd = scratch("expand");
        std::fs::write(
            cwd.join(".elmee/roles/reviewer.md"),
            "---\ndescription: reads\nskills: [rust-review, testing]\n\
             tools: read, exec\n---\nYou are the reviewer.\n",
        )
        .unwrap();

        let argv = ["--role", "reviewer", "--tools", "read"].map(str::to_owned).to_vec();
        let flags = expand(&cwd, argv).unwrap();
        assert_eq!(
            flags,
            [
                "--skills",
                "rust-review,testing",
                "--tools",
                "read,exec",
                "--append-system-prompt",
                "You are the reviewer.",
                // What the person typed comes after, and the parser takes the
                // last assignment — so the role is a default, not a ceiling.
                "--tools",
                "read",
            ]
        );
        let _ = std::fs::remove_dir_all(cwd);
    }

    /// A typo must not quietly hand the agent everything the role was written
    /// to narrow, and delegation must work in a project with no roles at all.
    #[test]
    fn an_unknown_role_fails_and_the_built_in_ones_always_exist() {
        let cwd = scratch("unknown");
        let argv = ["--role", "revewer"].map(str::to_owned).to_vec();
        assert!(expand(&cwd, argv).unwrap_err().to_string().contains("no role"));

        let research = expand(&cwd, ["--role", "research"].map(str::to_owned).to_vec()).unwrap();
        assert_eq!(research[0], "--tools");
        for forbidden in ["apply_patch", "write_run"] {
            assert!(!research[1].contains(forbidden), "a researcher cannot {forbidden}: {}", research[1]);
        }
        assert!(research[1].contains("exec"), "and can search: {}", research[1]);
        assert!(expand(&cwd, ["--role", "workspace"].map(str::to_owned).to_vec()).unwrap().is_empty());

        // A name that could reach outside the two role directories is not one.
        let escape = expand(&cwd, ["--role", "../../etc/passwd"].map(str::to_owned).to_vec());
        assert!(escape.unwrap_err().to_string().contains("not a role name"));

        // Naming two roles is a mistake, not a merge with invented precedence.
        let both = ["--role", "research", "--role", "workspace"].map(str::to_owned).to_vec();
        assert!(expand(&cwd, both).unwrap_err().to_string().contains("only one --role"));
        let _ = std::fs::remove_dir_all(cwd);
    }

    /// `prompt: replace` takes the whole system prompt, which is the one thing
    /// in a role file that changes what the agent is rather than adding to it.
    #[test]
    fn a_role_may_replace_the_prompt_instead_of_adding_to_it() {
        let cwd = scratch("replace");
        std::fs::write(cwd.join(".elmee/roles/bare.md"), "---\nprompt: replace\n---\nOnly this.\n").unwrap();
        let flags = expand(&cwd, ["--role", "bare"].map(str::to_owned).to_vec()).unwrap();
        assert_eq!(flags, ["--system-prompt", "Only this."]);
        let _ = std::fs::remove_dir_all(cwd);
    }

    /// What a project wants every agent in it to start with, said once.
    #[test]
    fn a_role_called_default_applies_when_none_is_named() {
        let cwd = scratch("default");
        std::fs::write(cwd.join(".elmee/roles/default.md"), "---\nskills: pdf\n---\n").unwrap();
        std::fs::write(cwd.join(".elmee/roles/reviewer.md"), "---\nskills: review\n---\n").unwrap();
        let typed = |args: &[&str]| expand(&cwd, args.iter().map(|arg| (*arg).to_owned()).collect()).unwrap();
        assert_eq!(typed(&["--skills", "kb", "go"]), ["--skills", "pdf", "--skills", "kb", "go"]);
        assert_eq!(
            typed(&["--role", "reviewer"]),
            ["--skills", "review"],
            "a named role is the whole answer"
        );
        let _ = std::fs::remove_dir_all(cwd);
    }
}
