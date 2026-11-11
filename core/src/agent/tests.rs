//! Tests for the module beside this one.
//!
//! A child module, so it still reaches the private items it is testing and
//! is still compiled out of a release build — the only thing it stops doing is
//! competing with the code for the reader.

use std::{
    collections::VecDeque,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Handler, InteractionRequest, InteractionResponse, Plugin, PluginFuture, Registrar, Registry, Workspace,
    message::ToolCall,
    provider::{AssistantTurn, Provider},
};

use super::*;

/// Everything said in the active context, for assertions about what a turn
/// left behind.
fn transcript(agent: &Agent) -> String {
    agent.messages().iter().filter_map(|message| message.content.as_deref()).collect::<Vec<_>>().join("\n")
}

struct MockProvider(Mutex<VecDeque<AssistantTurn>>);

#[derive(Clone)]
struct SharedLedger(Arc<AtomicU64>);

impl TokenLedger for SharedLedger {
    fn spent(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::Relaxed))
    }

    fn charge(&self, tokens: u64) -> Result<()> {
        self.0.fetch_add(tokens, Ordering::Relaxed);
        Ok(())
    }
}

#[crate::async_trait(?Send)]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[serde_json::Value],
        _on_text: &mut (dyn for<'chunk> FnMut(crate::provider::Chunk<'chunk>) + Send),
    ) -> Result<AssistantTurn> {
        Ok(self.0.lock().unwrap().pop_front().unwrap())
    }

    fn set_model(&mut self, _model: String) {}

    async fn list_models(&self) -> Result<Vec<crate::provider::Model>> {
        Ok(Vec::new())
    }
}

struct AnswerQuestions;

impl Interactor for AnswerQuestions {
    async fn request(&mut self, request: InteractionRequest) -> InteractionResponse {
        assert!(matches!(request, InteractionRequest::Questions { .. }));
        InteractionResponse::Answers(vec![vec!["Rust".into()]])
    }
}

/// A frontend that answers the agent tools and nothing else. Anything else it
/// is asked panics: nothing in a turn should be putting questions to it.
struct HostOnly;

const HOST_TOOLS: [&str; 3] = ["wait_agents", "stop_agent", "spawn_agent"];

impl Interactor for HostOnly {
    async fn request(&mut self, _: InteractionRequest) -> InteractionResponse {
        panic!("host tools must not put a question to the frontend")
    }

    fn handles_host_tool(&self, call: &ToolCall) -> bool {
        HOST_TOOLS.contains(&call.function.name.as_str())
    }

    async fn host_tool(&mut self, call: &ToolCall) -> Option<String> {
        self.handles_host_tool(call).then(|| "[]".into())
    }
}

