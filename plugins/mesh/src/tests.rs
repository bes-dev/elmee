#[test]
fn bounded_serialization_counts_escaping_and_stops_the_producer() {
    assert_eq!(super::serialize_bounded(&"\n\n\n", 8).unwrap(), br#""\n\n\n""#);
    assert!(super::serialize_bounded(&"\n\n\n\n", 8).is_err());
    assert_eq!(super::serialize_bounded(&"é", 4).unwrap(), "\"é\"".as_bytes());
    assert!(super::serialize_bounded(&"é", 3).is_err());
    struct Expanding<'a>(&'a std::cell::Cell<usize>);
    impl serde::Serialize for Expanding<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(None)?;
            for _ in 0..1000 {
                self.0.set(self.0.get() + 1);
                sequence.serialize_element("x")?;
            }
            sequence.end()
        }
    }
    let produced = std::cell::Cell::new(0);
    assert!(super::serialize_bounded(&Expanding(&produced), 8).is_err());
    assert!(produced.get() < 10, "reject while serializing, not after allocating the entire JSON");
}

#[test]
fn oversized_records_are_rejected_without_replacing_or_deleting_files() {
    let root = std::env::temp_dir().join(super::unique("mesh-record-limit"));
    std::fs::create_dir_all(&root).unwrap();
    let large = root.join("large.json");
    std::fs::File::create(&large).unwrap().set_len(super::MAX_RECORD_BYTES as u64 + 1).unwrap();
    let error = super::read_record(&large).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(std::fs::metadata(&large).unwrap().len(), super::MAX_RECORD_BYTES as u64 + 1);
    assert!(super::check_record_size(super::MAX_RECORD_BYTES as u64).is_ok());
    let target = root.join("existing.json");
    super::write_atomic(&target, b"true").unwrap();
    assert!(super::write_atomic(&target, &vec![b'x'; super::MAX_RECORD_BYTES + 1]).is_err());
    assert_eq!(super::read_record(&target).unwrap(), b"true");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2, "no temporary file for rejected publication");
    std::fs::remove_dir_all(root).unwrap();
}

use super::*;

