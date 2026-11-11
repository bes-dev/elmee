//! What the host offers the model, and how to use it.
//!
//! Declared apart from the runtime that answers these calls, for the same
//! reason the core keeps tool schemas away from tool implementations: a
//! description is a contract with the model and changes for reasons of
//! prompting, while what carries it out changes for reasons of behaviour.

/// What `spawn_agent` says about itself — the judgement of delegating as well
/// as the mechanics, because a description is paid for only when its tool is
/// offered, which is exactly what the system prompt cannot be.
///
/// Computed rather than a literal because the roles a machine declares are its
/// own, and naming them here is what lets an agent ask for one without being
/// told about models anywhere else. Built once at startup and unchanged for the
/// session, so it rides inside the cached prefix like every other schema.
fn spawn_description(roster: Option<&str>) -> String {
    let base = "Start an independent background agent — another elmee, or a program the project \
                declares. Returns at once with an id, never \
                with an answer: the report arrives later as its own message. Each agent is a \
                separate process with its own context and its own cold cache, so it costs far more \
                than doing the work here — reach for one only when parts can genuinely run at the \
                same time, or the work must outlive this session. Not for anything you would \
                finish yourself in a few calls. Having delegated, do not also do it yourself. \
                Parallel agents fail by making conflicting assumptions, not by working slowly: \
                give each one its objective, the constraints it must honour, and the report you \
                expect back. Fan out readers freely; keep writers in one lane unless the work \
                genuinely splits by path — agents run in this same working directory, nothing \
                enforces a split, and two agents editing one file lose one of the two edits \
                silently. To verify a claim that matters, ask a fresh agent to REFUTE it rather \
                than review it, and treat \"I could not refute it\" as the only passing answer; \
                when sweeping for an unknown number of findings, stop only after two rounds \
                surface nothing new";
    match roster {
        None => base.to_owned(),
        Some(roster) => format!("{base}. {roster}"),
    }
}

/// The tools this runtime answers through `host_tool`. They are declared here,
/// next to their implementation, because the core never executes them.
pub fn host_tools(roster: Option<&str>) -> Vec<serde_json::Value> {
    let tool = |name, description, schema: &str| {
        elmee_plugin_tools::tools::tool(
            name,
            description,
            serde_json::from_str(schema).expect("valid schema"),
        )
    };
    vec![
        tool(
            "spawn_agent",
            &spawn_description(roster),
            r#"{"type":"object","properties":{"name":{"type":"string","description":"Short unique name"},"task":{"type":"string","description":"Self-contained task with paths, constraints, and expected report"},"mode":{"type":"string","enum":["research","workspace"],"default":"research","description":"What kind of work this is. It selects the model your models.json declares for that word, and labels the task in the agent list; it grants nothing, since a child can reach whatever this session can"},"model":{"type":"string","description":"Which model this agent runs on: a declared alias, or a provider/model spec. Omit to take this session's model — never invent one, and never put a role name here"},"role":{"type":"string","description":"What this agent is: a role the project declares in .elmee/roles. Gives it instructions and narrows the skills and tools it is offered"},"agent":{"type":"string","description":"Another program in place of elmee: an agent the project declares in .elmee/agents.json, any that speaks the Agent Client Protocol on stdio. It brings its own model and tools; model and role do not apply, and send_to_agent cannot reach it"},"schema":{"type":"string","description":"Optional JSON Schema; the agent must end its report with one matching JSON object, so results can be consumed without parsing prose"}},"required":["name","task"]}"#,
        ),
        tool(
            "send_to_agent",
            "Send a message or course correction to a named elmee agent in this project. Use it to \
             open an exchange with someone who has not just asked you: answering a peer needs \
             nothing from you, since whatever you conclude is returned to whoever asked",
            r#"{"type":"object","properties":{"to":{"type":"string"},"message":{"type":"string"}},"required":["to","message"]}"#,
        ),
        tool(
            "stop_agent",
            "Stop a background agent spawned by this session",
            r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}"#,
        ),
        tool(
            "wait_agents",
            "Ask about background agents, or wait for one. With no ids and timeout_ms 0 it answers at \
             once with every elmee agent in this project — that is the whole status query, and there is \
             deliberately no second tool to call in a loop. Each entry carries a task when this session \
             started it, which is also what says you may wait on, steer, or stop it; one without is \
             someone else's. With ids it blocks without polling until all or any of them reach a \
             terminal state. Do not poll a running agent: you are told when it ends, so carry on \
             with what the delegation does not cover, and wait only when you cannot proceed \
             without the result",
            r#"{"type":"object","properties":{"ids":{"type":"array","items":{"type":"string"},"description":"Omit for the status of every agent"},"mode":{"type":"string","enum":["all","any"],"default":"all"},"timeout_ms":{"type":"integer","minimum":0,"maximum":300000,"default":60000}},"required":[]}"#,
        ),
    ]
}
