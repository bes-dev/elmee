//! One patch, any number of files, all of it or none of it.
//!
//! The grammar is the one Codex uses, chosen because models already write it
//! fluently and nothing here has to teach them. What it adds over naming a
//! file and a string is atomicity: a rename applied to half a tree is worse
//! than one never started, because the build then breaks in a way that reads
//! like the model's reasoning rather than like a tool that stopped in the
//! middle. So the whole patch is resolved in memory against the workspace, and
//! only then does anything reach disk — and if a write fails after others
//! have landed, those are put back before the failure is reported. That is
//! undo, not a filesystem transaction: a crash in the middle of applying, or
//! a file that cannot be put back, still leaves a partial tree, and the error
//! names exactly which files.
//!
//! Hunks are placed exactly and in order. An `@@` header is an exact source
//! line after which that hunk must occur; the preceding hunk is the implicit
//! lower bound when there is no header.

use std::path::{Path, PathBuf};

use super::{
    edit::{after_line, edit},
    schema::PatchArgs,
    workspace::{Fingerprint, Workspace},
};
use crate::{Error, Result};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
pub(crate) const ADD: &str = "*** Add File: ";
pub(crate) const UPDATE: &str = "*** Update File: ";
pub(crate) const DELETE: &str = "*** Delete File: ";
pub(crate) const MOVE: &str = "*** Move to: ";

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    Add,
    Update,
    Delete,
}

#[derive(Default)]
pub struct Hunk {
    pub anchor: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
}

