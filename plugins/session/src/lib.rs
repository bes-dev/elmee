#![forbid(unsafe_code)]

//! JSONL persistence for core conversation trees.
//!
//! The core owns session semantics. This plugin owns the filesystem format
//! and recovery from a torn final append. A session is one file: everything
//! said and every move between branches is a line of it.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use elmee_core::{
    Error, Result,
    session::{Record, SESSION_SCHEMA, Session, SessionStore},
};

mod delete;
mod files;
pub use delete::delete_session;
pub use files::{Summary, in_directory, is_transcript, new_path, summarize};

/// Session artifacts are regular files, never a terminal, pipe or file alias.
/// NOFOLLOW is checked by open itself, not a racy path preflight.
pub(crate) fn open_regular(path: &Path, write: bool) -> std::io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags};
    let access = if write { OFlags::RDWR | OFlags::CREATE | OFlags::APPEND } else { OFlags::RDONLY };
    let fd = rustix::fs::open(
        path,
        access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let file = fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session artifact must be a regular file",
        ));
    }
    Ok(file)
}

pub fn open(path: Option<PathBuf>) -> Result<Session> {
    match path {
        Some(path) => Session::with_store(JsonlSessionStore::new(path)),
        None => Ok(Session::memory()),
    }
}

pub struct JsonlSessionStore {
    path: PathBuf,
    locked: Option<fs::File>,
}

impl JsonlSessionStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, locked: None }
    }

    fn file(&mut self) -> Result<&mut fs::File> {
        if self.locked.is_none() {
            use std::os::unix::fs::MetadataExt;
            if let Some(parent) = self.path.parent().filter(|path| !path.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let file = open_regular(&self.path, true)?;
            file.try_lock().map_err(|error| {
                Error::Config(format!(
                    "session {} is already open by another process: {error}",
                    self.path.display()
                ))
            })?;
            // A deletion may have finished between opening the file and
            // locking it, and what is held would then be a file with no name:
            // everything written to it would vanish when it closed.
            let held = file.metadata()?;
            if !fs::symlink_metadata(&self.path)
                .is_ok_and(|named| (named.dev(), named.ino()) == (held.dev(), held.ino()))
            {
                return Err(Error::Config(format!(
                    "session {} was deleted while it was being opened",
                    self.path.display()
                )));
            }
            self.locked = Some(file);
        }
        Ok(self.locked.as_mut().expect("session file was initialized"))
    }
}

impl SessionStore for JsonlSessionStore {
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn load(&mut self) -> Result<Vec<Record>> {
        // A conversation nothing has been said in yet has no file: opening a
        // session is not a reason to leave one behind, and most of what used
        // to fill a session directory was sessions that were only opened.
        if fs::symlink_metadata(&self.path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
            return Ok(Vec::new());
        }
        let path = self.path.clone();
        mend(self.file()?, &path)?;
        read(&self.path, |_| true)
    }

    fn append(&mut self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file()?.write_all(&line)?;
        Ok(())
    }
}

/// Make the end of a transcript a place a record can be appended to.
///
/// A process that dies mid-write leaves half a record as the last line. A
/// reader can skip that, and did — but the writer then appended after it, which
/// turned a torn *last* line into a torn line in the middle, and the session
/// that had just been recovered refused to open the next time. So the writer,
/// holding the lock, cuts the file back to its last whole record first. What
/// was cut is kept beside the transcript rather than thrown away: it is the
/// only trace of what the dead process was saying.
fn mend(file: &mut fs::File, path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut bytes)?;
    let whole = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let start = whole.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
    let last = &whole[start..];
    if last.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    if serde_json::from_slice::<Record>(last).is_ok() {
        // A whole record that lost only its newline: finish the line, or the
        // next record would be written onto the end of this one.
        if whole.len() == bytes.len() {
            file.write_all(b"\n")?;
        }
        return Ok(());
    }
    let mut torn = open_regular(&delete::sidecar(path, ".torn"), true)?;
    torn.write_all(last)?;
    torn.write_all(b"\n")?;
    torn.sync_all()?;
    file.set_len(start as u64)?;
    file.sync_all()?;
    eprintln!("elmee: {} ended in half a record; it was moved to {}.torn", path.display(), path.display());
    Ok(())
}

