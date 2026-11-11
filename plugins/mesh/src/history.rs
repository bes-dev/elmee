use crate::{CompletionStatus, Envelope, MessageKind, read_record};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// A completed operation is persisted before publishing its result. The same
/// receipt is reused after a crash; its result ID and body never change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    pub request: Envelope,
    pub answer: String,
    pub status: CompletionStatus,
    pub response: Option<Envelope>,
}

impl Completion {
    /// History is a protocol record, not trusted internal state. A saved reply
    /// must be exactly the result implied by the receipt, never a fresh request.
    fn validate(&self, peer: &str, id: &str) -> io::Result<()> {
        let response_valid = match (&self.response, self.request.expects_reply()) {
            (None, false) => true,
            (Some(response), true) => {
                response.valid()
                    && response.kind == MessageKind::Result
                    && response.from.as_deref() == Some(peer)
                    && response.in_reply_to.as_deref() == Some(id)
                    && response.status == Some(self.status)
                    && response.prompt == self.answer
            }
            _ => false,
        };
        if !self.request.valid() || self.request.id != id || !response_valid {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "inconsistent mesh completion record"));
        }
        Ok(())
    }
}

fn execution_path(root: &Path, peer: &str, id: &str) -> io::Result<PathBuf> {
    location(root, peer, id)?;
    Ok(root.join("executions").join(peer).join(format!("{id}.json")))
}

pub fn execution_started(root: &Path, peer: &str, request: &Envelope) -> io::Result<bool> {
    let path = execution_path(root, peer, &request.id)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid execution record"));
    }
    let stored: Envelope = serde_json::from_slice(&read_record(&path)?)?;
    if stored != *request {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "execution ID reused with different content"));
    }
    Ok(true)
}

/// Caller holds the peer claim. Persist before handing the envelope to the
/// harness. A crash after this boundary is ambiguous, not safe to re-execute.
/// False means the source was removed/deleted before dispatch.
pub fn begin_execution(root: &Path, peer: &str, request: &Envelope) -> io::Result<bool> {
    if !request.valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid execution envelope"));
    }
    let _publication = crate::delivery_lock(root, peer)?;
    let execution = execution_path(root, peer, &request.id)?;
    let source = root.join("inbox").join(peer).join(format!("{}.json", request.id));
    let bytes = match read_record(&source) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if serde_json::from_slice::<Envelope>(&bytes)? != *request {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "execution source changed"));
    }
    if execution_started(root, peer, request)? {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "request was already dispatched"));
    }
    if read(root, peer, request)?.is_some() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "request was already completed"));
    }
    crate::write_atomic(&execution, &crate::serialize_record(request)?)?;
    Ok(true)
}

fn forget_execution(root: &Path, peer: &str, request: &Envelope) -> io::Result<()> {
    match fs::remove_file(execution_path(root, peer, &request.id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn location(root: &Path, peer: &str, id: &str) -> io::Result<PathBuf> {
    if !crate::valid_name(peer) || !crate::valid_message_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid history address"));
    }
    Ok(root.join("history").join(peer).join(format!("{id}.json")))
}

fn read(root: &Path, peer: &str, request: &Envelope) -> io::Result<Option<Completion>> {
    let Some(completed): Option<Completion> = crate::read_json(&location(root, peer, &request.id)?)? else {
        return Ok(None);
    };
    completed.validate(peer, &request.id)?;
    if completed.request != *request {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message ID reused with different content"));
    }
    // A prior atomic write may have renamed the receipt but failed its final
    // directory flush. Finish that step before it can justify forgetting the
    // execution journal or publishing a result. Do not rewrite the receipt.
    fs::File::open(root.join("history").join(peer))?.sync_all()?;
    Ok(Some(completed))
}

pub(crate) fn validate_redelivery(root: &Path, peer: &str, request: &Envelope) -> io::Result<()> {
    read(root, peer, request)?;
    execution_started(root, peer, request)?;
    Ok(())
}

fn publish(root: &Path, completed: &Completion) -> io::Result<()> {
    if let Some(response) = &completed.response {
        let sender =
            completed.request.from.as_deref().ok_or_else(|| io::Error::other("result has no recipient"))?;
        crate::deliver_checked(root, sender, response).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not publish result to @{sender}; saved result retained: {error}"),
            )
        })?;
    }
    Ok(())
}