/// One file's part of a patch. Public because a frontend that shows an edit
/// as an edit needs the same reading of the grammar the tool applies.
pub struct Change {
    pub kind: Kind,
    pub path: String,
    pub move_to: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// What a resolved patch is about to do, once every hunk has been placed.
enum Stage {
    Write { relative: PathBuf, text: String, expected: Option<Fingerprint> },
    Remove { relative: PathBuf, expected: Fingerprint },
}

impl Stage {
    fn target(&self) -> &Path {
        match self {
            Stage::Write { relative, .. } | Stage::Remove { relative, .. } => relative,
        }
    }
}

/// Put back what a patch had already changed when a later step failed, newest
/// first, and say which files could not be put back.
fn undo(done: Vec<(&Path, Option<Vec<u8>>)>) -> Vec<String> {
    let mut left = Vec::new();
    for (target, before) in done.into_iter().rev() {
        let restored = match before {
            Some(bytes) => elmee_plugin_atomic::write(
                target,
                &bytes,
                elmee_plugin_atomic::Options { preserve_mode: true, ..Default::default() },
            ),
            None => std::fs::remove_file(target),
        };
        if restored.is_err() {
            left.push(target.display().to_string());
        }
    }
    left
}

pub(super) fn apply(workspace: &mut Workspace, args: &PatchArgs) -> Result<String> {
    let changes = parse(&args.patch)?;
    let mut staged: Vec<Stage> = Vec::new();
    let mut report: Vec<String> = Vec::new();
    for change in &changes {
        match change.kind {
            Kind::Add => {
                let relative = workspace.creation_target(&change.path)?;
                let mut text = change.hunks.first().map(|hunk| hunk.new.join("\n")).unwrap_or_default();
                if !text.is_empty() {
                    text.push('\n');
                }
                staged.push(Stage::Write { relative, text, expected: None });
                report.push(format!("A {}", change.path));
            }
            Kind::Delete => {
                let (relative, expected) = workspace.delete_source(&change.path)?;
                staged.push(Stage::Remove { relative, expected });
                report.push(format!("D {}", change.path));
            }
            Kind::Update => {
                let (_, relative, disk, expected) = workspace.edit_source(&change.path)?;
                // A second section for the same file continues the first
                // rather than fighting it over the same original text.
                let mut text = staged
                    .iter()
                    .rev()
                    .find_map(|stage| match stage {
                        Stage::Write { relative: at, text, .. } if *at == relative => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or(disk);
                let mut cursor = 0;
                for (index, hunk) in change.hunks.iter().enumerate() {
                    if let Some(anchor) = &hunk.anchor {
                        let after_anchor = after_line(&text, anchor, 0).map_err(|matches| {
                            let reason = if matches == 0 {
                                "was not found as an exact line".to_owned()
                            } else {
                                format!("occurs {matches} times; use a unique exact line")
                            };
                            Error::Tool(format!(
                                "hunk {} of {} in {} did not apply, so nothing in this patch was \
                                 written: @@ anchor {anchor:?} {reason}",
                                index + 1,
                                change.hunks.len(),
                                change.path
                            ))
                        })?;
                        cursor = cursor.max(after_anchor);
                    }
                    let old = hunk.old.join("\n");
                    if old.is_empty() {
                        if hunk.anchor.is_some() {
                            let ending = if text[..cursor].ends_with("\r\n") { "\r\n" } else { "\n" };
                            let mut new = hunk.new.join("\n").replace('\n', ending);
                            if cursor > 0 && !text[..cursor].ends_with('\n') {
                                new.insert_str(0, ending);
                            }
                            if !new.is_empty() && !new.ends_with(ending) {
                                new.push_str(ending);
                            }
                            text.insert_str(cursor, &new);
                            cursor += new.len();
                            continue;
                        }
                        return Err(Error::Tool(format!(
                            "hunk {} of {} in {} has neither text to replace nor an @@ anchor",
                            index + 1,
                            change.hunks.len(),
                            change.path
                        )));
                    }
                    let (after, next) =
                        edit(&text, &old, &hunk.new.join("\n"), cursor, index == 0 && hunk.anchor.is_none())
                            .map_err(|error| {
                                Error::Tool(format!(
                                    "hunk {} of {} in {} did not apply, so nothing in this patch was \
                             written: {error}",
                                    index + 1,
                                    change.hunks.len(),
                                    change.path
                                ))
                            })?;
                    text = after;
                    cursor = next;
                }
                match &change.move_to {
                    Some(destination) => {
                        let created = workspace.creation_target(destination)?;
                        staged.push(Stage::Write { relative: created, text, expected: None });
                        staged.push(Stage::Remove { relative, expected });
                        report.push(format!("R {} -> {destination}", change.path));
                    }
                    None => {
                        staged.retain(
                            |stage| !matches!(stage, Stage::Write { relative: at, .. } if *at == relative),
                        );
                        staged.push(Stage::Write { relative, text, expected: Some(expected) });
                        report.push(format!("M {}", change.path));
                    }
                }
            }
        }
    }
    // Recheck every target before the first write. Individual commits check
    // again, but this pass prevents an already-known collision late in a
    // multi-file patch from leaving the earlier files changed.
    for stage in &staged {
        match stage {
            Stage::Write { relative, expected, .. } => workspace.verify(relative, *expected)?,
            Stage::Remove { relative, expected } => workspace.verify(relative, Some(*expected))?,
        }
    }
    // Two steps on one file are two answers to what it should contain, and
    // the checks above pass for both while only the first can then land.
    for (index, stage) in staged.iter().enumerate() {
        if staged[..index].iter().any(|earlier| earlier.target() == stage.target()) {
            return Err(Error::Tool(format!(
                "this patch names {} more than once, so nothing in it was written; say what the file \
                 should become in one section",
                stage.target().display()
            )));
        }
    }
    let mut done: Vec<(&Path, Option<Vec<u8>>)> = Vec::new();
    for stage in &staged {
        let before = std::fs::read(stage.target()).ok();
        let applied = match stage {
            Stage::Write { relative, text, expected } => workspace.commit(relative, text, *expected),
            Stage::Remove { relative, expected } => workspace.remove(relative, *expected),
        };
        if let Err(error) = applied {
            let left = undo(done);
            return Err(Error::Tool(if left.is_empty() {
                format!("{error}; what this patch had already changed was put back, so nothing in it stands")
            } else {
                format!(
                    "{error}; the patch was applied in part and could not be fully undone — still \
                     changed: {}",
                    left.join(", ")
                )
            }));
        }
        done.push((stage.target(), before));
    }
    Ok(format!("Applied patch to {} file(s):\n{}", changes.len(), report.join("\n")))
}

pub fn parse(text: &str) -> Result<Vec<Change>> {
    let mut lines = text.lines().skip_while(|line| line.trim().is_empty());
    if lines.next().map(str::trim_end) != Some(BEGIN) {
        return Err(Error::Tool(format!("a patch must start with {BEGIN}")));
    }
    let mut changes: Vec<Change> = Vec::new();
    let mut ended = false;
    for line in lines {
        if line.trim_end() == END {
            ended = true;
            break;
        }
        let header = |prefix: &str| line.strip_prefix(prefix).map(|path| path.trim().to_owned());
        if let Some(path) = header(ADD) {
            changes.push(Change { kind: Kind::Add, path, move_to: None, hunks: vec![Hunk::default()] });
            continue;
        }
        if let Some(path) = header(UPDATE) {
            changes.push(Change { kind: Kind::Update, path, move_to: None, hunks: Vec::new() });
            continue;
        }
        if let Some(path) = header(DELETE) {
            changes.push(Change { kind: Kind::Delete, path, move_to: None, hunks: Vec::new() });
            continue;
        }
        let change = changes
            .last_mut()
            .ok_or_else(|| Error::Tool(format!("{line:?} comes before any *** File header")))?;
        if let Some(destination) = header(MOVE) {
            if change.kind != Kind::Update {
                return Err(Error::Tool(format!("{MOVE}is only allowed under {UPDATE}")));
            }
            change.move_to = Some(destination);
            continue;
        }
        match change.kind {
            Kind::Delete if line.trim().is_empty() => {}
            Kind::Delete => {
                return Err(Error::Tool(format!(
                    "{DELETE}{} takes no lines, but found {line:?}",
                    change.path
                )));
            }
            Kind::Add => match line.strip_prefix('+') {
                Some(rest) => change.hunks[0].new.push(rest.to_owned()),
                None => {
                    return Err(Error::Tool(format!(
                        "every line of an added file must start with '+', but found {line:?}"
                    )));
                }
            },
            Kind::Update => {
                if line == "@@" || line.starts_with("@@ ") {
                    let anchor =
                        line.strip_prefix("@@ ").filter(|anchor| !anchor.is_empty()).map(str::to_owned);
                    change.hunks.push(Hunk { anchor, ..Hunk::default() });
                    continue;
                }
                let hunk = match change.hunks.last_mut() {
                    Some(hunk) => hunk,
                    None => {
                        change.hunks.push(Hunk::default());
                        change.hunks.last_mut().expect("just pushed")
                    }
                };
                let rest = || line[1..].to_owned();
                match line.chars().next() {
                    Some(' ') => {
                        hunk.old.push(rest());
                        hunk.new.push(rest());
                    }
                    Some('-') => hunk.old.push(rest()),
                    Some('+') => hunk.new.push(rest()),
                    Some(_) | None => {
                        return Err(Error::Tool(format!(
                            "a hunk line must start with ' ', '-' or '+', but found {line:?}"
                        )));
                    }
                }
            }
        }
    }
    if !ended {
        return Err(Error::Tool(format!("a patch must end with {END}")));
    }
    if changes.is_empty() {
        return Err(Error::Tool("this patch changes nothing".into()));
    }
    // A section with neither hunks nor a destination asks for nothing, which
    // is more likely a patch that lost its body than a patch that meant it.
    if let Some(change) = changes
        .iter()
        .find(|change| change.kind == Kind::Update && change.hunks.is_empty() && change.move_to.is_none())
    {
        return Err(Error::Tool(format!("{UPDATE}{} has no hunks", change.path)));
    }
    Ok(changes)
}
