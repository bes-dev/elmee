// Separate process from writer-lock unit tests: spawning mkfifo must not
// transiently inherit their locked descriptors before exec on macOS.
#[test]
fn fifo_artifacts_are_rejected_without_waiting_for_another_endpoint() {
    use elmee_plugin_session as session;
    use std::{fs, process::Command};
    let root = std::env::temp_dir().join(format!("session-fifo-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let path = root.join("chat.jsonl");
    assert!(Command::new("mkfifo").arg(&path).status().unwrap().success());
    assert!(session::open(Some(path.clone())).is_err());
    fs::remove_dir_all(root).unwrap();
}