/// An empty directory of this test's own to be the agent's project.
///
/// Handing an agent the shared temp directory made every turn's tree hash run
/// `git add --all` over whatever else the machine had put there — which is
/// both slow and a test reading state it does not own.
fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("elmee-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// An assistant turn that only calls tools, written out five times before this.
fn calling(calls: Vec<ToolCall>) -> AssistantTurn {
    AssistantTurn { content: None, tool_calls: calls, thinking: Vec::new(), tokens: (0, 0), metrics: None }
}

/// The loop's whole view of its environment, small enough to read: one real
/// tool (`write`, so dispatch and session recording are exercised against the
/// filesystem), everything else refused, every gate open.
struct StubTools {
    cwd: std::path::PathBuf,
    definitions: Vec<serde_json::Value>,
}

struct StubWorkspace(std::path::PathBuf);

struct TestContinuation {
    state: std::rc::Rc<std::cell::RefCell<TestContinuationState>>,
    definitions: Vec<serde_json::Value>,
}

#[derive(Default)]
struct TestContinuationState {
    objective: Option<String>,
    turns: usize,
    max_turns: usize,
    stopped: bool,
}

impl Default for TestContinuation {
    fn default() -> Self {
        Self {
            state: std::rc::Rc::new(std::cell::RefCell::new(TestContinuationState::default())),
            definitions: vec![serde_json::json!({
                "type":"function","function":{"name":"complete_work","parameters":{}}
            })],
        }
    }
}

impl StubTools {
    /// Plus the frontend's agent tools, offered the way a real host offers
    /// them: declared by the handler that carries their schemas, so they are
    /// classified and gated like everything else, and answered by the
    /// interactor rather than executed here.
    fn new(cwd: &std::path::Path) -> Self {
        let named = |name| serde_json::json!({"type":"function","function":{"name":name,"parameters":{}}});
        let mut definitions = vec![named("write")];
        definitions.extend(HOST_TOOLS.map(named));
        Self { cwd: cwd.to_owned(), definitions }
    }
}

impl Handler for StubTools {
    fn definitions(&self) -> &[serde_json::Value] {
        &self.definitions
    }
    fn execute<'a>(&'a mut self, call: &'a ToolCall) -> PluginFuture<'a, crate::ToolOutcome> {
        Box::pin(async move {
            if call.function.name != "write" {
                return crate::ToolOutcome::new(
                    format!("Error: unknown tool: {}", call.function.name),
                    false,
                );
            }
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap();
            let path = self.cwd.join(args["path"].as_str().unwrap());
            fs::write(path, args["content"].as_str().unwrap()).unwrap();
            crate::ToolOutcome::new("written", true)
        })
    }
}

impl Workspace for StubWorkspace {
    fn cwd(&self) -> &std::path::Path {
        &self.0
    }
}

impl Handler for TestContinuation {
    fn definitions(&self) -> &[serde_json::Value] {
        &self.definitions
    }

    fn execute<'a>(&'a mut self, _: &'a ToolCall) -> PluginFuture<'a, crate::ToolOutcome> {
        Box::pin(async { crate::ToolOutcome::new(String::new(), true) })
    }
    fn commands(&self) -> &[crate::CommandSpec] {
        &[
            crate::CommandSpec { name: "test.start", description: "start" },
            crate::CommandSpec { name: "test.status", description: "status" },
        ]
    }
    fn command(&mut self, name: &str, argument: &str) -> Option<Result<crate::CommandOutcome>> {
        let mut state = self.state.borrow_mut();
        match name {
            "test.start" => {
                let (objective, turns) = argument.rsplit_once('|').unwrap();
                *state = TestContinuationState {
                    objective: Some(objective.into()),
                    max_turns: turns.parse().unwrap(),
                    ..Default::default()
                };
                Some(Ok(crate::CommandOutcome::message("started")))
            }
            "test.status" => Some(Ok(crate::CommandOutcome::message(
                state
                    .objective
                    .as_ref()
                    .map(|objective| {
                        let label = if state.stopped { "out of turns" } else { "active" };
                        format!("continuation [{label}] {objective}")
                    })
                    .unwrap_or_else(|| "inactive".into()),
            ))),
            _ => None,
        }
    }
}

struct TestContinuationMiddleware(std::rc::Rc<std::cell::RefCell<TestContinuationState>>);

impl crate::Middleware for TestContinuationMiddleware {
    fn on<'a>(
        &'a mut self,
        event: crate::MiddlewareEvent<'a>,
    ) -> PluginFuture<'a, Result<Vec<crate::MiddlewareEffect>>> {
        Box::pin(async move {
            let crate::MiddlewareEvent::TurnFinished { .. } = event else { return Ok(Vec::new()) };
            let mut state = self.0.borrow_mut();
            if state.objective.is_none() || state.stopped {
                return Ok(Vec::new());
            }
            let prompt = if state.turns >= state.max_turns {
                state.stopped = true;
                "start no new work; wrap up"
            } else {
                state.turns += 1;
                "continue the standing objective"
            };
            Ok(vec![crate::MiddlewareEffect::Continue(prompt.into())])
        })
    }
}

struct StubPlugin {
    cwd: std::path::PathBuf,
}

impl Plugin for StubPlugin {
    fn id(&self) -> &'static str {
        "test-tools"
    }

    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_handler(StubTools::new(&self.cwd))?;
        registrar.provide_service::<dyn Workspace>(std::rc::Rc::new(StubWorkspace(self.cwd)));
        let continuation = TestContinuation::default();
        registrar.provide_middleware(TestContinuationMiddleware(continuation.state.clone()))?;
        registrar.provide_handler(continuation)
    }
}

