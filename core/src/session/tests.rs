//! Tests for the module beside this one.
//!
//! A child module, so it still reaches the private items it is testing and
//! is still compiled out of a release build — the only thing it stops doing is
//! competing with the code for the reader.

use super::*;
use crate::message::ToolCall;

#[test]
fn reports_dangling_tool_calls() {
    let mut session = Session::memory();
    session.push(Message::user("go")).unwrap();
    session
        .push(Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall::new("a", "read", "{}"), ToolCall::new("b", "read", "{}")],
            tool_call_id: None,
            thinking: Vec::new(),
            origin: crate::message::MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        })
        .unwrap();
    session.push(Message::tool("a", "done")).unwrap();
    assert_eq!(session.dangling_tool_calls(), vec!["b".to_owned()]);
}

#[test]
fn branches_without_copying_history() {
    let mut session = Session::memory();
    let root = session.push(Message::system("system")).unwrap();
    session.push(Message::user("branch a")).unwrap();
    session.checkout(root).unwrap();
    session.push(Message::user("branch b")).unwrap();

    assert_eq!(session.messages().len(), 2);
    assert_eq!(session.messages()[1].content.as_deref(), Some("branch b"));
    assert_eq!(session.entries.len(), 3);
}

#[test]
fn compaction_replaces_old_context_but_preserves_tree() {
    let mut session = Session::memory();
    session.push(Message::system("system")).unwrap();
    session.push(Message::user("large old context")).unwrap();
    session.compact("short summary".into(), 0).unwrap();
    session.push(Message::user("new work")).unwrap();

    assert_eq!(session.entries.len(), 4);
    assert_eq!(session.messages().len(), 3);
    assert!(session.messages()[1].content.as_deref().unwrap().contains("short summary"));
}

#[test]
fn compaction_keeps_the_newest_turns_verbatim_after_the_summary() {
    let mut session = Session::memory();
    session.push(Message::system("system")).unwrap();
    session.push(Message::user("ancient history that the summary will absorb")).unwrap();
    session
        .push(Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall::new("t1", "read", "{}")],
            tool_call_id: None,
            thinking: Vec::new(),
            origin: crate::message::MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        })
        .unwrap();
    session.push(Message::tool("t1", "file contents")).unwrap();
    session.push(Message::user("the exact instruction that must survive")).unwrap();

    session.compact("summary".into(), 40).unwrap();
    let context: Vec<_> = session.messages().iter().map(|m| m.content.as_deref()).collect();

    // system, then the summary, then raw turns — and the tool result is
    // not stranded without the assistant message that called it.
    assert_eq!(context[0], Some("system"));
    assert!(context[1].unwrap().contains("summary"));
    assert_eq!(context.last().unwrap(), &Some("the exact instruction that must survive"));
    assert!(!matches!(session.messages()[2].role, Role::Tool));
}

#[test]
fn a_rewind_keeps_what_followed_as_a_branch_to_return_to() {
    let mut session = Session::memory();
    session.push(Message::system("system")).unwrap();
    let first = session.push(Message::user("first")).unwrap();
    session.checkpoint("tree-before-first".into()).unwrap();
    session.checkpoint("a later tree".into()).unwrap();
    session.push(Message::assistant_text("one")).unwrap();
    let second = session.push(Message::user("second")).unwrap();
    let end = session.push(Message::assistant_text("two")).unwrap();

    let asked = session.prompts();
    assert_eq!(asked.iter().map(|prompt| prompt.text.as_str()).collect::<Vec<_>>(), ["first", "second"]);
    assert_eq!(
        asked[0].checkpoint.as_deref(),
        Some("tree-before-first"),
        "the first record for a turn stands"
    );
    assert_eq!(asked[1].checkpoint, None);

    assert_eq!(session.rewind(second).unwrap().text, "second");
    assert_eq!(session.messages().len(), 3, "back to just before it was asked");
    assert_eq!(session.prompts().len(), 1);
    assert!(session.rewind(second).is_err(), "it is no longer on this branch");
    session.push(Message::user("second, reworded")).unwrap();

    let branches = session.branches();
    assert_eq!(branches.len(), 2);
    assert_eq!(
        (branches[0].last.as_str(), branches[0].prompts, branches[0].active),
        ("second, reworded", 2, true)
    );
    assert_eq!((branches[1].leaf, branches[1].last.as_str(), branches[1].active), (end, "second", false));
    session.checkout(end).unwrap();
    assert_eq!(session.messages().last().unwrap().content.as_deref(), Some("two"));

    assert_eq!(session.rewind(first).unwrap().checkpoint.as_deref(), Some("tree-before-first"));
}
