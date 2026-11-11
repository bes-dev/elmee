//! The loop against its real environment: real tools, real hooks, real
//! snapshots. The loop's own mechanics are proven in `elmee-core` against a
//! stub; what is proven here is that the batteries actually fit the ports.

use std::{collections::VecDeque, fs, sync::Mutex};

use elmee_core::{
    Agent, Result,
    evidence::VerificationLevel,
    message::{Message, ToolCall},
    provider::{AssistantTurn, Chunk, Provider},
    session::Session,
};
use elmee_plugin_project::hooks::Hooks;
use elmee_plugin_tools::{WorkspacePlugin, tools::ToolSet};

/// Everything said in the active context, for assertions about what a turn
/// left behind.
fn transcript(agent: &Agent) -> String {
    agent.messages().iter().filter_map(|message| message.content.as_deref()).collect::<Vec<_>>().join("\n")
}

struct MockProvider(Mutex<VecDeque<AssistantTurn>>);

struct ModelPlugin<P>(P);

impl<P: Provider + 'static> elmee_core::Plugin for ModelPlugin<P> {
    fn id(&self) -> &'static str {
        "test-model"
    }

    fn declare(self, registrar: &mut elmee_core::Registrar) -> Result<()> {
        registrar.provide_service(std::rc::Rc::new(elmee_core::ModelService::new(self.0)));
        Ok(())
    }
}

#[elmee_core::async_trait(?Send)]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[serde_json::Value],
        _on_text: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        Ok(self.0.lock().unwrap().pop_front().unwrap())
    }

    fn set_model(&mut self, _model: String) {}

    async fn list_models(&self) -> Result<Vec<elmee_core::provider::Model>> {
        Ok(Vec::new())
    }
}