struct ModelPlugin(MockProvider);

impl Plugin for ModelPlugin {
    fn id(&self) -> &'static str {
        "test-model"
    }

    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_service(std::rc::Rc::new(crate::ModelService::new(self.0)));
        Ok(())
    }
}

struct LedgerPlugin(SharedLedger);

impl Plugin for LedgerPlugin {
    fn id(&self) -> &'static str {
        "test-ledger"
    }

    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_service::<dyn TokenLedger>(std::rc::Rc::new(self.0));
        Ok(())
    }
}

fn stub_plugins(cwd: &std::path::Path) -> Registry {
    let mut plugins = Registry::new();
    plugins.install(StubPlugin { cwd: cwd.to_owned() }).unwrap();
    plugins
}

fn agent(turns: impl Into<VecDeque<AssistantTurn>>, session: Session, cwd: &std::path::Path) -> Agent {
    let mut plugins = stub_plugins(cwd);
    plugins.install(ModelPlugin(MockProvider(Mutex::new(turns.into())))).unwrap();
    plugins.install(session).unwrap();
    Agent::new(plugins, 4, 0, "test system prompt".into()).unwrap()
}

fn agent_with_ledger(
    turns: impl Into<VecDeque<AssistantTurn>>,
    cwd: &std::path::Path,
    ledger: SharedLedger,
) -> Agent {
    let mut plugins = stub_plugins(cwd);
    plugins.install(ModelPlugin(MockProvider(Mutex::new(turns.into())))).unwrap();
    plugins.install(Session::memory()).unwrap();
    plugins.install(LedgerPlugin(ledger)).unwrap();
    Agent::new(plugins, 4, 0, "test system prompt".into()).unwrap()
}

