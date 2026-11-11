//! Where the built-in file tools read and write.
//!
//! Not a boundary. The working directory is what a relative path is resolved
//! against and what a report is shown relative to; an absolute path is used as
//! written. What the agent may touch is decided by the account elmee runs as,
//! which is the only place it was ever decided honestly — a second answer
//! inside the process refused work the operator had allowed, and disagreed
//! with the shell about the same file.
//!
//! What this does own is the part that is about correctness rather than
//! authority: a file changed since it was read is somebody else's work, so an
//! edit built on a stale read is refused rather than applied.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, Read},
    path::{Component, Path, PathBuf},
};

use ring::digest::{Context, SHA256};

use crate::{Error, Result};

pub(super) type Fingerprint = [u8; 32];

pub(super) struct Workspace {
    root: PathBuf,
    /// Hash of the exact bytes returned by the last `read` of each file.
    seen: HashMap<PathBuf, Fingerprint>,
}

impl Workspace {
    pub(super) fn new(root: PathBuf) -> Result<Self> {
        Ok(Self { root, seen: HashMap::new() })
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// The absolute path a tool argument names.
    pub(super) fn resolve(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);
        let joined = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
        Ok(normalize(&joined))
    }

    /// Read selected lines and hash the whole file in one bounded-memory pass.
    pub(super) fn read_lines(
        &mut self,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(PathBuf, String)> {
        const MAX_CAPTURE: usize = 64 * 1024;
        let target = self.resolve(path)?;
        let file = fs::File::open(&target)?;
        let mut reader = std::io::BufReader::new(file);
        let mut digest = Context::new(&SHA256);
        let mut output = Vec::new();
        let mut line = 0_usize;
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            digest.update(available);
            for byte in available {
                if (offset..offset.saturating_add(limit)).contains(&line) && output.len() < MAX_CAPTURE {
                    output.push(*byte);
                }
                if *byte == b'\n' {
                    line += 1;
                }
            }
            let consumed = available.len();
            reader.consume(consumed);
        }
        self.seen.insert(target.clone(), digest.finish().as_ref().try_into().expect("SHA-256 length"));
        let text = String::from_utf8_lossy(&output).trim_end_matches('\n').to_owned();
        // An empty answer to a window past the end of the file reads as "the
        // file is empty", which is a different fact and the wrong one to act
        // on. Saying where the end actually is costs a line and ends the
        // re-read the other answer invites.
        if text.is_empty() && offset > 0 && offset >= line {
            let total = line;
            return Ok((
                target,
                format!("[{path} has {total} lines; the window from line {offset} is past the end]"),
            ));
        }
        Ok((target, text))
    }

    /// Current text plus the identity that must still be current at commit.
    pub(super) fn edit_source(&self, path: &str) -> Result<(PathBuf, PathBuf, String, Fingerprint)> {
        let target = self.resolve(path)?;
        let bytes = fs::read(&target)?;
        let current = fingerprint(&bytes);
        self.refuse_if_changed(&target, Some(current))?;
        let text =
            String::from_utf8(bytes).map_err(|_| Error::Tool(format!("{} is not UTF-8 text", path)))?;
        Ok((target.clone(), target, text, current))
    }

    /// Where a file that does not exist yet would go.
    pub(super) fn creation_target(&self, path: &str) -> Result<PathBuf> {
        let target = self.resolve(path)?;
        if target.symlink_metadata().is_ok() {
            return Err(Error::Tool(format!("{path} already exists; update it instead of adding it")));
        }
        Ok(target)
    }

    /// The identity a file must still have for a deletion to be the one asked
    /// for. Same freshness rule as an edit: what changed since it was read is
    /// somebody else's work.
    pub(super) fn delete_source(&self, path: &str) -> Result<(PathBuf, Fingerprint)> {
        let target = self.resolve(path)?;
        let current = self
            .current_fingerprint(&target)?
            .ok_or_else(|| Error::Tool(format!("{path} does not exist")))?;
        self.refuse_if_changed(&target, Some(current))?;
        Ok((target, current))
    }

    pub(super) fn remove(&mut self, target: &Path, expected: Fingerprint) -> Result<()> {
        self.verify(target, Some(expected))?;
        fs::remove_file(target)?;
        self.seen.remove(target);
        Ok(())
    }

    pub(super) fn commit(
        &mut self,
        target: &Path,
        content: &str,
        expected: Option<Fingerprint>,
    ) -> Result<()> {
        self.verify(target, expected)?;
        // Crash-safe replacement is one mechanism for the whole workspace, and
        // this is now an ordinary path like every other caller's.
        elmee_plugin_atomic::write(
            target,
            content.as_bytes(),
            elmee_plugin_atomic::Options { preserve_mode: true, ..Default::default() },
        )?;
        self.seen.remove(target);
        Ok(())
    }

    pub(super) fn verify(&self, target: &Path, expected: Option<Fingerprint>) -> Result<()> {
        if self.current_fingerprint(target)? != expected {
            return Err(changed(target));
        }
        Ok(())
    }

    fn current_fingerprint(&self, target: &Path) -> Result<Option<Fingerprint>> {
        match fs::File::open(target) {
            Ok(mut file) => {
                let mut digest = Context::new(&SHA256);
                let mut chunk = [0_u8; 64 * 1024];
                loop {
                    let bytes = file.read(&mut chunk)?;
                    if bytes == 0 {
                        break;
                    }
                    digest.update(&chunk[..bytes]);
                }
                Ok(Some(digest.finish().as_ref().try_into().expect("SHA-256 length")))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn refuse_if_changed(&self, target: &Path, current: Option<Fingerprint>) -> Result<()> {
        match self.seen.get(target) {
            Some(seen) if current != Some(*seen) => Err(changed(target)),
            _ => Ok(()),
        }
    }
}

/// Fold away `.` and `..` without touching the filesystem, so a path reads
/// back the way a person wrote it. Symlinks are left to the kernel.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(super) fn fingerprint(bytes: &[u8]) -> Fingerprint {
    let mut digest = Context::new(&SHA256);
    digest.update(bytes);
    digest.finish().as_ref().try_into().expect("SHA-256 length")
}

fn changed(target: &Path) -> Error {
    Error::Tool(format!(
        "{} changed on disk after you read it — another agent, a command, or the editor wrote to it. Read it again and redo this change against the new contents; applying yours now would drop theirs.",
        target.display()
    ))
}