/// Returns true when completed work was recovered without executing it again.
pub fn replay_completed(root: &Path, peer: &str, request: &Envelope) -> io::Result<bool> {
    let Some(completed) = read(root, peer, request)? else { return Ok(false) };
    forget_execution(root, peer, request)?;
    publish(root, &completed)?;
    Ok(true)
}

/// Caller must hold the recipient's exclusive peer claim.
pub fn complete(
    root: &Path,
    peer: &str,
    request: &Envelope,
    answer: String,
    status: CompletionStatus,
) -> io::Result<()> {
    if !crate::valid_name(peer) || !request.valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid request"));
    }
    let publication = crate::delivery_lock(root, peer)?;
    if let Some(completed) = read(root, peer, request)? {
        forget_execution(root, peer, request)?;
        drop(publication);
        return publish(root, &completed);
    }
    let response = request.expects_reply().then(|| Envelope {
        id: crate::unique("result"),
        prompt: answer.clone(),
        from: Some(peer.to_owned()),
        kind: MessageKind::Result,
        in_reply_to: Some(request.id.clone()),
        status: Some(status),
    });
    let completed = Completion { request: request.clone(), answer, status, response };
    crate::write_atomic(&location(root, peer, &request.id)?, &crate::serialize_record(&completed)?)?;
    forget_execution(root, peer, request)?;
    drop(publication);
    publish(root, &completed)
}

