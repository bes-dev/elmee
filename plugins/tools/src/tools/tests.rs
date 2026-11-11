//! Tests for the module beside this one.
//!
//! A child module, so it still reaches the private items it is testing and
//! is still compiled out of a release build — the only thing it stops doing is
//! competing with the code for the reader.

use std::fs;

use super::{edit::edit, *};

#[test]
fn truncates_from_either_end_on_char_boundaries() {
    let long = format!("start {} end", "é".repeat(MAX_OUTPUT));
    let head = truncate(long.clone(), true);
    assert!(head.starts_with("start é") && head.ends_with("bytes]") && head.len() <= MAX_OUTPUT + 64);
    let tail = truncate(long, false);
    assert!(tail.starts_with("[output truncated") && tail.ends_with(" end"));
}

#[test]
fn edit_uses_the_first_exact_match_after_the_cursor() {
    let content = "same\nsame\n";
    assert!(edit(content, "same", "first", 0, true).unwrap_err().contains("occurs 2 times"));
    let (once, cursor) = edit(content, "same", "first", 0, false).unwrap();
    let (twice, _) = edit(&once, "same", "second", cursor, false).unwrap();
    assert_eq!(twice, "first\nsecond\n");

    let (deleted, _) = edit("keep\ndelete\nafter\n", "delete", "", 0, true).unwrap();
    assert_eq!(deleted, "keep\nafter\n");

    let (windows, _) = edit("old\r\nnext\r\n", "old", "new", 0, true).unwrap();
    assert_eq!(windows, "new\r\nnext\r\n");
}

#[test]
fn edit_failures_tell_the_model_what_to_do_next() {
    let missing = edit("let value = 1;\n", "let  value = 1;", "x", 0, true).unwrap_err();
    assert!(missing.contains("was not found") && missing.contains("match exactly"));
    assert!(missing.contains("1 | let value = 1;"), "the retry gets current text: {missing}");
    assert!(edit("plain 'quote'\n", "plain ’quote’", "x", 0, true).is_err());
    assert!(edit("prefix suffix\n", "prefix", "x", 0, true).is_err(), "patch lines are complete lines");
}

#[test]
fn builtin_tool_names_are_unique() {
    let definitions = definitions();
    let mut names =
        definitions.iter().map(|tool| tool["function"]["name"].as_str().unwrap()).collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), definitions.len());
}

