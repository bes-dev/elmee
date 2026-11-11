//! The mesh's durable behaviour, checked from outside the crate: storage
//! failures are errors rather than an empty mailbox, locks interoperate with
//! the kernel's, envelopes are validated before publication, and a project's
//! records stay private to the user who owns them.

#[test]
fn discovery_errors_are_neither_empty_meshes_nor_live_peers() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(elmee_plugin_mesh::peers_of(root, "").unwrap().is_empty());
    std::fs::write(root.join("agents"), b"not a directory").unwrap();
    assert!(elmee_plugin_mesh::peers_of(root, "").is_err());
    std::fs::remove_file(root.join("agents")).unwrap();
    let path = root.join("agents/worker.json");
    let registration = elmee_plugin_mesh::Registration {
        name: "worker".into(),
        pid: std::process::id(),
        model: "fixture".into(),
        status: "idle".into(),
    };
    elmee_plugin_atomic::write_json(&path, &registration).unwrap();
    std::fs::create_dir_all(root.join("locks/worker")).unwrap();
    assert!(elmee_plugin_mesh::peers_of(root, "").is_err(), "a failed lock inspection is not a live peer");
    assert!(path.exists(), "unknown ownership cannot justify registration cleanup");
    std::fs::write(&path, b"{").unwrap();
    assert!(elmee_plugin_mesh::peers_of(root, "").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{");
}

#[test]
fn envelope_field_combinations_are_checked_before_publication() {
    use elmee_plugin_mesh::{CompletionStatus, Envelope, MessageKind};
    let directory = tempfile::tempdir().unwrap();
    for kind in [MessageKind::Request, MessageKind::Result, MessageKind::Event] {
        for sender in [false, true] {
            for correlation in [false, true] {
                for outcome in [false, true] {
                    let envelope = Envelope {
                        id: "operation".into(),
                        prompt: String::new(),
                        kind,
                        from: sender.then(|| "author".into()),
                        in_reply_to: correlation.then(|| "request".into()),
                        status: outcome.then_some(CompletionStatus::Completed),
                    };
                    let valid = match kind {
                        MessageKind::Result => sender && correlation && outcome,
                        MessageKind::Request | MessageKind::Event => !correlation && !outcome,
                    };
                    assert_eq!(envelope.valid(), valid, "{envelope:?}");
                    let root = directory.path().join(format!("{kind:?}-{sender}-{correlation}-{outcome}"));
                    let published = elmee_plugin_mesh::deliver_checked(&root, "receiver", &envelope);
                    if valid {
                        published.unwrap();
                        let inbox = elmee_plugin_mesh::inbox(&root, "receiver").unwrap();
                        assert_eq!(inbox.len(), 1);
                        assert_eq!(inbox[0].1, envelope);
                        assert_eq!(envelope.expects_reply(), kind == MessageKind::Request && sender);
                    } else {
                        assert_eq!(published.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
                        assert!(!root.exists(), "invalid envelope must not create mailbox state");
                    }
                }
            }
        }
    }
}

#[test]
fn envelope_wire_defaults_do_not_repair_invalid_types_or_duplicate_fields() {
    let legacy: elmee_plugin_mesh::Envelope = serde_json::from_str(
        r#"{"id":"work","prompt":"","from":null,"status":null,"in_reply_to":null,"recipient_incarnation":null,"future_extension":{}}"#,
    ).unwrap();
    assert!(legacy.valid());
    assert_eq!(legacy.kind, elmee_plugin_mesh::MessageKind::Request);
    assert!(!legacy.expects_reply());
    for invalid in [
        r#"{"prompt":"text"}"#,
        r#"{"id":"work"}"#,
        r#"{"id":1,"prompt":"text"}"#,
        r#"{"id":"work","prompt":null}"#,
        r#"{"id":"work","prompt":"text","kind":null}"#,
        r#"{"id":"work","prompt":"text","kind":"future_kind"}"#,
        r#"{"id":"work","prompt":"text","status":"unknown"}"#,
        r#"{"id":"work","id":"other","prompt":"text"}"#,
        r#"{"id":"work","prompt":"text","from":"a","from":"b"}"#,
    ] {
        assert!(serde_json::from_str::<elmee_plugin_mesh::Envelope>(invalid).is_err(), "accepted {invalid}");
    }
}

#[cfg(unix)]
#[test]
fn mesh_records_and_directories_are_private_to_their_owner() {
    use std::os::unix::fs::PermissionsExt;

    // Assert the modes mesh asks for, not the ones this environment's umask allows.
    let previous = rustix::process::umask(rustix::fs::Mode::empty());
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("mesh");
    let request = elmee_plugin_mesh::Envelope {
        id: "private".into(),
        prompt: "secret work".into(),
        ..Default::default()
    };
    elmee_plugin_mesh::deliver_checked(&root, "peer", &request).unwrap();
    rustix::process::umask(previous);

    let mode = |path: &std::path::Path| std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
    let mut checked = 0;
    for entry in walkdir(&root) {
        let expected = if entry.is_dir() { 0o700 } else { 0o600 };
        assert_eq!(mode(&entry), expected, "{}", entry.display());
        checked += 1;
    }
    assert!(checked >= 3, "expected the root, the mailbox and the record");
}

#[cfg(unix)]
fn walkdir(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = vec![path.to_path_buf()];
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            found.extend(walkdir(&entry.unwrap().path()));
        }
    }
    found
}
