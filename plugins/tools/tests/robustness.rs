//! End-to-end checks against the public API; unit behavior lives in-module.

use elmee_core::message::ToolCall;
use elmee_plugin_tools::tools::ToolSet;

#[tokio::test]
async fn output_is_bounded_and_each_tool_keeps_the_end_that_matters() {
    let root = std::env::temp_dir().join(format!("elmee-verify-bound-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    // A command's last words are where the reason is, so its tail is kept.
    let flood = r#"{"command":"head -c 300000 /dev/zero | tr '\\0' 'x'; echo the-end; exit 3"}"#;
    let (out, _) = tools.execute(&ToolCall::new("1", "exec", flood)).await;
    assert!(out.len() <= 51 * 1024 + 200, "unbounded command output: {}", out.len());
    assert!(out.contains("the-end") && out.contains("[exit status: 3]"), "the tail is what is kept");

    // A file's beginning is where it starts making sense, so its head is kept.
    let line = format!("head-marker{}\n", "x".repeat(60));
    std::fs::write(root.join("big.txt"), line.repeat(1_500)).unwrap();
    let (read, ok) = tools.execute(&ToolCall::new("2", "read", r#"{"path":"big.txt"}"#)).await;
    assert!(ok, "reading a file in the workspace succeeds: {read:?}");
    assert!(read.starts_with("head-marker"), "read must keep head");
    assert!(
        read.trim_end().ends_with("bytes]"),
        "and say the tail was dropped: {:?}",
        &read[read.len().saturating_sub(60)..]
    );
    std::fs::remove_dir_all(root).unwrap();
}
