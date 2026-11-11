//! Explicit deletion of a closed local transcript and what is kept beside it.
use elmee_core::{Error, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Remove a transcript and what belongs to it, and say what was removed.
///
/// Done under the writer's lock, so a session that is open is refused, and
/// the transcript goes last, so a deletion cut short leaves it there to be
/// deleted again. One that is not there removed nothing, and says so.
pub fn delete_session(path: &Path) -> Result<Vec<PathBuf>> {
    if !super::is_transcript(path) {
        return Err(Error::Config(
            "deleting a session needs the path of its transcript, a .jsonl file".into(),
        ));
    }
    let file = crate::open_regular(path, false)?;
    file.try_lock().map_err(|error| {
        Error::Config(format!("session is open; close its agent before deletion: {error}"))
    })?;
    let mut removed = Vec::new();
    // The half record a crash left, the token ledger a budgeted run keeps
    // beside its transcript, then the transcript itself.
    for artifact in [sidecar(path, ".torn"), path.with_extension("tokens"), path.to_owned()] {
        match fs::remove_file(&artifact) {
            Ok(()) => removed.push(artifact),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("session-delete-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn deletion_requires_a_closed_session_and_takes_what_is_kept_beside_it() {
        let root = scratch("closed");
        let path = root.join("chat.jsonl");
        let mut session = crate::open(Some(path.clone())).unwrap();
        session.push(elmee_core::message::Message::user("private words")).unwrap();
        fs::write(sidecar(&path, ".torn"), b"half").unwrap();
        fs::write(path.with_extension("tokens"), b"1").unwrap();
        assert!(delete_session(&path).is_err());
        assert!(path.exists());
        drop(session);
        assert_eq!(delete_session(&path).unwrap().len(), 3);
        assert!(fs::read_dir(&root).unwrap().next().is_none(), "nothing is left behind");
        assert!(delete_session(&path).is_err(), "there is nothing left to delete");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_link_to_a_transcript_deletes_nothing() {
        let root = scratch("alias");
        let path = root.join("chat.jsonl");
        let alias = root.join("alias.jsonl");
        fs::write(&path, b"history").unwrap();
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(delete_session(&alias).is_err());
        assert!(delete_session(&root.join("notes.txt")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"history");
        fs::remove_dir_all(root).unwrap();
    }
}
