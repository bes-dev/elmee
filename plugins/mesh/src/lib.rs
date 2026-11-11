#![forbid(unsafe_code)]

//! Elmee's local peer mesh: identity, presence and durable addressed messages
//! between the agents running in one project.
//!
//! It knows nothing about models, prompts or turns. A runtime decides how an
//! [`Envelope`] becomes work.

mod events;
pub use events::{notify, persist_event, persist_event_from, persist_message};

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use elmee_core::unique;
use serde::{Deserialize, Serialize};

/// Reference local-binding limit on serialized record/asset bytes, not text
/// characters. Readers and atomic writers enforce the same ceiling.
pub const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

fn check_record_size(bytes: u64) -> std::io::Result<()> {
    if bytes > MAX_RECORD_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mesh record exceeds the 64 MiB byte limit",
        ));
    }
    Ok(())
}

/// Read the opened record, not a path checked separately before opening it.
/// NONBLOCK prevents a substituted FIFO from blocking before we can reject it.
/// Ancestor directory trust is a separate constraint; NOFOLLOW protects the
/// record itself, not every component of its path.
pub fn read_record(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    #[cfg(unix)]
    let mut file = {
        use rustix::fs::{Mode, OFlags, open};
        File::from(open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    };
    #[cfg(not(unix))]
    let mut file = {
        if !fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(std::io::Error::other("mesh record is not a regular file"));
        }
        File::open(path)?
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other("mesh record is not a regular file"));
    }
    check_record_size(metadata.len())?;
    let mut bytes = Vec::new();
    // Metadata is an early rejection only: another writer may grow the open
    // inode after this check. Bound actual reads as well.
    (&mut file).take(MAX_RECORD_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    check_record_size(bytes.len() as u64)?;
    Ok(bytes)
}

/// Missing is distinct from malformed or unreadable; all callers use the same
/// bounded, non-symlink record reader before decoding their own schema.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match read_record(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

mod history;
pub use history::{
    Completion, begin_execution, complete, execution_started, prepare_startup, replay_completed,
};

/// What a live mesh participant publishes while it holds its name.
#[derive(Clone, Serialize, Deserialize)]
pub struct Registration {
    pub name: String,
    pub pid: u32,
    pub model: String,
    pub status: String,
}

/// A participant as another peer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerView {
    pub name: String,
    pub model: String,
    pub status: String,
}

/// One durable message waiting in a peer's inbox.
///
/// Requests, results and events share one envelope. A result preserves its
/// author's identity without creating a new automatic reply obligation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Envelope {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CompletionStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    #[default]
    Request,
    Result,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    #[default]
    Completed,
    Failed,
    Cancelled,
}

impl Envelope {
    /// Preserve routing context when the runtime renders a message for a model.
    pub fn agent_input(&self) -> String {
        let metadata = serde_json::json!({
            "id": self.id, "from": self.from, "kind": self.kind,
            "in_reply_to": self.in_reply_to, "status": self.status,
        });
        format!("mesh {metadata}\n{}", self.prompt)
    }

    pub fn expects_reply(&self) -> bool {
        self.kind == MessageKind::Request && self.from.is_some()
    }

    pub fn valid(&self) -> bool {
        valid_message_id(&self.id)
            && self.from.as_deref().is_none_or(valid_name)
            && self.in_reply_to.as_deref().is_none_or(valid_message_id)
            && match self.kind {
                MessageKind::Result => {
                    self.from.is_some() && self.in_reply_to.is_some() && self.status.is_some()
                }
                _ => self.in_reply_to.is_none() && self.status.is_none(),
            }
    }
}

mod names;
pub use names::*;

fn serialize_record(value: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    serialize_bounded(value, MAX_RECORD_BYTES)
}

/// Serialize JSON while stopping before the output buffer exceeds `limit` bytes.
/// The limit covers encoded bytes, not the caller's input object or allocations
/// performed inside a custom Serialize implementation.
pub fn serialize_bounded(value: &impl Serialize, limit: usize) -> serde_json::Result<Vec<u8>> {
    struct Buffer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "serialized mesh record exceeds the byte limit",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer { bytes: Vec::new(), limit };
    serde_json::to_writer(&mut buffer, value)?;
    Ok(buffer.bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    check_record_size(bytes.len() as u64)?;
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_dir_private(parent)?;
    elmee_plugin_atomic::write(
        path,
        bytes,
        elmee_plugin_atomic::Options { mode: Some(0o600), ..Default::default() },
    )
}

mod inbox;
pub use inbox::*;

#[cfg(test)]
mod tests;