/// Called while holding the peer claim, before publishing presence. Fresh
/// sessions retain stale requests in history as cancelled instead of executing
/// them. Saved completed work is always recovered, never cancelled or rerun.
pub fn prepare_startup(root: &Path, peer: &str) -> io::Result<()> {
    for (path, request) in crate::inspect_inbox(root, peer)?.messages {
        if !replay_completed(root, peer, &request)? {
            if execution_started(root, peer, &request)? {
                complete(root, peer, &request,
                    "Interrupted after dispatch to the harness. Execution and external side effects are unknown; this request will not be executed again automatically. Inspect the harness transcript and effects before submitting a new request.".into(),
                    CompletionStatus::Failed)?;
            } else {
                complete(
                    root,
                    peer,
                    &request,
                    "Not executed: this message predates the agent session that found it. Send it again."
                        .into(),
                    CompletionStatus::Cancelled,
                )?;
            }
        }
        forget_execution(root, peer, &request)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_messages(root: &Path, peer: &str) -> io::Result<Vec<Completion>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(root.join("history").join(peer))? {
            let entry = entry?;
            let filename = entry.file_name();
            let id = filename.to_str().and_then(|name| name.strip_suffix(".json")).unwrap();
            let completion: Completion = serde_json::from_slice(&read_record(&entry.path())?)?;
            completion.validate(peer, id)?;
            records.push(completion);
        }
        records.sort_by(|a, b| a.request.id.cmp(&b.request.id));
        Ok(records)
    }

    #[cfg(unix)]
    #[test]
    fn failed_journal_flush_never_becomes_retry_admission() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let _owner = crate::claim_exact_name(root, "worker").unwrap().unwrap();
        let request = Envelope { id: "work".into(), prompt: "once".into(), ..Default::default() };
        crate::deliver_checked(root, "worker", &request).unwrap();
        let executions = root.join("executions/worker");
        fs::create_dir_all(&executions).unwrap();
        fs::set_permissions(&executions, fs::Permissions::from_mode(0o300)).unwrap();
        if fs::File::open(&executions).is_ok() {
            fs::set_permissions(&executions, fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("directory permission fault unavailable for this privileged test user");
            return;
        }
        let first = begin_execution(root, "worker", &request);
        let journal = fs::read(executions.join("work.json"));
        let retry = begin_execution(root, "worker", &request);
        fs::set_permissions(&executions, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(first.is_err(), "failed durability cannot authorize native execution");
        assert!(journal.is_ok(), "exercise successful rename followed by failed flush");
        assert_eq!(retry.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        prepare_startup(root, "worker").unwrap();
        let completed = completed_messages(root, "worker").unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].status, CompletionStatus::Failed);
        assert!(!begin_execution(root, "worker", &request).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn completion_retry_requires_directory_durability_before_forgetting_execution() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let _owner = crate::claim_exact_name(root, "worker").unwrap().unwrap();
        let request = Envelope { id: "work".into(), prompt: "once".into(), ..Default::default() };
        crate::deliver_checked(root, "worker", &request).unwrap();
        begin_execution(root, "worker", &request).unwrap();
        let history = root.join("history/worker");
        fs::create_dir_all(&history).unwrap();
        // Search/write are allowed, so rename succeeds; opening the directory
        // for its durability flush fails. This exercises the real filesystem.
        fs::set_permissions(&history, fs::Permissions::from_mode(0o300)).unwrap();
        if fs::File::open(&history).is_ok() {
            fs::set_permissions(&history, fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("directory permission fault unavailable for this privileged test user");
            return;
        }
        let first = complete(root, "worker", &request, "first answer".into(), CompletionStatus::Completed);
        let saved = fs::read(history.join("work.json"));
        let retry = complete(root, "worker", &request, "late answer".into(), CompletionStatus::Failed);
        let replay = replay_completed(root, "worker", &request);
        let journal = execution_started(root, "worker", &request);
        fs::set_permissions(&history, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(first.is_err());
        assert!(saved.is_ok(), "rename must have succeeded before the failed flush");
        assert!(retry.is_err(), "an unflushed completion must not authorize journal removal");
        assert!(replay.is_err());
        assert!(journal.unwrap());
        assert!(replay_completed(root, "worker", &request).unwrap());
        assert!(!execution_started(root, "worker", &request).unwrap());
        assert_eq!(fs::read(history.join("work.json")).unwrap(), saved.unwrap());
    }

    #[test]
    fn inconsistent_history_cannot_publish_or_erase_execution_evidence() {
        let root = std::env::temp_dir().join(crate::unique("mesh-history-validation"));
        let request = Envelope {
            id: "work".into(),
            prompt: "question".into(),
            from: Some("sender".into()),
            ..Default::default()
        };
        crate::deliver_checked(&root, "worker", &request).unwrap();
        assert!(begin_execution(&root, "worker", &request).unwrap());
        let valid = Completion {
            request: request.clone(),
            answer: "answer".into(),
            status: CompletionStatus::Completed,
            response: Some(Envelope {
                id: "reply".into(),
                prompt: "answer".into(),
                from: Some("worker".into()),
                kind: MessageKind::Result,
                in_reply_to: Some("work".into()),
                status: Some(CompletionStatus::Completed),
            }),
        };
        for fault in ["missing", "request", "sender", "correlation", "status", "body", "id", "extra"] {
            let mut broken = valid.clone();
            match fault {
                "missing" => broken.response = None,
                "extra" => broken.request.kind = MessageKind::Event,
                _ => {
                    let response = broken.response.as_mut().unwrap();
                    match fault {
                        "request" => {
                            response.kind = MessageKind::Request;
                            response.in_reply_to = None;
                            response.status = None;
                        }
                        "sender" => response.from = Some("impostor".into()),
                        "correlation" => response.in_reply_to = Some("other".into()),
                        "status" => response.status = Some(CompletionStatus::Failed),
                        "body" => response.prompt = "different answer".into(),
                        "id" => response.id = "../escape".into(),
                        _ => unreachable!(),
                    }
                }
            }
            let path = location(&root, "worker", "work").unwrap();
            let bytes = serde_json::to_vec(&broken).unwrap();
            crate::write_atomic(&path, &bytes).unwrap();
            assert!(replay_completed(&root, "worker", &request).is_err(), "{fault}");
            assert!(validate_redelivery(&root, "worker", &request).is_err(), "{fault}");
            assert!(
                complete(&root, "worker", &request, "replacement".into(), CompletionStatus::Failed).is_err(),
                "{fault}"
            );
            assert!(completed_messages(&root, "worker").is_err(), "{fault}");
            assert!(execution_started(&root, "worker", &request).unwrap(), "{fault}");
            assert!(crate::inbox(&root, "sender").unwrap().is_empty(), "{fault}");
            assert_eq!(fs::read(path).unwrap(), bytes, "{fault}");
        }
        crate::write_atomic(
            &location(&root, "worker", "work").unwrap(),
            &serde_json::to_vec(&valid).unwrap(),
        )
        .unwrap();
        assert!(replay_completed(&root, "worker", &request).unwrap());
        assert_eq!(crate::inbox(&root, "sender").unwrap()[0].1, valid.response.unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_dispatch_is_never_reexecuted() {
        let root = std::env::temp_dir().join(crate::unique("mesh-interrupted"));
        let request = Envelope {
            id: "started".into(),
            prompt: "side effect".into(),
            from: Some("sender".into()),
            ..Default::default()
        };
        assert!(crate::deliver(&root, "worker", &request));
        assert!(begin_execution(&root, "worker", &request).unwrap());
        assert!(execution_started(&root, "worker", &request).unwrap());
        assert_eq!(
            begin_execution(&root, "worker", &request).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        // Simulate loss of process memory, not an intentional cancellation.
        prepare_startup(&root, "worker").unwrap();
        assert!(crate::inbox(&root, "worker").unwrap().is_empty());
        assert!(!execution_started(&root, "worker", &request).unwrap());
        let completion = &completed_messages(&root, "worker").unwrap()[0];
        assert_eq!(completion.status, CompletionStatus::Failed);
        assert!(completion.answer.contains("external side effects are unknown"));
        let result = crate::inbox(&root, "sender").unwrap()[0].1.clone();
        assert!(crate::deliver(&root, "worker", &request));
        prepare_startup(&root, "worker").unwrap();
        assert_eq!(crate::inbox(&root, "sender").unwrap()[0].1, result);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_completed_receipt_wins_over_a_leftover_execution_marker() {
        let root = std::env::temp_dir().join(crate::unique("mesh-complete-marker"));
        let request = Envelope {
            id: "done".into(),
            prompt: "work".into(),
            from: Some("worker".into()),
            ..Default::default()
        };
        assert!(crate::deliver(&root, "worker", &request));
        assert!(begin_execution(&root, "worker", &request).unwrap());
        complete(&root, "worker", &request, "original".into(), CompletionStatus::Completed).unwrap();
        assert!(!execution_started(&root, "worker", &request).unwrap());
        // Crash after saving the receipt but before removing its marker.
        crate::write_atomic(
            &execution_path(&root, "worker", &request.id).unwrap(),
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        prepare_startup(&root, "worker").unwrap();
        assert_eq!(
            completed_messages(&root, "worker")
                .unwrap()
                .iter()
                .find(|entry| entry.request.id == "done")
                .unwrap()
                .status,
            CompletionStatus::Completed
        );
        assert!(!execution_started(&root, "worker", &request).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_delivery_recovers_the_same_result_without_reexecuting_work() {
        let root = std::env::temp_dir().join(crate::unique("mesh-history-test"));
        fs::create_dir_all(root.join("inbox")).unwrap();
        fs::write(root.join("inbox/sender"), "blocked").unwrap();
        let request = Envelope {
            id: "request".into(),
            prompt: "work".into(),
            from: Some("sender".into()),
            ..Default::default()
        };
        assert!(complete(&root, "worker", &request, "original".into(), CompletionStatus::Completed).is_err());
        let saved = read(&root, "worker", &request).unwrap().unwrap();
        fs::remove_file(root.join("inbox/sender")).unwrap();
        assert!(replay_completed(&root, "worker", &request).unwrap());
        for late_status in
            [CompletionStatus::Completed, CompletionStatus::Failed, CompletionStatus::Cancelled]
        {
            complete(&root, "worker", &request, "must not replace".into(), late_status).unwrap();
            assert_eq!(read(&root, "worker", &request).unwrap().unwrap(), saved);
            assert_eq!(crate::inbox(&root, "sender").unwrap().len(), 1);
        }
        let inbox = crate::inbox(&root, "sender").unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(Some(&inbox[0].1), saved.response.as_ref());
        assert!(!inbox[0].1.expects_reply());
        let changed = Envelope { prompt: "different work".into(), ..request };
        assert!(replay_completed(&root, "worker", &changed).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
