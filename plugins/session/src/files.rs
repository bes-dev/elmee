//! Session files on disk: which of them are transcripts, what a new one is
//! named, and what a past one was about.
//!
//! Naming and recognising a stored prompt are facts about the format this
//! crate writes, so they live beside the writer rather than in whichever
//! application happens to list sessions.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use elmee_core::{
    Error, Result,
    message::{MessageOrigin, Role},
    session::Record,
};

/// A conversation transcript, by its name.
pub fn is_transcript(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("jsonl")
}

/// A fresh transcript in `directory`: when it started and which process started
/// it, so two agents in one project cannot claim the same file.
pub fn new_path(directory: &Path) -> Result<PathBuf> {
    /// One process may open several sessions in one project, and two asked for
    /// in the same millisecond used to be handed the same name — so the second
    /// found the first already holding the writer lock and could not start.
    /// An editor opening two buffers is exactly that case.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fs::create_dir_all(directory)?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Config(error.to_string()))?
        .as_millis();
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(directory.join(format!("{millis}-{}-{serial}.jsonl", std::process::id())))
}

/// A past session as a person recognises it: when it was, what it was about,
/// and how much of it there is.
///
/// The picker used to list file names, which are millisecond timestamps —
/// technically a list of sessions and practically a list of nothing. What
/// identifies a session is the first thing that was asked of it.
#[derive(Debug)]
pub struct Summary {
    pub path: PathBuf,
    pub subject: String,
    pub prompts: usize,
    pub age: String,
    /// When it was last written to, in seconds since the epoch.
    pub modified: u64,
}

/// Read the head of each session for its subject and size.
pub fn summarize(paths: Vec<PathBuf>) -> Vec<Summary> {
    paths
        .into_iter()
        .map(|path| {
            let mut subject = String::new();
            let mut title = None;
            let mut prompts = 0;
            // Most of a session is tool output and replies, and parsing all of
            // it to find the prompts costs more than reading the file. Entries
            // are written compactly by `append`, so this substring is exact;
            // what survives it is still parsed by the canonical reader, which
            // is what keeps a torn final append working.
            let wanted = |line: &str| line.contains(r#""role":"user""#) || line.starts_with(r#"{"title""#);
            for record in crate::read(&path, wanted).unwrap_or_default() {
                let entry = match record {
                    Record::Entry(entry) => entry,
                    Record::Title { title: named } => {
                        title = Some(named);
                        continue;
                    }
                    _ => continue,
                };
                if !matches!(entry.message.role, Role::User)
                    || entry.message.origin == MessageOrigin::Background
                {
                    continue;
                }
                prompts += 1;
                if subject.is_empty() {
                    subject = entry.message.content.unwrap_or_default().replace(char::is_control, " ");
                }
            }
            let written = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
            let age = written
                .and_then(|at| at.elapsed().ok())
                .map_or_else(String::new, |ago| how_long(ago.as_secs()));
            let modified =
                written.and_then(|at| at.duration_since(UNIX_EPOCH).ok()).map_or(0, |since| since.as_secs());
            Summary { path, subject: title.unwrap_or(subject), prompts, age, modified }
        })
        .collect()
}

fn how_long(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        86_400..2_592_000 => format!("{}d ago", seconds / 86_400),
        _ => "long ago".into(),
    }
}

/// The transcripts kept in `directory`, newest first. A link to one is not a
/// session.
pub fn in_directory(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !is_transcript(&path) {
            continue;
        }
        // Transcript readers reject file symlinks; discovery must not select
        // their targets (or fail the whole list on a dangling file alias).
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_file() {
            files.push((metadata.modified()?, path));
        }
    }
    files.sort_by(|a, b| b.cmp(a));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use elmee_core::message::Message;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("elmee-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_session_is_summarised_by_the_first_thing_asked_of_it() {
        let root = root("summary");
        let path = new_path(&root).unwrap();
        let mut session = crate::open(Some(path.clone())).unwrap();
        session.push(Message::system("prompt")).unwrap();
        session.push(Message::user("fix the tokenizer")).unwrap();
        session.push(Message::assistant_text("done")).unwrap();
        session.push(Message::background("e1", "<external_event>ping</external_event>")).unwrap();
        session.push(Message::user("now add tests")).unwrap();

        let summary = summarize(vec![path]).pop().expect("a summary");
        assert_eq!(summary.subject, "fix the tokenizer");
        assert_eq!(summary.prompts, 2, "what arrived on its own is not something asked");
        assert_eq!(summary.age, "just now");
        fs::remove_dir_all(root).unwrap();
    }

    /// An append cut short by a crash costs the summary only that entry.
    #[test]
    fn a_torn_final_line_still_has_a_subject() {
        let root = root("summary-torn");
        let torn = root.join("torn.jsonl");
        fs::write(
            &torn,
            "{\"id\":1,\"message\":{\"role\":\"user\",\"content\":\"kept\"}}\n\
             {\"id\":2,\"message\":{\"role\":\"user\",\"content\":\"lo",
        )
        .unwrap();

        let summaries = summarize(vec![torn]);
        assert_eq!(summaries[0].subject, "kept");
        assert_eq!(summaries[0].prompts, 1, "the torn entry costs only itself");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_selection_does_not_follow_file_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "session-index-links-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let transcript = root.join("real.jsonl");
        fs::write(&transcript, b"history").unwrap();
        std::os::unix::fs::symlink(&transcript, root.join("alias.jsonl")).unwrap();
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling.jsonl")).unwrap();
        assert_eq!(in_directory(&root).unwrap(), vec![transcript]);
        fs::remove_dir_all(root).unwrap();
    }
}