#[tokio::test]
async fn viewing_an_image_returns_pixels_instead_of_text_or_a_filename() {
    use elmee_core::Handler;

    let root = std::env::temp_dir().join(format!("elmee-image-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    // A one-pixel GIF; detection is by content, not by the filename.
    let bytes = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
    fs::write(root.join("picture.bin"), bytes).unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let outcome =
        Handler::execute(&mut tools, &ToolCall::new("image", "view_image", r#"{"path":"picture.bin"}"#))
            .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(outcome.images.len(), 1);
    assert_eq!(outcome.images[0].media_type, "image/gif");
    assert_eq!(outcome.images[0].data, elmee_core::text::base64(bytes));

    fs::write(root.join("fake.png"), "this is text").unwrap();
    for path in [root.join("fake.png"), root.join("missing.png"), root.clone()] {
        let outcome = Handler::execute(
            &mut tools,
            &ToolCall::new("bad", "view_image", serde_json::json!({"path":path}).to_string()),
        )
        .await;
        assert!(!outcome.ok, "{}", outcome.output);
        assert!(outcome.images.is_empty());
    }
    let large = fs::File::create(root.join("large.png")).unwrap();
    large.set_len(5 * 1024 * 1024 + 1).unwrap();
    let outcome =
        Handler::execute(&mut tools, &ToolCall::new("large", "view_image", r#"{"path":"large.png"}"#)).await;
    assert!(!outcome.ok && outcome.output.contains("5 MiB"));
    assert!(outcome.images.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_plugin_cannot_shadow_a_builtin_tool() {
    struct Collision(Vec<serde_json::Value>);
    impl elmee_core::Plugin for Collision {
        fn id(&self) -> &'static str {
            "collision"
        }
        fn declare(self, plugins: &mut elmee_core::Registrar) -> elmee_core::Result<()> {
            plugins.provide_handler(self)
        }
    }
    impl elmee_core::Handler for Collision {
        fn definitions(&self) -> &[serde_json::Value] {
            &self.0
        }
        fn execute<'a>(
            &'a mut self,
            _call: &'a elmee_core::message::ToolCall,
        ) -> elmee_core::PluginFuture<'a, elmee_core::ToolOutcome> {
            Box::pin(async { unreachable!("a colliding plugin is never installed") })
        }
    }

    let root = std::env::temp_dir().join(format!("elmee-plugin-collision-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let tools = ToolSet::discover(root.clone()).unwrap();
    let mut plugins = elmee_core::Registry::new();
    plugins.install(tools).unwrap();
    let plugin = Collision(vec![tool("read", "shadow", serde_json::json!({"type": "object"}))]);
    let error = match plugins.install(plugin) {
        Ok(_) => panic!("colliding plugin was installed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("collides on tool read"), "{error}");
    let _ = fs::remove_dir_all(root);
}

/// A patch as a JSON tool call, written the way a model would send one.
fn patching(id: &str, patch: &str) -> ToolCall {
    ToolCall::new(id, "apply_patch", serde_json::json!({ "patch": patch }).to_string())
}

#[tokio::test]
async fn a_patch_over_someone_else_s_change_is_refused_until_the_file_is_read_again() {
    let root = std::env::temp_dir().join(format!("elmee-freshness-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let file = root.join("shared.rs");
    fs::write(&file, "fn one() {}\n").unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();

    // A file that does not exist yet is a decision, not a collision, so
    // nothing stands in the way of adding it.
    let fresh = tools
        .execute(&patching("0", "*** Begin Patch\n*** Add File: new.rs\n+fn new() {}\n*** End Patch"))
        .await;
    assert!(fresh.1, "{fresh:?}");
    assert_eq!(fs::read_to_string(root.join("new.rs")).unwrap(), "fn new() {}\n");

    let read = tools.execute(&ToolCall::new("1", "read", r#"{"path":"shared.rs"}"#)).await;
    assert!(read.1);
    // Somebody else — a sibling agent, a hook, the editor — gets there
    // first, preserving both size and mtime. Only content identity catches it.
    let modified = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, "fn one(){ }\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();

    let change = "*** Begin Patch\n*** Update File: shared.rs\n-fn one() {}\n+fn ONE() {}\n*** End Patch";
    let blind = tools.execute(&patching("2", change)).await;
    assert!(!blind.1, "a stale patch must not report success");
    assert!(blind.0.contains("changed on disk after you read it"), "{blind:?}");
    assert!(blind.0.contains("Read it again"), "and say what to do: {blind:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "fn one(){ }\n", "theirs survived");

    // Reading again is what makes the write legitimate.
    assert!(tools.execute(&ToolCall::new("3", "read", r#"{"path":"shared.rs"}"#)).await.1);
    let redone = tools
        .execute(&patching(
            "4",
            "*** Begin Patch\n*** Update File: shared.rs\n-fn one(){ }\n+fn ONE(){ }\n*** End Patch",
        ))
        .await;
    assert!(redone.1, "{redone:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "fn ONE(){ }\n");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_patch_lands_whole_or_not_at_all() {
    let root = std::env::temp_dir().join(format!("elmee-atomic-patch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn old() {}\n").unwrap();
    fs::write(root.join("b.rs"), "old();\n").unwrap();
    fs::write(root.join("gone.rs"), "obsolete\n").unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();

    // One hunk cannot be placed, so none of the patch is written — not the
    // files before it, and not the ones it would have created or deleted.
    let refused = tools
        .execute(&patching(
            "1",
            "*** Begin Patch\n\
             *** Update File: a.rs\n-fn old() {}\n+fn new() {}\n\
             *** Add File: made.rs\n+never\n\
             *** Delete File: gone.rs\n\
             *** Update File: b.rs\n-nothere\n+x\n\
             *** End Patch",
        ))
        .await;
    assert!(!refused.1, "{refused:?}");
    assert!(refused.0.contains("hunk 1 of 1 in b.rs"), "the error names file and hunk: {refused:?}");
    assert!(refused.0.contains("nothing in this patch was written"), "{refused:?}");
    assert_eq!(fs::read_to_string(root.join("a.rs")).unwrap(), "fn old() {}\n", "untouched");
    assert!(!root.join("made.rs").exists(), "and nothing was created");
    assert!(root.join("gone.rs").exists(), "and nothing was deleted");

    // Every kind of change in one patch, applied together.
    let applied = tools
        .execute(&patching(
            "2",
            "*** Begin Patch\n\
             *** Update File: a.rs\n@@\n-fn old() {}\n+fn new() {}\n\
             *** Update File: b.rs\n*** Move to: renamed.rs\n-old();\n+new();\n\
             *** Add File: made.rs\n+first\n+second\n\
             *** Delete File: gone.rs\n\
             *** End Patch",
        ))
        .await;
    assert!(applied.1, "{applied:?}");
    assert_eq!(fs::read_to_string(root.join("a.rs")).unwrap(), "fn new() {}\n");
    assert_eq!(fs::read_to_string(root.join("renamed.rs")).unwrap(), "new();\n");
    assert!(!root.join("b.rs").exists(), "a move leaves nothing behind");
    assert_eq!(fs::read_to_string(root.join("made.rs")).unwrap(), "first\nsecond\n");
    assert!(!root.join("gone.rs").exists());
    assert!(applied.0.contains("M a.rs") && applied.0.contains("R b.rs -> renamed.rs"), "{applied:?}");
    assert!(applied.0.contains("A made.rs") && applied.0.contains("D gone.rs"), "{applied:?}");

    // Two hunks in one file compose. The exact anchor selects the scope, and
    // the second hunk starts after the first rather than requiring uniqueness.
    fs::write(root.join("c.rs"), "fn main() {\n    one two\n    let x = 1;\n    let y = 2;\n}\n").unwrap();
    let both = tools
        .execute(&patching(
            "3",
            "*** Begin Patch\n\
             *** Update File: c.rs\n@@\n-    one two\n+    1 2\n\
             @@ fn main() {\n-    let x = 1;\n-    let y = 2;\n+    let x = 42;\n+    let y = 2;\n\
             *** End Patch",
        ))
        .await;
    assert!(both.1, "{both:?}");
    assert_eq!(
        fs::read_to_string(root.join("c.rs")).unwrap(),
        "fn main() {\n    1 2\n    let x = 42;\n    let y = 2;\n}\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn an_exact_anchor_selects_between_repeated_text() {
    let root = std::env::temp_dir().join(format!("elmee-patch-anchor-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("scopes.rs"), "fn one() {\n    same();\n}\nfn two() {\n    same();\n}\n").unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let ambiguous = tools
        .execute(&patching(
            "0",
            "*** Begin Patch\n*** Update File: scopes.rs\n@@     same();\n+    after();\n*** End Patch",
        ))
        .await;
    assert!(!ambiguous.1 && ambiguous.0.contains("occurs 2 times"), "{ambiguous:?}");
    assert_eq!(
        fs::read_to_string(root.join("scopes.rs")).unwrap(),
        "fn one() {\n    same();\n}\nfn two() {\n    same();\n}\n"
    );
    let applied = tools
        .execute(&patching(
            "1",
            "*** Begin Patch\n*** Update File: scopes.rs\n@@ fn two() {\n-    same();\n+    changed();\n*** End Patch",
        ))
        .await;
    assert!(applied.1, "{applied:?}");
    assert_eq!(
        fs::read_to_string(root.join("scopes.rs")).unwrap(),
        "fn one() {\n    same();\n}\nfn two() {\n    changed();\n}\n"
    );

    fs::write(root.join("insert.txt"), "head\r\ntail\r\n").unwrap();
    let inserted = tools
        .execute(&patching(
            "2",
            "*** Begin Patch\n*** Update File: insert.txt\n@@ head\n+middle\n*** End Patch",
        ))
        .await;
    assert!(inserted.1, "{inserted:?}");
    assert_eq!(fs::read_to_string(root.join("insert.txt")).unwrap(), "head\r\nmiddle\r\ntail\r\n");
    fs::remove_dir_all(root).unwrap();
}

/// The shell as something that can be held a conversation with: started
/// without waiting, typed to, followed, and ended.
#[tokio::test]
async fn a_run_takes_input_and_a_look_shows_only_what_is_new() {
    let root = std::env::temp_dir().join(format!("elmee-run-input-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let call = |id: &str, name: &str, args: serde_json::Value| ToolCall::new(id, name, args.to_string());

    // Nothing is waiting on it, so a command that would never end on its own
    // still comes back at once.
    let started = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tools.execute(&call("1", "exec", serde_json::json!({"command": "cat", "yield_ms": 0}))),
    )
    .await
    .expect("yield_ms 0 returns without waiting for the command");
    assert!(started.1, "{started:?}");
    let id = started.0.split_whitespace().nth(2).unwrap().trim_end_matches(';').to_owned();

    // Typing to it and hearing back are one call: the answer is the point of
    // the question.
    let first = tools
        .execute(&call("2", "write_run", serde_json::json!({"id": id, "input": "hello", "yield_ms": 10_000})))
        .await;
    assert!(first.0.contains("hello"), "{first:?}");
    assert!(first.0.contains("still going"), "and the run is still there to be talked to: {first:?}");

    // Nothing to say is a plain look, and there is nothing new to see, because
    // the last call moved the offset past it.
    let idle = tools.execute(&call("3", "write_run", serde_json::json!({"id": id, "yield_ms": 200}))).await;
    assert!(idle.1 && idle.0.contains("written nothing new"), "{idle:?}");

    let second = tools
        .execute(&call("4", "write_run", serde_json::json!({"id": id, "input": "world", "yield_ms": 10_000})))
        .await;
    assert!(second.0.contains("world"), "{second:?}");
    assert!(!second.0.contains("hello"), "a second look continues rather than repeats: {second:?}");

    assert!(tools.execute(&call("5", "stop_run", serde_json::json!({"id": id}))).await.1);
    let ended =
        tools.execute(&call("6", "write_run", serde_json::json!({"id": id, "yield_ms": 10_000}))).await;
    assert!(ended.0.contains("finished with exit status"), "{ended:?}");

    // A run nobody can reach any more is refused in as many words, never
    // silently swallowed.
    let stray = tools.execute(&call("7", "write_run", serde_json::json!({"id": id, "input": "x"}))).await;
    assert!(!stray.1 && stray.0.contains(&id), "{stray:?}");
    fs::remove_dir_all(root).unwrap();
}

/// A patch that names one file twice passed every check — the file was absent
/// both times it was looked at — then created it and refused to create it
/// again, reporting failure over a tree it had already changed.
#[tokio::test]
async fn a_patch_that_names_a_file_twice_changes_nothing() {
    let root = std::env::temp_dir().join(format!("elmee-patch-twice-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let twice = "*** Begin Patch\n*** Add File: a.txt\n+first\n*** Add File: a.txt\n+second\n*** End Patch";
    let refused = tools.execute(&patching("1", twice)).await;
    assert!(!refused.1 && refused.0.contains("more than once"), "{refused:?}");
    assert!(!root.join("a.txt").exists(), "and the first of the two was not written");
    fs::remove_dir_all(root).unwrap();
}

/// A write that fails after others have landed takes them back with it.
#[cfg(unix)]
#[tokio::test]
async fn a_patch_that_fails_partway_puts_back_what_it_had_changed() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("elmee-patch-undo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("locked")).unwrap();
    fs::write(root.join("a.rs"), "one\n").unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let read = ToolCall::new("0", "read", serde_json::json!({ "path": "a.rs" }).to_string());
    assert!(tools.execute(&read).await.1);
    // The directory refuses a new file only once the patch gets to it, after
    // every check before the first write has passed.
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
    let patch = "*** Begin Patch\n*** Update File: a.rs\n-one\n+two\n*** Add File: made.rs\n+new\n\
                 *** Add File: locked/b.rs\n+never\n*** End Patch";
    let refused = tools.execute(&patching("1", patch)).await;
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    if fs::metadata(root.join("locked/b.rs")).is_ok() {
        // Running as a user the mode does not bind: nothing to observe.
        fs::remove_dir_all(root).unwrap();
        return;
    }
    assert!(!refused.1 && refused.0.contains("put back"), "{refused:?}");
    assert_eq!(fs::read_to_string(root.join("a.rs")).unwrap(), "one\n", "the edit was undone");
    assert!(!root.join("made.rs").exists(), "and so was the file it had created");
    fs::remove_dir_all(root).unwrap();
}

/// Stopping a turn stops what it was waiting on. A command left to finish on
/// its own was reported like any other when it did, and woke the agent a
/// person had just told to stop.
#[tokio::test]
async fn a_command_whose_wait_is_dropped_is_ended_and_never_reported() {
    let root = std::env::temp_dir().join(format!("elmee-run-abandon-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let marker = root.join("survived");
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let command = format!("sleep 2; touch {}", marker.display());
    let call = ToolCall::new("1", "exec", serde_json::json!({ "command": command }).to_string());
    tokio::select! {
        outcome = tools.execute(&call) => panic!("the command outlived its wait: {outcome:?}"),
        () = tokio::time::sleep(std::time::Duration::from_millis(300)) => {}
    }
    assert!(crate::runs::list(&root).is_empty(), "a stopped command is nobody's to list");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert!(!marker.exists(), "and it did not run on to its end");
    assert!(crate::runs::list(&root).is_empty());
    fs::remove_dir_all(root).unwrap();
}

/// Running out of time is a demotion, not a death.
#[tokio::test]
async fn a_command_that_outlives_its_wait_keeps_going_and_says_where() {
    let root = std::env::temp_dir().join(format!("elmee-run-demote-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let call = |id: &str, args: serde_json::Value| ToolCall::new(id, "exec", args.to_string());

    let quick = tools.execute(&call("1", serde_json::json!({"command": "printf hello"}))).await;
    assert_eq!(quick.0, "hello");

    let slow = tools
        .execute(&call("2", serde_json::json!({"command": "printf tick; sleep 60", "yield_ms": 500})))
        .await;
    assert!(slow.1, "{slow:?}");
    assert!(slow.0.contains("tick"), "what it wrote before the wait ended comes back: {slow:?}");
    assert!(slow.0.contains("was not stopped"), "{slow:?}");
    let id = slow.0.rsplit("[run ").next().unwrap().split_whitespace().next().unwrap().to_owned();
    assert!(
        tools.execute(&ToolCall::new("3", "stop_run", serde_json::json!({"id": id}).to_string())).await.1
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_precreated_legacy_temp_symlink_is_never_followed() {
    let root = std::env::temp_dir().join(format!("elmee-temp-symlink-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("elmee-temp-victim-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(&outside, "untouched").unwrap();
    std::os::unix::fs::symlink(&outside, root.join(".target.txt.elmee-tmp")).unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let result = tools
        .execute(&patching("1", "*** Begin Patch\n*** Add File: target.txt\n+safe\n*** End Patch"))
        .await;
    assert!(result.1, "{result:?}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    assert_eq!(fs::read_to_string(root.join("target.txt")).unwrap(), "safe\n");
    fs::remove_dir_all(root).unwrap();
    fs::remove_file(outside).unwrap();
}

#[tokio::test]
async fn a_path_outside_the_working_directory_is_an_ordinary_path() {
    let root = std::env::temp_dir().join(format!("elmee-tool-paths-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("elmee-tool-elsewhere-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("ok.txt"), "ok").unwrap();
    fs::write(&outside, "elsewhere").unwrap();
    let mut tools = ToolSet::discover(root.clone()).unwrap();
    let mut read = async |path: serde_json::Value| {
        tools
            .execute(&elmee_core::message::ToolCall::new(
                "1",
                "read",
                serde_json::json!({ "path": path }).to_string(),
            ))
            .await
    };

    // Relative to the working directory, absolute, or up and out of it: three
    // spellings of a path, and the account elmee runs as decides all three.
    // The tools used to answer differently from the shell about the same file,
    // which is what made a skill living in ~/.elmee unreadable while `cat` on
    // it worked.
    assert_eq!(read(serde_json::json!("ok.txt")).await.0, "ok");
    assert_eq!(read(serde_json::json!(outside)).await.0, "elsewhere");
    let up = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    assert_eq!(read(serde_json::json!(up)).await.0, "elsewhere");

    fs::remove_dir_all(root).unwrap();
    fs::remove_file(outside).unwrap();
}

/// The tool surface on inputs nobody would type. There is no boundary to
/// enforce any more, so the property left is the one that always mattered
/// more: every path is answered, none panics, and nothing comes back bare
/// enough for a model to misread.
#[tokio::test]
async fn no_input_goes_unanswered() {
    let root = std::env::temp_dir().join(format!("elmee-hostile-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("elmee-hostile-out-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(root.join("inside")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("inside/ok.txt"), "hello\n").unwrap();
    fs::write(outside.join("secret.txt"), "SECRET\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("inside/escape.txt")).unwrap();

    let mut tools = ToolSet::discover(root.join("inside")).unwrap();
    let call = |name: &str, args: serde_json::Value| ToolCall {
        id: "1".into(),
        kind: "function".into(),
        function: elmee_core::message::FunctionCall { name: name.into(), arguments: args.to_string() },
    };
    let mut run = async |name: &str, args| tools.execute(&call(name, args)).await;

    // A path leading out of the working directory is followed, in every
    // spelling — including through a symlink, which the shell would have
    // followed all along.
    let up = format!("../../{}/secret.txt", outside.file_name().unwrap().to_string_lossy());
    for (label, args) in [
        ("relative", serde_json::json!({ "path": up })),
        ("symlink", serde_json::json!({"path": "escape.txt"})),
        ("absolute", serde_json::json!({ "path": outside.join("secret.txt") })),
    ] {
        let (out, ok) = run("read", args).await;
        assert!(ok && out.contains("SECRET"), "{label}: {out}");
    }
    let (out, ok) = run(
        "apply_patch",
        serde_json::json!({"patch": "*** Begin Patch\n*** Add File: ../pwned.txt\n+x\n*** End Patch"}),
    )
    .await;
    assert!(ok, "a write above the working directory is an ordinary write: {out}");
    assert!(root.join("pwned.txt").exists());

    // A window past the end of a file used to come back empty, which reads as
    // "the file is empty" — a different fact, and the wrong one to act on.
    let (out, ok) = run("read", serde_json::json!({"path": "ok.txt", "offset": u64::MAX})).await;
    assert!(ok && out.contains("past the end"), "an empty window says why: {out}");

    // Nothing that arrives malformed is allowed to be a panic or a silence.
    for (label, name, args) in [
        ("unknown tool", "definitely_not_a_tool", serde_json::json!({})),
        ("arguments that are not an object", "read", serde_json::json!("")),
        ("a path that is a directory", "read", serde_json::json!({"path": "."})),
        ("a path with a NUL", "read", serde_json::json!({"path": "ok\u{0}.txt"})),
        ("a path longer than any", "read", serde_json::json!({"path": "a".repeat(5000)})),
        ("an offset that cannot be", "read", serde_json::json!({"path": "ok.txt", "offset": -1})),
    ] {
        let (out, ok) = run(name, args).await;
        assert!(!ok && out.starts_with("Error:"), "{label} answered {ok} with {out:?}");
    }

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}