#[tokio::test]
async fn executes_tool_calls_until_final_answer() {
    let cwd = std::env::temp_dir().join(format!("elmee-agent-test-{}", std::process::id()));
    fs::create_dir_all(&cwd).unwrap();
    let mut agent = agent(
        [
            calling(vec![ToolCall::new("call-1", "write", r#"{"path":"answer.txt","content":"42"}"#)]),
            AssistantTurn::text("done"),
        ],
        Session::memory(),
        &cwd,
    );
    assert_eq!(
        agent.run_interactive("write the answer".into(), &mut |_| {}, &mut RejectInteractions).await.unwrap(),
        "done"
    );
    assert_eq!(fs::read_to_string(cwd.join("answer.txt")).unwrap(), "42");
    fs::remove_dir_all(cwd).unwrap();
}

/// A child manages the children it started without anything in the turn
/// getting between them: the calls are answered by the frontend and nothing
/// else is consulted.
#[tokio::test]
async fn an_agent_manages_its_own_children_without_interruption() {
    let cwd = scratch("host-children");
    let reap = vec![
        ToolCall::new("host-1", "wait_agents", r#"{"ids":["a"]}"#),
        ToolCall::new("host-2", "stop_agent", r#"{"id":"a"}"#),
    ];
    let mut reaper = agent([calling(reap), AssistantTurn::text("done")], Session::memory(), &cwd);
    assert_eq!(reaper.run_interactive("delegate".into(), &mut |_| {}, &mut HostOnly).await.unwrap(), "done");
    let tree = transcript(&reaper);
    assert_eq!(tree.matches("[]").count(), 2, "every reaping call was answered: {tree}");
    fs::remove_dir_all(cwd).unwrap();
}

/// Tool schemas render at the very front of a request, so a set that changes
/// mid-session throws away the prompt cache and the whole conversation with it —
/// one turn at roughly twelve times the price of a cached one. Activating a
/// continuation must therefore change its state and touch nothing else.
#[test]
fn activating_a_continuation_leaves_the_set_of_tools_alone() {
    let cwd = scratch("continuation-tools");
    let mut agent = agent([AssistantTurn::text("done")], Session::memory(), &cwd);

    let declared = agent.plugins.definitions().to_vec();
    let named =
        |tools: &[serde_json::Value], name: &str| tools.iter().any(|tool| tool["function"]["name"] == name);
    assert!(named(&declared, "complete_work"), "the claim tool is declared from the start");

    agent.plugin_command("test.start", "ship it|4").unwrap();
    assert_eq!(agent.plugins.definitions(), declared.as_slice(), "activation must not add a tool");
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn hitting_the_runaway_guard_is_an_answer_and_stays_in_the_session() {
    let cwd = std::env::temp_dir().join(format!("elmee-guard-test-{}", std::process::id()));
    fs::create_dir_all(&cwd).unwrap();
    // A model that only ever calls tools: nothing but the guard ends this.
    let looping = || calling(vec![ToolCall::new("call", "write", r#"{"path":"x.txt","content":"x"}"#)]);
    let mut agent = agent([looping(), looping(), looping(), looping()], Session::memory(), &cwd);
    let answer = agent.run_interactive("go".into(), &mut |_| {}, &mut RejectInteractions).await.unwrap();

    // An Err here would never reach the session, which is what made the
    // guard indistinguishable from the agent simply going quiet.
    assert!(answer.contains("runaway guard"), "{answer}");
    let last = agent.session.messages().last().unwrap();
    assert!(matches!(last.role, crate::message::Role::Assistant));
    assert_eq!(last.content.as_deref(), Some(answer.as_str()));
    fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn a_spent_token_budget_stops_the_agent_before_the_next_request() {
    let cwd = scratch("budget");
    // Two answers are queued but the budget only covers the first; a
    // second request would exhaust the mock and panic if it were made.
    let mut agent = agent(
        [
            AssistantTurn {
                content: Some("first".into()),
                tool_calls: Vec::new(),
                thinking: Vec::new(),
                tokens: (60, 40),
                metrics: None,
            },
            AssistantTurn::text("second"),
        ],
        Session::memory(),
        &cwd,
    );
    agent.set_token_budget(100);

    assert_eq!(agent.run("one".into(), &mut |_| {}).await.unwrap(), "first");
    assert_eq!(agent.tokens_left(), Some(0));
    let stopped = agent.run("two".into(), &mut |_| {}).await.unwrap_err().to_string();
    assert!(stopped.contains("token budget"), "{stopped}");
}

/// A spent budget stops the next paid request whatever it is for. Compaction
/// ran ahead of the check, so an agent out of money still paid for a summary
/// before being told it could not continue.
#[tokio::test]
async fn a_spent_token_budget_stops_compaction_too() {
    let cwd = scratch("budget-compact");
    // One answer is queued: a compaction request would take it, and the mock
    // panics on a request it has no answer for.
    let mut agent = agent(
        [AssistantTurn {
            content: Some("first".into()),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            tokens: (60, 40),
            metrics: None,
        }],
        Session::memory(),
        &cwd,
    );
    agent.set_token_budget(100);
    assert_eq!(agent.run("one".into(), &mut |_| {}).await.unwrap(), "first");
    agent.compact_chars = 1;
    let stopped = agent.run("two".into(), &mut |_| {}).await.unwrap_err().to_string();
    assert!(stopped.contains("token budget"), "{stopped}");
    assert!(agent.compact().await.is_err(), "and asked for directly it is refused the same way");
}

#[tokio::test]
async fn separate_agents_charge_one_shared_budget_ledger() {
    let cwd = scratch("ledger");
    let ledger = SharedLedger(Arc::new(AtomicU64::new(0)));
    let mut first = agent_with_ledger(
        [AssistantTurn {
            content: Some("spent".into()),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            tokens: (70, 30),
            metrics: None,
        }],
        &cwd,
        ledger.clone(),
    );
    first.set_token_budget(100);
    assert_eq!(first.run("one".into(), &mut |_| {}).await.unwrap(), "spent");

    let mut second = agent_with_ledger([AssistantTurn::text("must not run")], &cwd, ledger);
    second.set_token_budget(100);
    assert_eq!(second.tokens_left(), Some(0));
    let stopped = second.run("two".into(), &mut |_| {}).await.unwrap_err().to_string();
    assert!(stopped.contains("across this agent tree"), "{stopped}");
}

#[tokio::test]
async fn required_verification_sends_an_unproven_finish_back_once() {
    let cwd = scratch("verify");
    let mut agent = agent(
        [AssistantTurn::text("first claim"), AssistantTurn::text("honest unverified handoff")],
        Session::memory(),
        &cwd,
    );
    agent.require_verification(VerificationLevel::Static);
    let answer = agent.run("finish safely".into(), &mut |_| {}).await.unwrap();
    assert_eq!(answer, "honest unverified handoff");
    assert_eq!(agent.evidence().level(), VerificationLevel::Unverified);
    assert!(transcript(&agent).contains("completion requires Static verification"));
}

#[tokio::test]
async fn verification_escape_is_scoped_to_one_user_turn() {
    let cwd = scratch("verify-reset");
    let mut agent = agent(
        [
            AssistantTurn::text("first claim"),
            AssistantTurn::text("first honest handoff"),
            AssistantTurn::text("second claim"),
            AssistantTurn::text("second honest handoff"),
        ],
        Session::memory(),
        &cwd,
    );
    agent.require_verification(VerificationLevel::Static);
    assert_eq!(agent.run("first".into(), &mut |_| {}).await.unwrap(), "first honest handoff");
    assert_eq!(agent.run("second".into(), &mut |_| {}).await.unwrap(), "second honest handoff");
}

#[tokio::test]
async fn compaction_that_frees_nothing_is_not_attempted_again() {
    let cwd = scratch("compact");
    // The context is dominated by a system prompt larger than the
    // threshold, so nothing can bring it under: what follows the prompt is
    // the tail that stays verbatim anyway. The mock holds two answers and no
    // summary, so a request spent on one — now or on the next turn — would
    // exhaust it and panic.
    let mut agent = agent(
        [AssistantTurn::text("first answer"), AssistantTurn::text("second answer")],
        Session::memory(),
        &cwd,
    );
    agent.compact_chars = 100;
    agent.session.push(Message::system("s".repeat(500))).unwrap();
    agent.session.push(Message::user("earlier")).unwrap();
    agent.session.push(Message::assistant_text("earlier reply")).unwrap();

    assert_eq!(agent.run("one".into(), &mut |_| {}).await.unwrap(), "first answer");
    assert_eq!(agent.run("two".into(), &mut |_| {}).await.unwrap(), "second answer");
    assert!(agent.compaction_is_futile);
    assert!(transcript(&agent).contains("Compaction could not free room"));
}

#[tokio::test]
async fn an_exhausted_continuation_says_so_instead_of_claiming_success() {
    let provider = MockProvider(Mutex::new(VecDeque::from([
        AssistantTurn::text("one"),
        AssistantTurn::text("two"),
        AssistantTurn::text("wrapping up"),
    ])));
    let mut plugins = stub_plugins(&std::env::temp_dir());
    plugins.install(ModelPlugin(provider)).unwrap();
    plugins.install(Session::memory()).unwrap();
    let mut agent = Agent::new(plugins, 8, 0, "system".into()).unwrap();
    agent.plugin_command("test.start", "boil the ocean|1").unwrap();
    agent.run("start".into(), &mut |_| {}).await.unwrap();
    let status = agent.plugin_command("test.status", "").unwrap().message;
    assert!(status.contains("out of turns"), "{status}");
    assert!(!status.contains("achieved"), "giving up must never render as success");
    assert!(transcript(&agent).contains("start no new work"));
}

#[tokio::test]
async fn repairs_interrupted_tool_calls_before_next_turn() {
    let cwd = scratch("repair");
    let mut session = Session::memory();
    session.push(Message::system("system")).unwrap();
    session
        .push(Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall::new("orphan", "read", "{}")],
            tool_call_id: None,
            thinking: Vec::new(),
            origin: crate::message::MessageOrigin::User,
            event_id: None,
            images: Vec::new(),
        })
        .unwrap();
    let mut agent = agent([AssistantTurn::text("ok")], session, &cwd);
    assert_eq!(agent.run("continue".into(), &mut |_| {}).await.unwrap(), "ok");
    assert!(transcript(&agent).contains("interrupted"));
}

#[tokio::test]
async fn returns_structured_question_answers_to_the_model() {
    let cwd = scratch("questions");
    let mut agent = agent(
        [
            calling(vec![ToolCall::new(
                "q1",
                "ask_user",
                r#"{"questions":[{"header":"Language","question":"Which language?","options":[{"label":"Rust","description":"Native"},{"label":"Go","description":"Simple"}],"multiSelect":false}]}"#,
            )]),
            AssistantTurn::text("done"),
        ],
        Session::memory(),
        &cwd,
    );
    let mut events = Vec::new();
    assert_eq!(
        agent
            .run_interactive("ask".into(), &mut |event| events.push(event), &mut AnswerQuestions)
            .await
            .unwrap(),
        "done"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolEnd { output, .. } if output.contains("Rust")))
    );
}

/// A finished plan does not hang over the next piece of work. It used to: the
/// frontend held the only copy and nothing cleared it.
#[tokio::test]
async fn a_finished_plan_is_dropped_when_new_work_starts() {
    let cwd = scratch("plan-cleared");
    let done = r#"{"steps":[{"step":"one","status":"done"}]}"#;
    let plan = calling(vec![ToolCall::new("p", "plan", done)]);
    let turns = [plan, AssistantTurn::text("first"), AssistantTurn::text("second")];
    let mut agent = agent(turns, Session::memory(), &cwd);
    let mut sizes = Vec::new();
    let mut watch = |event| {
        if let AgentEvent::Plan { steps } = event {
            sizes.push(steps.len());
        }
    };
    agent.run("do it".into(), &mut watch).await.unwrap();
    agent.run("something else".into(), &mut watch).await.unwrap();
    assert_eq!(sizes, vec![1, 0], "recorded once, then cleared — and the frontend told so");
    assert!(agent.plan.is_empty());
    let _ = fs::remove_dir_all(cwd);
}

/// Old tool output is given up before anything is summarized, and where that
/// frees enough no request is spent at all.
#[tokio::test]
async fn old_tool_output_is_shortened_before_anything_is_summarized() {
    let cwd = scratch("elide");
    // Two answers and no summary: a summarizing request would exhaust the mock.
    let mut agent =
        agent([AssistantTurn::text("first answer"), AssistantTurn::text("second")], Session::memory(), &cwd);
    agent.compact_chars = 4_000;
    agent.schema_chars = 0;
    agent.session.push(Message::system("prompt")).unwrap();
    agent.session.push(Message::user("read the big file")).unwrap();
    let mut calling = Message::assistant_text("reading");
    calling.tool_calls = vec![ToolCall::new("1", "read", r#"{"path":"big.txt"}"#)];
    agent.session.push(calling).unwrap();
    agent.session.push(Message::tool("1", format!("first line\n{}\nlast line", "x".repeat(6_000)))).unwrap();
    agent.session.push(Message::assistant_text("it is long")).unwrap();

    let mut compacted = false;
    let answer = agent
        .run("and now?".into(), &mut |event| compacted |= matches!(event, AgentEvent::Compacted { .. }))
        .await
        .unwrap();
    assert_eq!(answer, "first answer");
    assert!(!compacted, "no summary was needed");
    let output = agent.messages()[3].content.as_deref().unwrap();
    assert!(output.starts_with("first line") && output.ends_with("last line"), "both ends say what it was");
    assert!(output.len() < 600 && output.contains("left out"), "{output}");
    assert_eq!(
        agent.messages()[2].tool_calls[0].function.arguments,
        r#"{"path":"big.txt"}"#,
        "the call stays"
    );
}

/// What a summary loses first is carried across by the harness: the user's
/// own words, whatever a plugin says must survive, and only the older part
/// of the conversation is what gets summarized.
#[tokio::test]
async fn a_summary_replaces_the_older_part_and_keeps_what_the_user_asked_word_for_word() {
    let cwd = scratch("summary");
    let mut agent = agent(
        [AssistantTurn::text("## Goal\nport it"), AssistantTurn::text("## Goal\nport it, updated")],
        Session::memory(),
        &cwd,
    );
    agent.compact_chars = 2_000;
    agent.session.push(Message::system("prompt")).unwrap();
    agent.session.push(Message::user("Port the parser. Never touch vendor/.")).unwrap();
    agent.session.push(Message::assistant_text("a".repeat(900))).unwrap();
    agent.session.push(Message::user("second ask")).unwrap();
    agent.session.push(Message::assistant_text("b".repeat(900))).unwrap();
    agent.session.push(Message::user("newest ask")).unwrap();
    agent.session.push(Message::assistant_text("recent reply")).unwrap();

    agent.compact().await.unwrap();
    let said: Vec<&str> = agent.messages().iter().filter_map(|message| message.content.as_deref()).collect();
    let summary = said[1];
    assert!(summary.contains("## Goal\nport it"), "{summary}");
    assert!(summary.contains("<asked>\nPort the parser. Never touch vendor/.\n</asked>"), "{summary}");
    assert_eq!(said.last(), Some(&"recent reply"), "the newest part stays as it was said");
    assert!(!summary.contains("recent reply"));

    // A second summary keeps what the first carried, though that part of the
    // conversation is long gone.
    agent.session.push(Message::user("third ask")).unwrap();
    agent.session.push(Message::assistant_text("c".repeat(900))).unwrap();
    agent.session.push(Message::assistant_text("latest")).unwrap();
    agent.compact().await.unwrap();
    let summaries: Vec<&str> = agent
        .messages()
        .iter()
        .filter_map(|message| message.content.as_deref())
        .filter(|text| text.contains("Conversation summary"))
        .collect();
    assert_eq!(summaries.len(), 1, "one summary stands, updated rather than stacked");
    assert!(
        summaries[0].contains("Never touch vendor/.") && summaries[0].contains("updated"),
        "{}",
        summaries[0]
    );
    assert_eq!(summaries[0].matches("Never touch vendor/.").count(), 1, "carried once, not copied");
}

/// A window nobody declared is learned from the refusal it causes.
#[tokio::test]
async fn a_request_that_does_not_fit_makes_room_and_is_asked_again() {
    let cwd = scratch("overflow");
    let mut plugins = stub_plugins(&cwd);
    struct Small(Arc<Mutex<Vec<usize>>>);
    #[crate::async_trait(?Send)]
    impl Provider for Small {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[serde_json::Value],
            _: &mut (dyn for<'chunk> FnMut(crate::provider::Chunk<'chunk>) + Send),
        ) -> Result<AssistantTurn> {
            let size: usize =
                messages.iter().filter_map(|message| message.content.as_ref()).map(String::len).sum();
            self.0.lock().unwrap().push(size);
            match size > 3_000 {
                true => Err(Error::Overflow("the request exceeds the available context size".into())),
                false => Ok(AssistantTurn::text("fits now")),
            }
        }
        fn set_model(&mut self, _: String) {}
        async fn list_models(&self) -> Result<Vec<crate::provider::Model>> {
            Ok(Vec::new())
        }
    }
    impl Plugin for Small {
        fn id(&self) -> &'static str {
            "small-model"
        }
        fn declare(self, registrar: &mut Registrar) -> Result<()> {
            registrar.provide_service(std::rc::Rc::new(crate::ModelService::new(self)));
            Ok(())
        }
    }
    let sizes = Arc::new(Mutex::new(Vec::new()));
    plugins.install(Small(sizes.clone())).unwrap();
    plugins.install(Session::memory()).unwrap();
    // Told the window is enormous, which is what an undeclared one amounts to.
    let mut agent = Agent::new(plugins, 8, 400_000, "prompt".into()).unwrap();
    agent.session.push(Message::system("prompt")).unwrap();
    agent.session.push(Message::user("read it")).unwrap();
    let mut calling = Message::assistant_text("reading");
    calling.tool_calls = vec![ToolCall::new("1", "read", "{}")];
    agent.session.push(calling).unwrap();
    agent.session.push(Message::tool("1", "y".repeat(8_000))).unwrap();
    agent.session.push(Message::assistant_text("done reading")).unwrap();

    assert_eq!(agent.run("go on".into(), &mut |_| {}).await.unwrap(), "fits now");
    let sizes = sizes.lock().unwrap();
    assert!(sizes[0] > 3_000 && *sizes.last().unwrap() <= 3_000, "{sizes:?}");
    assert!(agent.compact_chars < 400_000, "and the room is remembered as smaller");
}