/// An assistant turn that only calls tools.
fn calling(calls: Vec<ToolCall>) -> AssistantTurn {
    AssistantTurn { content: None, tool_calls: calls, thinking: Vec::new(), tokens: (0, 0), metrics: None }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("elmee-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn agent(turns: impl Into<VecDeque<AssistantTurn>>, session: Session, cwd: &std::path::Path) -> Agent {
    let mut plugins = plugins(cwd);
    plugins.install(ModelPlugin(MockProvider(Mutex::new(turns.into())))).unwrap();
    plugins.install(session).unwrap();
    Agent::new(plugins, 12, 0, "system".into()).unwrap()
}

fn plugins(cwd: &std::path::Path) -> elmee_core::Registry {
    let mut plugins = elmee_core::Registry::new();
    plugins.install(ToolSet::discover(cwd.to_owned()).unwrap()).unwrap();
    plugins.install(Hooks::discover(cwd.to_owned()).unwrap()).unwrap();
    plugins.install(WorkspacePlugin::new(cwd).with_snapshots()).unwrap();
    plugins.install(elmee_plugin_project::MemoryPlugin::new(cwd)).unwrap();
    plugins.install(elmee_plugin_goal::GoalPlugin::default()).unwrap();
    plugins.install(elmee_plugin_web::WebPlugin).unwrap();
    plugins
}

#[tokio::test]
async fn a_viewed_image_reaches_the_next_model_request_as_image_content() {
    struct Viewer(std::cell::Cell<bool>);

    #[elmee_core::async_trait(?Send)]
    impl Provider for Viewer {
        async fn complete(
            &self,
            messages: &[Message],
            tools: &[serde_json::Value],
            _: &mut (dyn for<'chunk> FnMut(Chunk<'chunk>) + Send),
        ) -> Result<AssistantTurn> {
            if !self.0.replace(true) {
                assert!(tools.iter().any(|tool| tool["function"]["name"] == "view_image"));
                return Ok(calling(vec![ToolCall::new("view", "view_image", r#"{"path":"page.gif"}"#)]));
            }
            let image = messages.iter().flat_map(|message| &message.images).next().expect("image pixels");
            assert_eq!(image.media_type, "image/gif");
            assert!(!image.data.is_empty());
            assert!(messages.iter().any(|message| message.tool_call_id.as_deref() == Some("view")));
            Ok(AssistantTurn::text("image received"))
        }

        fn set_model(&mut self, _: String) {}

        async fn list_models(&self) -> Result<Vec<elmee_core::provider::Model>> {
            Ok(Vec::new())
        }
    }

    let cwd = scratch("image-loop");
    fs::write(cwd.join("page.gif"), b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b").unwrap();
    let mut builder = elmee_core::AgentBuilder::new("system");
    builder.plugin(ModelPlugin(Viewer(std::cell::Cell::new(false)))).unwrap();
    builder.plugin(ToolSet::discover(cwd.clone()).unwrap()).unwrap();
    let mut agent = builder.build().unwrap();
    let answer = agent
        .run_interactive(
            "inspect the rendered page".into(),
            &mut |_| {},
            &mut elmee_core::interaction::RejectInteractions,
        )
        .await
        .unwrap();
    assert_eq!(answer, "image received");
    let _ = fs::remove_dir_all(cwd);
}

/// The snapshot is started before the model call and settled before the first
/// tool. What matters is that the overlap did not cost correctness: the
/// recorded tree must still be the one the turn began with, not one a tool has
/// already written to.
#[tokio::test]
async fn a_snapshot_started_behind_the_model_still_lands_before_any_tool_writes() {
    let cwd = std::env::temp_dir().join(format!("elmee-snapshot-race-{}", std::process::id()));
    let _ = fs::remove_dir_all(&cwd);
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("file.txt"), "before").unwrap();
    let snapshots = elmee_plugin_snapshot::Snapshots::new(&cwd);
    if snapshots.fingerprint().await.is_none() {
        let _ = fs::remove_dir_all(cwd); // no git on this machine
        return;
    }
    let mut agent = agent(
        [
            calling(vec![ToolCall::new(
                "c1",
                "apply_patch",
                serde_json::json!({
                    "patch": "*** Begin Patch\n*** Update File: file.txt\n-before\n+after\n*** End Patch"
                })
                .to_string(),
            )]),
            AssistantTurn::text("done"),
        ],
        Session::memory(),
        &cwd,
    );
    agent.defer_barrier(snapshots.begin("overwrite the file"));
    agent
        .run_interactive("go".into(), &mut |_| {}, &mut elmee_core::interaction::RejectInteractions)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(cwd.join("file.txt")).unwrap(), "after");

    // The loop settled the snapshot itself, at the one point it must, and
    // kept it with the prompt that started the turn. Were that await removed,
    // its `git add` would race the write above rather than precede it.
    let recorded = agent.prompts()[0].checkpoint.clone().expect("the prompt knows the tree it started from");
    snapshots.restore(&recorded).await.unwrap();
    assert_eq!(
        fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "before",
        "and what it recorded is the tree as it stood before the tool wrote"
    );
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn a_passing_completion_hook_becomes_tree_bound_evidence() {
    let cwd = std::env::temp_dir().join(format!("elmee-evidence-hook-{}", std::process::id()));
    fs::create_dir_all(cwd.join(".elmee")).unwrap();
    fs::write(
        cwd.join(".elmee/hooks.json"),
        r#"{"before_completion":[{"command":"sh","args":["-c","exit 0 # test-suite"]}]}"#,
    )
    .unwrap();
    if elmee_plugin_snapshot::Snapshots::new(&cwd).fingerprint().await.is_none() {
        let _ = fs::remove_dir_all(cwd);
        return;
    }
    let mut agent = Agent::new(
        {
            let mut plugins = plugins(&cwd);
            plugins
                .install(ModelPlugin(MockProvider(Mutex::new(VecDeque::from([AssistantTurn::text(
                    "verified",
                )])))))
                .unwrap();
            plugins.install(Session::memory()).unwrap();
            plugins
        },
        4,
        0,
        "system".into(),
    )
    .unwrap();
    agent.require_verification(VerificationLevel::Integration);
    assert_eq!(agent.run("go".into(), &mut |_| {}).await.unwrap(), "verified");
    assert_eq!(agent.evidence().level(), VerificationLevel::Integration);
    fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn a_goal_keeps_working_until_it_is_claimed_and_verified() {
    let cwd = std::env::temp_dir().join(format!("elmee-goal-test-{}", std::process::id()));
    fs::create_dir_all(cwd.join(".elmee")).unwrap();
    // The project's own check rejects a claim that offers no evidence.
    fs::write(
        cwd.join(".elmee/hooks.json"),
        r#"{"before_completion":[{"command":"sh","args":["-c","grep -q 'trust me' && { echo no proof of completion; exit 1; }; exit 0"]}]}"#,
    )
    .unwrap();
    let claim = |evidence: &str| {
        calling(vec![ToolCall::new(
            "c",
            elmee_plugin_goal::DONE_TOOL,
            serde_json::json!({ "evidence": evidence }).to_string(),
        )])
    };
    let provider = MockProvider(Mutex::new(VecDeque::from([
        AssistantTurn::text("made a start"),
        claim("trust me"),
        AssistantTurn::text("kept going"),
        claim("proof: the suite is green"),
        AssistantTurn::text("finished"),
    ])));
    let mut agent = Agent::new(
        {
            let mut plugins = plugins(&cwd);
            plugins.install(ModelPlugin(provider)).unwrap();
            plugins.install(Session::memory()).unwrap();
            plugins
        },
        12,
        0,
        "system".into(),
    )
    .unwrap();
    agent.plugin_command(elmee_plugin_goal::COMMAND, "ship the thing").unwrap();
    let answer = agent.run("start".into(), &mut |_| {}).await.unwrap();
    assert_eq!(answer, "finished");
    let tree = transcript(&agent);
    // Ending a turn without claiming continued the work, with the
    // objective quoted as data rather than as an instruction...
    assert!(tree.contains("<goal trusted=\"no\""), "{tree}");
    // ...and the unevidenced claim was refused by the project's own check.
    assert!(tree.contains("the completion check refused this claim"), "{tree}");
    assert!(agent.plugin_command(elmee_plugin_goal::COMMAND, "").unwrap().message.contains("achieved"));
    fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn a_failing_completion_gate_sends_the_agent_back_to_work() {
    let cwd = std::env::temp_dir().join(format!("elmee-gate-test-{}", std::process::id()));
    fs::create_dir_all(cwd.join(".elmee")).unwrap();
    fs::write(
        cwd.join(".elmee/hooks.json"),
        r#"{"before_completion":[{"command":"sh","args":["-c","echo tests are failing; exit 1"]}]}"#,
    )
    .unwrap();
    let provider = MockProvider(Mutex::new(VecDeque::from([
        AssistantTurn::text("all done"),
        AssistantTurn::text("really done"),
    ])));
    let mut agent = Agent::new(
        {
            let mut plugins = plugins(&cwd);
            plugins.install(ModelPlugin(provider)).unwrap();
            plugins.install(Session::memory()).unwrap();
            plugins
        },
        4,
        0,
        "system".into(),
    )
    .unwrap();
    // The gate rejects the first answer, so the agent takes another turn;
    // the identical second rejection ends it rather than looping.
    assert_eq!(agent.run("work".into(), &mut |_| {}).await.unwrap(), "really done");
    assert!(transcript(&agent).contains("tests are failing"));
    fs::remove_dir_all(cwd).unwrap();
}

/// The system prompt is assembled once, and stays exactly that while the
/// session lives.
///
/// Caching rests on it: the provider caches by prefix and the prompt is the
/// prefix, so one byte differing between two requests in a session is a miss on
/// every request after it, at roughly ten times the price and with no symptom
/// but the bill. It matters more than it did, because what feeds the prompt now
/// changes by itself — a note is written when compaction runs, a rule can be
/// armed mid-turn. Compaction is the one place the message list is rewritten,
/// so it is the one place this can break.
#[tokio::test]
async fn the_system_prompt_is_not_rebuilt_while_the_session_runs() {
    use elmee_plugin_project::memory::{Scope, Source};
    let cwd = scratch("prompt-not-rebuilt");
    let assembled = elmee_plugin_project::context::system_prompt(&cwd).unwrap();
    let turns = [AssistantTurn::text("first"), AssistantTurn::text("summary"), AssistantTurn::text("second")];
    let mut agent = Agent::new(
        {
            let mut plugins = plugins(&cwd);
            plugins.install(ModelPlugin(MockProvider(Mutex::new(turns.into())))).unwrap();
            plugins.install(Session::memory()).unwrap();
            plugins
        },
        4,
        0, // compacting on turn one would find nothing freed and latch itself off
        assembled.clone(),
    )
    .unwrap();
    agent.run("hello".into(), &mut |_| {}).await.unwrap();

    // What the prompt is built from, changing mid-session — which is what this
    // harness does on its own when compaction keeps a note. The test is worth
    // nothing unless that really would have changed the prompt, so it says so.
    elmee_plugin_project::memory::apply(Scope::Project, &cwd, Source::Agent, "a note", None).unwrap();
    assert_ne!(elmee_plugin_project::context::system_prompt(&cwd).unwrap(), assembled);

    agent.set_compact_chars(1);
    agent.run("again".into(), &mut |_| {}).await.unwrap();
    let messages = agent.messages();
    let summarized = |text: &str| text.contains("Conversation summary");
    assert!(
        messages.iter().any(|m| m.content.as_deref().is_some_and(summarized)),
        "worthless unless compaction ran"
    );
    let head = messages.first().and_then(|m| m.content.as_deref());
    assert_eq!(head, Some(assembled.as_str()), "the prefix moved, and the cache with it");
    let _ = fs::remove_dir_all(cwd);
}