/// The canonical reader. `keep` lets a consumer that wants one kind of record
/// skip the rest before they are parsed, without owning a second parser.
fn read(path: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<Record>> {
    let mut lines = BufReader::new(open_regular(path, false)?).lines().peekable();
    let mut records = Vec::new();
    while let Some(line) = lines.next() {
        let line = line?;
        if line.trim().is_empty() || !keep(&line) {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(Record::Entry(entry)) if entry.schema > SESSION_SCHEMA => {
                return Err(Error::Config(format!(
                    "session {} uses schema {}, but this elmee supports through {}; upgrade elmee",
                    path.display(),
                    entry.schema,
                    SESSION_SCHEMA
                )));
            }
            Ok(record) => records.push(record),
            Err(error) if lines.peek().is_none() => {
                eprintln!("elmee: ignoring incomplete last line of {}: {error}", path.display());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use elmee_core::message::Message;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("elmee-{name}-{}.jsonl", std::process::id()))
    }

    #[test]
    fn clearing_switching_and_going_back_survive_reopen_without_erasing_history() {
        let path = path("durable-head");
        let mut session = open(Some(path.clone())).unwrap();
        let first = session.push(Message::user("first")).unwrap();
        session.push(Message::user("second")).unwrap();
        session.reset().unwrap();
        assert!(session.messages().is_empty());
        drop(session);
        let mut session = open(Some(path.clone())).unwrap();
        assert!(session.messages().is_empty());
        assert_eq!(session.entries().len(), 2, "clearing the context erases nothing");
        session.checkout(first).unwrap();
        drop(session);
        let mut session = open(Some(path.clone())).unwrap();
        assert_eq!(session.messages().len(), 1);
        let branched = session.push(Message::user("new branch")).unwrap();
        session.checkpoint("tree-before-branch".into()).unwrap();
        drop(session);
        let mut session = open(Some(path.clone())).unwrap();
        assert_eq!(session.messages().len(), 2, "what is said next continues the chosen branch");
        assert_eq!(session.messages()[1].content.as_deref(), Some("new branch"));
        let taken_back = session.rewind(branched).unwrap();
        assert_eq!(taken_back.text, "new branch");
        assert_eq!(taken_back.checkpoint.as_deref(), Some("tree-before-branch"), "kept in the journal");
        assert_eq!(session.branches().len(), 2, "both lines of conversation are still there");
        drop(session);
        let mut session = open(Some(path.clone())).unwrap();
        assert_eq!(session.messages().len(), 1);
        session.reset().unwrap();
        session.push(Message::user("after clear")).unwrap();
        drop(session);
        let session = open(Some(path.clone())).unwrap();
        assert_eq!(session.messages().len(), 1);
        assert_eq!(session.messages()[0].content.as_deref(), Some("after clear"));
        drop(session);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_branch_that_is_not_in_the_journal_is_refused() {
        let path = path("head-errors");
        fs::write(&path, "{\"id\":1,\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n{\"head\":99}\n")
            .unwrap();
        assert!(open(Some(path.clone())).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_a_second_writer_for_the_same_session() {
        let path = path("locked");
        let mut first = open(Some(path.clone())).unwrap();
        assert!(!path.exists(), "a session nothing was said in leaves no file");
        first.push(Message::user("mine")).unwrap();
        let mut second = JsonlSessionStore::new(path.clone());
        assert!(second.load().is_err(), "once it is written to, it has one writer");
        drop(first);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_a_newer_schema() {
        let path = path("future");
        fs::write(&path, r#"{"schema":999,"id":1,"message":{"role":"user","content":"future"}}"#).unwrap();
        let error = match open(Some(path.clone())) {
            Ok(_) => panic!("future session schema was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("uses schema 999") && error.contains("upgrade elmee"), "{error}");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_torn_final_line_costs_only_that_entry() {
        let path = path("torn");
        fs::write(
            &path,
            "{\"id\":1,\"message\":{\"role\":\"user\",\"content\":\"kept\"}}\n\
             {\"id\":2,\"parent\":1,\"message\":{\"role\":\"assist",
        )
        .unwrap();
        let session = open(Some(path.clone())).unwrap();
        assert_eq!(session.messages()[0].content.as_deref(), Some("kept"));

        fs::write(&path, "{\"id\":1,\"bogus\":\n{\"id\":2,\"message\":{\"role\":\"user\"}}\n").unwrap();
        assert!(open(Some(path.clone())).is_err());
        fs::remove_file(&path).unwrap();
        let _ = fs::remove_file(delete::sidecar(&path, ".torn"));
    }

    /// The whole cycle, not only the first read: a session recovered from a
    /// torn append must still open after it has been written to again. It
    /// used to append after the torn half-record, which moved the damage from
    /// the last line — forgiven — to the middle, which is not.
    #[test]
    fn a_session_recovered_from_a_torn_append_survives_being_continued() {
        let path = path("torn-continue");
        let torn = delete::sidecar(&path, ".torn");
        let mut session = open(Some(path.clone())).unwrap();
        session.push(Message::user("before the crash")).unwrap();
        drop(session);
        let half = b"{\"id\":2,\"parent\":1,\"message\":{\"role\":\"assist";
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(half).unwrap();

        let mut session = open(Some(path.clone())).unwrap();
        assert_eq!(session.messages().len(), 1, "the half record is not a message");
        session.push(Message::user("after the crash")).unwrap();
        drop(session);

        let session = open(Some(path.clone())).expect("a continued session opens again");
        let said: Vec<_> =
            session.messages().iter().filter_map(|message| message.content.as_deref()).collect();
        assert_eq!(said, ["before the crash", "after the crash"]);
        assert_eq!(fs::read(&torn).unwrap(), [half.as_slice(), b"\n"].concat(), "what was cut is kept");
        drop(session);

        // A whole record that lost only its newline is finished, not cut.
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.trim_end()).unwrap();
        let mut session = open(Some(path.clone())).unwrap();
        session.push(Message::user("third")).unwrap();
        drop(session);
        assert_eq!(open(Some(path.clone())).unwrap().messages().len(), 3);
        fs::remove_file(path).unwrap();
        fs::remove_file(torn).unwrap();
    }

    #[test]
    fn resumes_appended_history() {
        let path = path("resume");
        let mut first = open(Some(path.clone())).unwrap();
        first.push(Message::user("hello")).unwrap();
        drop(first);
        assert_eq!(open(Some(path.clone())).unwrap().messages().len(), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn shortened_tool_output_stays_shortened_after_reopening_and_whole_in_the_file() {
        let path = path("elide");
        let long = format!("first\n{}\nlast", "x".repeat(5_000));
        let mut session = open(Some(path.clone())).unwrap();
        session.push(Message::user("read it")).unwrap();
        session.push(Message::tool("1", long)).unwrap();
        session.push(Message::assistant_text("read")).unwrap();
        assert!(session.elide(0).unwrap() > 4_000);
        drop(session);
        let session = open(Some(path.clone())).unwrap();
        let shown = session.messages()[1].content.as_deref().unwrap();
        assert!(shown.len() < 600 && shown.starts_with("first") && shown.ends_with("last"), "{shown}");
        assert!(
            fs::read_to_string(&path).unwrap().contains(&"x".repeat(5_000)),
            "the journal keeps all of it"
        );
        drop(session);
        fs::remove_file(path).unwrap();
    }
}
