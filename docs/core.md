# Core kernel

## Contract of the crate

`elmee-core` defines a provider-neutral agent loop and the minimum state needed
to decide when a turn is complete. Its public ports are implemented by plugins
or the host. Its Cargo dependencies are only `async-trait`, `serde` and
`serde_json`.

Core has no executor and does not start a runtime. It does not read application
configuration, choose a provider, open a session file or render an interface.

## Module map

### `agent/`

The turn state machine. `Agent` owns:

- the system prompt and active `Session`;
- the resolved `Registry`;
- workspace capability and evidence manifest;
- live plan state;
- token counters, optional shared ledger and compaction threshold;
- pending application barriers and finish-gate repetition state.

`agent/mod.rs` orchestrates prompts, background events, provider turns, tools,
finish gates and public runtime controls. `agent/budget.rs` contains token
budgeting and `agent/compaction.rs` keeps room in the context. `agent/answers.rs` implements loop-owned answers,
kernel tools and plugin completion claims.

The module emits `AgentEvent` values rather than calling a UI. Frontends decide
how text, reasoning, tool progress, plans, compaction and model usage are
presented.

### `builder.rs`

`AgentBuilder` is the validated assembly path for one agent. It owns a registry
while plugins are installed, adds the kernel tool schemas, applies the optional
global allowlist and invokes agent-specific resolution before constructing
`Agent`.

The builder is intentionally small. Product profiles, provider selection and
frontend policy belong in `apps/`, not in builder variants.

### `plugin.rs`

The universal composition contract described in
[Plugin composition](composition.md). It defines:

- `Plugin`, `Registrar`, `Registry` and typed `Services`;
- `Handler` and `Middleware`;
- neutral tool outcomes;
- the model, workspace and session-seed service ports;
- deterministic manifests.

This is the most sensitive kernel surface. New methods must describe a general
loop lifecycle, not one feature. A new role or phase requires two independent
production consumers.

### `provider.rs`

The model port. It contains neutral message-to-model and stream-from-model
types:

- `Provider` for completion, model switching and model listing;
- `AssistantTurn` for content, reasoning blocks, tool calls and token counts;
- `Chunk` for streamed text and reasoning.

Wire-specific terminal markers, retry policies, authentication and request
formats belong to `plugins/provider`.

### `message.rs`

The provider-neutral conversation representation:

- roles and message content;
- tool calls and tool-call IDs;
- reasoning/thinking data;
- image attachments;
- message origin and durable background event IDs.

Provider adapters translate this vocabulary. Persistence adapters serialize it.
Neither owns its semantics.

### `session.rs`

An append-only conversation tree. Every entry has an ID, optional parent,
message and compaction metadata. The active branch is rebuilt into the exact
message list sent to a provider.

The tree supports:

- appending messages without rewriting history;
- reset, fork and checkout;
- compaction nodes with a recent verbatim tail;
- deduplication of durable background events;
- detection and repair of dangling tool calls.

`SessionStore` is the persistence port. Core owns entry schema and branch
invariants; the session plugin owns JSONL locking, append and recovery.

### `interaction.rs`

The host boundary.

`Interactor` handles:

- user questions;
- frontend-owned host tools;
- standing-goal refutation by a fresh host capability;
- background event injection and acknowledgement.

### `evidence.rs`

Evidence ties a completion claim to the workspace state it observed. Each entry
records kind, claim, command, tree fingerprint, time, output digest,
verification level and staleness.

Executable verification levels form an ordered sequence:

```text
unverified < static < integration < e2e
```

`external` is deliberately separate rather than pretending to be stronger or
weaker than executable verification. A changed workspace fingerprint makes
tree-bound evidence stale. Evidence lives in memory for the life of the agent:
proof of an earlier run is not proof of this one.

The process plugin understands command ecosystems and supplies neutral
`CommandEvidence`; core only applies the current tree fingerprint and finish
requirement.

### `plan.rs`

Loop-owned live plan state. It validates plan transitions, tracks open and
completed steps and exposes an event representation to the frontend. Plan data
is carried through compaction because it is part of active turn control.

This is not standing product intent. Long-lived objectives and their
continuation policy live in `plugins/goal`.

### `text.rs`

Small provider-independent text boundary utilities:

- UTF-8-safe bounding;
- encoding helpers;
- fencing and marking untrusted external content.

The module prevents every adapter from reimplementing truncation and provenance
slightly differently without pulling an HTML or protocol dependency into core.

### `error.rs`

A compact cross-boundary error vocabulary. Plugins translate transport and
domain errors into configuration, provider, I/O or general operation errors
without importing their concrete error types into core.

### `lib.rs`

The public facade and tiny dependency-free utilities: current Unix time, short
unique identifiers, stable local hashes, stable path identity, an output budget bounded by the model
context and the compaction threshold a context window implies. These helpers are admitted only when multiple plugins need the same
neutral primitive and a separate mechanism crate would add more boundary than
behavior.

## Kernel tools

`plan` and `ask_user` participate in the same global definition namespace as
handler tools:

- collisions are rejected;
- `--tools` can filter them;
- the model sees ordinary function schemas;
- execution is routed before plugin handlers because it requires private loop
  state.

No plugin should declare either schema. Conversely, a new product feature must
not become a kernel tool simply to avoid writing a handler. It qualifies only
if no plugin can implement it without privileged access to private turn state.

`AgentBuilder::interactive(false)` omits `ask_user` when the frontend cannot
answer it. The stock one-shot frontend uses this; TUI and ACP keep it. An
explicit allowlist requesting an unavailable tool is rejected, not silently
expanded to include a capability the frontend cannot fulfil.

## Invariants of a turn

The kernel maintains these important invariants:

1. A system prompt exists before the first provider request.
2. Every recorded model tool call eventually has a tool result, including an
   interrupted marker after recovery.
3. No ordinary handler preparation or execution occurs before `BeforeTool`
   middleware has passed the call.
4. The model sees exactly the resolved, globally filtered tool namespace.
5. Auxiliary summarization/completion calls receive no tool schemas.
6. Evidence applies only to the workspace fingerprint it observed.
7. A fresh user message resets any repeated finish-gate escape.
8. Background events are deduplicated by durable event ID.
9. Token usage is charged after every model request, including auxiliary ones.
10. The runaway step guard returns a recorded explanation rather than silently
    ending or losing already applied work.

## API and concurrency invariants

- `Agent` is not `Send` and must have one local-task owner.
- Model calls and handler state transitions are serialized per agent.
- Event callbacks may be `Send` because presentation can cross a channel.
- `Pending` may wrap sendable work, but settling occurs at a defined barrier.
- Persistence traits that may be owned behind broader boundaries use `Send`
  where required; this does not imply a shared multithreaded registry.
- Parallel requests use separate agents, never a mutex around one global
  agent.

## Rules for future core changes

Before editing core for a feature, answer all of these:

1. Is the behavior required for an agent turn to be semantically valid?
2. Can it be expressed as a service, handler, middleware or host
   interaction?
3. Does the proposed API name a provider, product, protocol or feature?
4. Are there two unrelated production consumers for a new universal hook?
5. Can the invariant be tested without installing the feature that motivated
   it?

If the second answer is yes, the change belongs outside core. If the third is
yes or the fourth is no, the universal contract is almost certainly too broad.