#[cfg(unix)]
#[test]
fn protocol_locks_reject_symlinks_and_fifos_without_claiming_an_identity() {
    for directory in ["locks", "delivery-locks"] {
        for kind in ["symlink", "fifo"] {
            let root = std::env::temp_dir().join(unique("mesh-lock-types"));
            fs::create_dir_all(root.join(directory)).unwrap();
            let path = root.join(directory).join("worker");
            let target = root.join("external");
            fs::write(&target, b"do not touch").unwrap();
            if kind == "symlink" {
                std::os::unix::fs::symlink(&target, &path).unwrap();
            } else {
                assert!(std::process::Command::new("mkfifo").arg(&path).status().unwrap().success());
            }
            assert!(claim_exact_name(&root, "worker").is_err(), "{directory}/{kind}");
            if directory == "delivery-locks" {
                let request = Envelope { id: "work".into(), prompt: "test".into(), ..Default::default() };
                assert!(deliver_checked(&root, "worker", &request).is_err());
                assert!(!root.join("inbox/worker/work.json").exists());
            }
            assert_eq!(fs::read(target).unwrap(), b"do not touch");
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[test]
fn exact_claims_distinguish_conflicts_from_errors_and_wait_for_inspection() {
    let root = std::env::temp_dir().join(unique("mesh-exact-claim"));
    assert!(claim_exact_name(&root, "../bad").is_err());
    assert!(!root.exists());
    let gate = delivery_lock(&root, "worker").unwrap();
    let inspection = hold(&root.join("locks/worker")).unwrap().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let base = root.clone();
    let thread = std::thread::spawn(move || send.send(claim_exact_name(&base, "worker")).unwrap());
    assert!(receive.recv_timeout(std::time::Duration::from_millis(25)).is_err());
    drop(inspection);
    drop(gate);
    let held = receive.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap().unwrap();
    thread.join().unwrap();
    assert!(claim_exact_name(&root, "worker").unwrap().is_none());
    drop(held);
    assert!(claim_exact_name(&root, "worker").unwrap().is_some());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn protocol_record_reads_refuse_symlinks_directories_and_fifos() {
    let root = std::env::temp_dir().join(unique("mesh-record-types"));
    fs::create_dir_all(&root).unwrap();
    let target = root.join("target");
    fs::write(&target, b"private bytes").unwrap();
    assert_eq!(read_record(&target).unwrap(), b"private bytes");
    let link = root.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(read_record(&link).is_err());
    assert!(read_record(&root).is_err());
    let fifo = root.join("fifo");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        send.send(read_record(&fifo)).unwrap();
    });
    assert!(
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("opening a protocol FIFO must not block")
            .is_err()
    );
    reader.join().unwrap();
    assert_eq!(fs::read(target).unwrap(), b"private bytes");
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_presence_and_dispatch_sources_are_not_protocol_records() {
    let root = std::env::temp_dir().join(unique("mesh-record-links"));
    let (_, claim) = claim_name(&root, "worker");
    let _claim = claim.unwrap();
    fs::create_dir_all(root.join("agents")).unwrap();
    let target = root.join("external-registration");
    assert!(write_json_atomic(
        &target,
        &Registration {
            name: "worker".into(),
            pid: std::process::id(),
            model: "fixture".into(),
            status: "idle".into(),
        }
    ));
    std::os::unix::fs::symlink(&target, root.join("agents/worker.json")).unwrap();
    assert!(peers_of(&root, "sender").is_err());
    let request = Envelope { id: "work".into(), prompt: "private".into(), ..Default::default() };
    let source = root.join("external-request");
    assert!(write_json_atomic(&source, &request));
    fs::create_dir_all(root.join("inbox/worker")).unwrap();
    std::os::unix::fs::symlink(&source, root.join("inbox/worker/work.json")).unwrap();
    assert!(begin_execution(&root, "worker", &request).is_err());
    assert!(!root.join("executions/worker/work.json").exists());
    assert_eq!(serde_json::from_slice::<Envelope>(&fs::read(source).unwrap()).unwrap(), request);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_stale_presence_inspection_cannot_rename_a_concurrent_launcher() {
    let root = scratch("claim-inspection-gate");
    let gate = delivery_lock(&root, "worker").unwrap();
    let inspection = hold(&root.join("locks/worker")).unwrap().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            ready_tx.send(()).unwrap();
            let (name, claim) = claim_name(&root, "worker");
            assert!(claim.is_some());
            result_tx.send(name).unwrap();
        });
        ready_rx.recv().unwrap();
        let early = result_rx.recv_timeout(std::time::Duration::from_millis(100));
        drop(inspection);
        drop(gate);
        assert!(matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(result_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), "worker");
    });
    fs::remove_dir_all(root).unwrap();
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(unique(&format!("mesh-{name}")));
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn only_published_matching_records_are_visible() {
    let root = scratch("publication");
    let dir = root.join("inbox/worker");
    fs::create_dir_all(&dir).unwrap();
    let envelope = Envelope {
        id: "request".into(),
        prompt: "work".into(),
        from: Some("sender".into()),
        ..Default::default()
    };
    let temporary = dir.join(".write.tmp");
    fs::write(&temporary, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(inbox(&root, "worker").unwrap().is_empty());
    assert!(temporary.exists());
    fs::rename(&temporary, dir.join("request.json")).unwrap();
    assert_eq!(inbox(&root, "worker").unwrap().len(), 1);
    fs::write(dir.join("mismatch.json"), serde_json::to_vec(&envelope).unwrap()).unwrap();
    fs::write(dir.join("invalid.json"), br#"{"id":"../../outside","prompt":"work"}"#).unwrap();
    fs::write(dir.join("sender.json"), br#"{"id":"sender","prompt":"work","from":"../outside"}"#).unwrap();
    fs::write(dir.join("partial.json"), b"{").unwrap();
    assert_eq!(inbox(&root, "worker").unwrap().len(), 1);
    assert!(dir.join("partial.json").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn claimed_names_remain_deliverable_after_collisions() {
    let root = scratch("long-names");
    let base = "x".repeat(40);
    let held: Vec<_> = (0..101).map(|_| claim_name(&root, &base)).collect();
    let envelope = Envelope { id: "request".into(), prompt: "work".into(), from: None, ..Default::default() };
    for (name, lock) in &held {
        assert!(valid_name(name), "invalid claimed name: {name}");
        assert!(lock.is_some());
        assert!(deliver(&root, name, &envelope));
    }
    let (name, lock) = claim_name(&root, "agent");
    assert_eq!(name, "agent");
    assert!(lock.is_some());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn independent_peers_discover_and_exchange_a_durable_message() {
    let root = scratch("exchange");
    let (alice, alice_lock) = claim_name(&root, "alice");
    let (bob, bob_lock) = claim_name(&root, "bob");
    assert!(alice_lock.is_some() && bob_lock.is_some());
    for name in [&alice, &bob] {
        write_json_atomic(
            &root.join("agents").join(format!("{name}.json")),
            &Registration {
                name: name.clone(),
                pid: std::process::id(),
                model: "test".into(),
                status: "idle".into(),
            },
        );
    }
    assert_eq!(peers_of(&root, &alice).unwrap()[0].name, bob);
    let message =
        Envelope { id: unique("msg"), prompt: "hello".into(), from: Some(alice), ..Default::default() };
    assert!(deliver(&root, &bob, &message));
    let stored: Envelope = serde_json::from_slice(
        &fs::read(root.join("inbox").join(&bob).join(format!("{}.json", message.id))).unwrap(),
    )
    .unwrap();
    assert_eq!(stored, message);
    drop((alice_lock, bob_lock));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn names_and_ids_are_safe_and_unique() {
    assert_eq!(clean_name("review agent!"), "reviewagent");
    assert_eq!(clean_name("../escape"), "escape");
    assert_ne!(unique("agent"), unique("agent"));
    assert!(valid_name("reviewer-2"));
    assert!(!valid_name("../reviewer"));
}

#[test]
fn protocol_paths_cannot_be_supplied_by_an_envelope() {
    let root = scratch("paths");
    let message = Envelope { id: "msg".into(), prompt: "hello".into(), from: None, ..Default::default() };
    assert!(!deliver(&root, "../outside", &message));
    let bad_id = Envelope { id: "../outside".into(), ..message.clone() };
    assert!(!deliver(&root, "peer", &bad_id));
    let bad_sender = Envelope { from: Some("../outside".into()), ..message };
    assert!(!deliver(&root, "peer", &bad_sender));
    assert_eq!(inbox(&root, "../outside").unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn published_ids_are_immutable_before_and_after_completion() {
    let root = scratch("immutable");
    let request = Envelope { id: "same".into(), prompt: "original".into(), ..Default::default() };
    deliver_checked(&root, "peer", &request).unwrap();
    let changed = Envelope { prompt: "replacement".into(), ..request.clone() };
    assert_eq!(deliver_checked(&root, "peer", &changed).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    deliver_checked(&root, "peer", &request).unwrap();
    assert_eq!(inbox(&root, "peer").unwrap()[0].1, request);
    complete(&root, "peer", &request, "done".into(), CompletionStatus::Completed).unwrap();
    fs::remove_file(&inbox(&root, "peer").unwrap()[0].0).unwrap();
    assert_eq!(deliver_checked(&root, "peer", &changed).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    assert!(inbox(&root, "peer").unwrap().is_empty());
    deliver_checked(&root, "peer", &request).unwrap();
    assert!(replay_completed(&root, "peer", &request).unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_conflicting_publishers_have_one_winner() {
    let root = scratch("publish-race");
    let barrier = std::sync::Barrier::new(16);
    let winners = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|index| {
                let root = &root;
                let barrier = &barrier;
                scope.spawn(move || {
                    let request =
                        Envelope { id: "contended".into(), prompt: index.to_string(), ..Default::default() };
                    barrier.wait();
                    deliver_checked(root, "peer", &request).ok().map(|()| request)
                })
            })
            .collect();
        handles.into_iter().filter_map(|handle| handle.join().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(winners.len(), 1);
    assert_eq!(inbox(&root, "peer").unwrap()[0].1, winners[0]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn delivery_does_not_replace_corrupt_records() {
    let root = scratch("corrupt-publish");
    let path = root.join("inbox/peer/broken.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"incomplete JSON").unwrap();
    let request = Envelope { id: "broken".into(), prompt: "new".into(), ..Default::default() };
    assert!(deliver_checked(&root, "peer", &request).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"incomplete JSON");
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn delivery_does_not_follow_or_replace_a_message_symlink() {
    let root = scratch("symlink-publish");
    let path = root.join("inbox/peer/link.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let target = root.join("target");
    fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let request = Envelope { id: "link".into(), prompt: "new".into(), ..Default::default() };
    assert!(deliver_checked(&root, "peer", &request).is_err());
    assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn temporary_claim_does_not_survive_in_a_duplicated_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("probe");
    let probe = probe_claim(&path).unwrap().unwrap();
    let inherited = probe.0.try_clone().unwrap();
    assert!(hold(&path).unwrap().is_none(), "the probe must retain its lock until scope exit");
    drop(probe);
    assert!(hold(&path).unwrap().is_some(), "a completed probe must not look like a live owner");
    drop(inherited);
}

#[test]
fn a_held_name_is_not_reissued_and_is_freed_on_drop() {
    let root = scratch("claims");
    let (first, held) = claim_name(&root, "worker");
    let (second, second_held) = claim_name(&root, "worker");
    assert_eq!(first, "worker");
    assert_eq!(second, "worker-2");
    assert!(second_held.is_some());
    drop(held);
    let freed = (0..100).any(|_| {
        if claim_of(&root, "worker") == Claim::Free {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        false
    });
    assert!(freed);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn exhausting_readable_names_still_returns_unique_held_names() {
    let root = scratch("exhaustion");
    let held: Vec<_> = (0..100).map(|_| claim_name(&root, "worker")).collect();
    let names: std::collections::HashSet<_> = held.iter().map(|(name, _)| name).collect();
    assert_eq!(names.len(), held.len());
    for (name, lock) in &held {
        assert!(lock.is_some(), "@{name} has no claim");
        assert_eq!(claim_of(&root, name), Claim::Held);
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn failed_serialization_leaves_no_record() {
    let root = scratch("serialization");
    let path = root.join("broken.json");
    let bad = std::collections::BTreeMap::from([((1, 2), "value")]);
    assert!(!write_json_atomic(&path, &bad));
    assert!(!path.exists());
    let _ = fs::remove_dir_all(root);
}
