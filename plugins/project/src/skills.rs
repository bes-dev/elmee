//! Skills: instructions the agent loads only when a task calls for them.
//!
//! Discovery only. Counting used to live here, on the argument that discovery
//! and use-counting are the same subject — and they are, for skills. But a
//! skill is one rung of a ladder the agent learns down, and the question worth
//! answering is which of *everything* it taught itself has since been used. So
//! the counter moved to `usage`, which counts every kind by the same rule, and
//! this file went back to answering one question.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use crate::Result;

pub struct Skill {
    pub name: String,
    pub description: String,
    /// What a person is expected to type after `/name`, where the skill says.
    pub hint: Option<String>,
    pub path: PathBuf,
}

impl Skill {
    /// The instructions themselves, without the metadata block above them.
    pub fn body(&self) -> Result<String> {
        let content = fs::read_to_string(&self.path)?;
        let body =
            content.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---\n")).map(|(_, body)| body);
        Ok(body.unwrap_or(&content).trim().to_owned())
    }
}

/// Where skills may live, nearest first: the project, then the user.
fn roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".elmee/skills"), cwd.join(".agents/skills")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".elmee/skills"));
        roots.push(PathBuf::from(home).join(".agents/skills"));
    }
    roots
}

pub fn discover(cwd: &Path) -> Result<Vec<Skill>> {
    let mut skills = Vec::new();
    let mut names = BTreeSet::new();
    // Roots are nearest first. Resolve duplicate names here, before callers
    // see them, so the body shown in the prompt, the body read by the agent,
    // and the usage count all refer to the same skill.
    for root in roots(cwd) {
        let mut paths = Vec::new();
        collect(&root, &mut paths)?;
        paths.sort();
        for path in paths {
            let skill = parse(path)?;
            if names.insert(skill.name.clone()) {
                skills.push(skill);
            }
        }
    }
    Ok(skills)
}

/// How deep a skill may be nested. A bound rather than a cycle check because
/// symlinks are followed here: a skill collection is normally installed by
/// linking a checkout into `skills/`, and a directory entry's own type says
/// "symlink" rather than what it points at, so asking the entry would have
/// made every shared skill invisible. Following them means a loop is possible;
/// eight levels is far past any real layout and ends one.
const MAX_DEPTH: usize = 8;

fn collect(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    collect_within(directory, MAX_DEPTH, output)
}

fn collect_within(directory: &Path, depth: usize, output: &mut Vec<PathBuf>) -> Result<()> {
    if depth == 0 || !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_within(&path, depth - 1, output)?;
        } else if path.is_file() && path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
            output.push(path);
        }
    }
    Ok(())
}

fn parse(path: PathBuf) -> Result<Skill> {
    let content = fs::read_to_string(&path)?;
    let mut name =
        path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).unwrap_or("skill").to_owned();
    let mut description = String::new();
    let mut hint = None;
    let mut lines = content.lines();
    if lines.next() == Some("---") {
        for line in lines {
            if line == "---" {
                break;
            }
            if let Some(value) = line.strip_prefix("name:") {
                name = clean_yaml_scalar(value);
            } else if let Some(value) = line.strip_prefix("description:") {
                description = clean_yaml_scalar(value);
            } else if let Some(value) = line.strip_prefix("argument-hint:") {
                hint = Some(clean_yaml_scalar(value));
            }
        }
    }
    Ok(Skill { name, description, hint, path })
}

fn clean_yaml_scalar(value: &str) -> String {
    value.trim().trim_matches(['\'', '"']).to_owned()
}

/// Keep only the skills a caller named — a role that declared what it works
/// with, or an automation that attached exactly what its task needs.
pub fn only_named(skills: &mut Vec<Skill>, allowed: &[String]) {
    skills.retain(|skill| allowed.iter().any(|name| name == &skill.name));
}

/// Every skill costs prompt on every request, forever, so the listing has a
/// ceiling of `total` bytes of description — set by whoever knows how large
/// the model's window is. Names and paths always survive — the agent can
/// still read a skill it can see. Descriptions share what is left, handed out
/// a character at a time in turn, so a large harness degrades evenly instead
/// of going dark from one end.
///
/// A description is there to say when the skill applies, which takes a
/// sentence or two; what it does at length is in the file.
pub fn budget(skills: &mut [Skill], total: usize) {
    const PER_SKILL: usize = 320;
    for skill in skills.iter_mut() {
        skill.description = elmee_core::text::clip(&skill.description, PER_SKILL);
    }
    // Shortest first, each taking an equal share of what is left; whatever a
    // short description does not use widens the share for the rest.
    let mut order: Vec<usize> = (0..skills.len()).collect();
    order.sort_by_key(|index| skills[*index].description.len());
    let count = skills.len();
    let mut left = total;
    for (spent, index) in order.into_iter().enumerate() {
        let share = left / (count - spent);
        truncate(&mut skills[index].description, share);
        left = left.saturating_sub(skills[index].description.len());
    }
}

