//! Addressed delivery: publishing an envelope to a peer and reading an inbox.

use super::*;

/// Durably enqueue an envelope for one peer.
pub fn deliver(base: &Path, recipient: &str, envelope: &Envelope) -> bool {
    deliver_checked(base, recipient, envelope).is_ok()
}

/// Serialize local publication with name acquisition, independently of the
/// peer's lifetime claim. Never hold this lock while invoking an agent or
/// publishing to another mailbox. Lock files must not be removed while the
/// root is in use.
pub(super) fn delivery_lock(base: &Path, recipient: &str) -> std::io::Result<File> {
    if !valid_name(recipient) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid peer name"));
    }
    let directory = base.join("delivery-locks");
    create_dir_private(&directory)?;
    let peer = open_lock_record(&directory.join(recipient))?;
    peer.lock()?;
    Ok(peer)
}

/// Like [`deliver`], but preserves the reason for rejection. Published message
/// IDs are immutable: retries must carry the same envelope, even after completion.
pub fn deliver_checked(base: &Path, recipient: &str, envelope: &Envelope) -> std::io::Result<()> {
    if !valid_name(recipient) || !envelope.valid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid message address or envelope",
        ));
    }
    let _publication = delivery_lock(base, recipient)?;
    publish_locked(base, recipient, envelope)
}

/// Publish only while the captured recipient lifetime still owns the address.
/// Unlike ordinary offline delivery, an absent owner is an error. Capture the
/// incarnation during attachment; do not refresh it after a failed send.
/// This fences the recipient only, not the envelope's sender identity.
pub fn deliver_to_incarnation(
    base: &Path,
    recipient: &str,
    incarnation: &str,
    envelope: &Envelope,
) -> std::io::Result<()> {
    if !valid_name(recipient) || !valid_message_id(incarnation) || !envelope.valid() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid fenced mesh delivery"));
    }
    // Claim acquisition/rotation and publication hold the same peer gate.
    // Keep it across both the lifetime check and immutable publication.
    let _publication = delivery_lock(base, recipient)?;
    let path = base.join("locks").join(recipient);
    if probe_claim(&path)?.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "mesh recipient has no active owner",
        ));
    }
    let current: String = serde_json::from_slice(&read_record(&path)?)?;
    if current != incarnation {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "mesh recipient incarnation changed; stale attachment rejected",
        ));
    }
    publish_locked(base, recipient, envelope)
}

fn publish_locked(base: &Path, recipient: &str, envelope: &Envelope) -> std::io::Result<()> {
    // Reject existing directory aliases before inspecting or publishing inbox
    // content. This is structural validation, not full ancestor confinement:
    // concurrent same-user directory replacement still requires openat-based IO.
    let mut directory = base.to_path_buf();
    for component in ["inbox", recipient] {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(std::io::Error::other("mesh inbox component is not a real directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    history::validate_redelivery(base, recipient, envelope)?;
    let path = base.join("inbox").join(recipient).join(format!("{}.json", envelope.id));
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "message path is not a regular file",
                ));
            }
            match read_record(&path) {
                Ok(bytes) => {
                    let existing: Envelope = serde_json::from_slice(&bytes)?;
                    if existing != *envelope {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "message ID reused with different content",
                        ));
                    }
                    // The previous attempt may have published successfully
                    // but failed its directory flush. A successful retry must
                    // finish that durability step without rewriting the file.
                    File::open(path.parent().expect("inbox file has a parent"))?.sync_all()?;
                    return Ok(());
                }
                // The owner may acknowledge concurrently. Republishing an
                // identical retry is safe: the saved completion handles replay.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Completion can appear while the owner removes the previously observed
    // inbox file. Recheck after observing absence, before publishing a retry.
    history::validate_redelivery(base, recipient, envelope)?;
    write_atomic(&path, &serialize_record(envelope)?)
}

/// Ordered inbox records. Reading does not acknowledge them; the consumer
/// removes a record only after it has durably accepted or handled it.
/// Storage errors remain errors, never an empty mailbox. Use `inspect_inbox`
/// when individual rejected records and their diagnostics are also needed.
pub fn inbox(base: &Path, recipient: &str) -> std::io::Result<Vec<(PathBuf, Envelope)>> {
    inspect_inbox(base, recipient).map(|inspection| inspection.messages)
}

#[derive(Debug, Serialize)]
pub struct InboxProblem {
    /// Display-only filename. Use the validated id, not this possibly lossy
    /// label, when offering a clear-by-ID operation.
    pub file: String,
    pub id: Option<String>,
    pub error: String,
}

#[derive(Default)]
pub struct InboxInspection {
    pub messages: Vec<(PathBuf, Envelope)>,
    pub problems: Vec<InboxProblem>,
}

/// Read-only diagnostic view using exactly the runtime's admission rules.
/// Unpublished temporary files are ignored; rejected .json records are visible.
pub fn inspect_inbox(base: &Path, recipient: &str) -> std::io::Result<InboxInspection> {
    if !valid_name(recipient) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid inbox recipient"));
    }
    let entries = match fs::read_dir(base.join("inbox").join(recipient)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(InboxInspection::default()),
        Err(error) => return Err(error),
    };
    let mut paths: Vec<_> =
        entries.map(|entry| entry.map(|entry| entry.path())).collect::<std::io::Result<_>>()?;
    paths.sort();
    let mut inspection = InboxInspection::default();
    for path in paths {
        if path.file_name().is_none_or(|name| !name.as_encoded_bytes().ends_with(b".json")) {
            continue;
        }
        let id = path.file_stem().and_then(|stem| stem.to_str()).filter(|id| valid_message_id(id));
        let read = || -> std::io::Result<Envelope> {
            let id = id.ok_or_else(|| std::io::Error::other("invalid record filename"))?;
            let envelope: Envelope = serde_json::from_slice(&read_record(&path)?)?;
            if envelope.id != id || !envelope.valid() {
                return Err(std::io::Error::other("invalid envelope or filename/id mismatch"));
            }
            Ok(envelope)
        };
        match read() {
            Ok(envelope) => inspection.messages.push((path, envelope)),
            // Concurrent acknowledgement is not a corrupt record.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => inspection.problems.push(InboxProblem {
                file: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                id: id.map(str::to_owned),
                error: error.to_string(),
            }),
        }
    }
    Ok(inspection)
}