/// Shorten to at most `limit` bytes, ellipsis included — the caller is
/// dividing a fixed budget, so overshooting starves whoever comes next.
fn truncate(text: &mut String, limit: usize) {
    const ELLIPSIS: char = '…';
    if text.len() > limit {
        let kept = elmee_core::text::head(text, limit.saturating_sub(ELLIPSIS.len_utf8())).len();
        text.truncate(kept);
        text.push(ELLIPSIS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listing_stays_within_budget_and_degrades_evenly() {
        let skill = |name: &str, description: String| Skill {
            hint: None,
            name: name.into(),
            description,
            path: PathBuf::from("/skills").join(name),
        };
        let mut skills: Vec<Skill> =
            (0..40).map(|index| skill(&format!("s{index}"), "x".repeat(4000))).collect();
        skills.push(skill("brief", "short one".into()));
        budget(&mut skills, 6 * 1024);

        let total: usize = skills.iter().map(|skill| skill.description.len()).sum();
        assert!(total <= 6 * 1024, "listing over budget: {total}");
        // A short description is never cut, and the long ones share the rest
        // equally rather than the first few taking it all.
        assert_eq!(skills.last().unwrap().description, "short one");
        let long: Vec<usize> = skills[..40].iter().map(|skill| skill.description.len()).collect();
        assert!(long.iter().max().unwrap() - long.iter().min().unwrap() <= 4);
        assert!(long[0] > 100, "descriptions should not collapse to nothing: {}", long[0]);
    }

    /// A collection of skills is normally installed by linking a checkout into
    /// `skills/`, which is how every harness that shares them does it. Asking
    /// the directory entry for its type answers "symlink", so the whole
    /// collection was silently invisible — present on disk, absent from the
    /// prompt, with nothing anywhere saying why.
    #[test]
    fn a_skill_collection_linked_in_from_elsewhere_is_found() {
        let root = std::env::temp_dir().join(format!("elmee-skills-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let elsewhere = root.join("checkout/kb");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("SKILL.md"), "---\nname: kb\ndescription: the vault\n---\nbody").unwrap();
        let skills = root.join("home/.elmee/skills");
        fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&elsewhere, skills.join("kb")).unwrap();

        let mut found = Vec::new();
        collect(&skills, &mut found).unwrap();
        assert_eq!(found.len(), 1, "the linked collection is what a person installed: {found:?}");
        assert_eq!(parse(found.remove(0)).unwrap().description, "the vault");

        // Following links means a cycle is possible; the depth bound ends one
        // instead of the walk never returning.
        std::os::unix::fs::symlink(&skills, elsewhere.join("loop")).unwrap();
        let mut looped = Vec::new();
        collect(&skills, &mut looped).unwrap();
        assert!(!looped.is_empty(), "a loop must not swallow the real skill");
        let _ = fs::remove_dir_all(root);
    }

    /// A skill one directory deeper than usual is still found, so the report
    /// and the prompt cannot disagree about what exists.
    #[test]
    fn a_skill_nested_below_the_root_is_still_discovered() {
        let root = std::env::temp_dir().join(format!("elmee-skills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let nested = root.join(".agents/skills/group/review");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("SKILL.md"), "---\nname: review\ndescription: Review changes\n---\nBODY\n")
            .unwrap();
        let found = discover(&root).unwrap();
        assert!(found.iter().any(|skill| skill.name == "review"), "a nested skill must be found");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_nearest_skill_wins_when_two_roots_use_the_same_name() {
        let root = std::env::temp_dir().join(format!("elmee-skill-priority-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let nearest = root.join(".elmee/skills/review");
        let farther = root.join(".agents/skills/review");
        fs::create_dir_all(&nearest).unwrap();
        fs::create_dir_all(&farther).unwrap();
        fs::write(nearest.join("SKILL.md"), "---\nname: review\ndescription: nearest\n---\n").unwrap();
        fs::write(farther.join("SKILL.md"), "---\nname: review\ndescription: farther\n---\n").unwrap();

        let matches: Vec<_> =
            discover(&root).unwrap().into_iter().filter(|skill| skill.name == "review").collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].description, "nearest");
        fs::remove_dir_all(root).unwrap();
    }
}
