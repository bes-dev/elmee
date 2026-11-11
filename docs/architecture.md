# Architecture

## Layers and dependency direction

The workspace has three source layers:

```text
┌─────────────────────────────────────────────────────────────┐
│ apps/                                                       │
│ product composition, configuration policy, process lifetime│
└──────────────────────────────┬──────────────────────────────┘
                               │ selects
┌──────────────────────────────▼──────────────────────────────┐
│ plugins/                                                    │
│ providers, tools, stores, policies and adapters             │
└──────────────────────────────┬──────────────────────────────┘
                               │ implements ports from
┌──────────────────────────────▼──────────────────────────────┐
│ core/                                                       │
│ provider-neutral agent loop and composition contract        │
└─────────────────────────────────────────────────────────────┘
```

Cargo dependencies point downward only. Core has three dependencies and no
knowledge of Tokio, HTTP, concrete providers or presentation. Plugins may
depend on other plugins when they are integration or mechanism layers, but
reusable behavior must never depend upward on an application.

## Runtime domains

A registry belongs to one agent: it owns the model service, tool handlers,
loop middleware and optional kernel services for one `Agent`.

It is not a global container. A session has a main agent, any background
agents it spawns as processes of their own, and the host around them. The
composition root prints every service in `--explain-plugins` output.

## One agent turn

At a high level, a turn is:

```text
user prompt
    |
    v
UserPrompt middleware adds bounded context
    |
    v
append prompt -> check token budget -> compact if needed -> check token budget
    |
    v
provider.complete(messages, visible tool schemas)
    |
    +-- no tool calls --> finish gates --> continuation or answer
    |
    `-- tool calls ----> BeforeTool/execute/record --> next model turn
```

The loop repairs interrupted transcript state before work begins: dangling tool
calls receive an interrupted result. The system prompt is appended once. New
user work resets only finish-gate escape state; an unfinished plan may continue
across prompts.

Making room in the context is provider-neutral and has two stages: old tool
output is shown shortened without asking any model, and only if that is not
enough is the older part of the conversation replaced by a summary, with a
recent tail kept verbatim. See [runtime and state](runtime-and-state.md).

The provider returns a neutral `AssistantTurn`: text, reasoning, tool calls and
token counts. Provider adapters are responsible for proving that their stream
reached its protocol-specific terminal event before returning success.

## Tool execution pipeline

Tool calls follow a deliberate authority order:

```text
1. settle application barriers
2. loop-owned answers and completion claims
3. BeforeTool middleware, which a project's hook may refuse at
4. batch preparation for the calls that survived it
5. frontend-owned host tools, or handler execution
6. optional tool-requested model completion without tool schemas
7. AfterTool middleware
8. evidence binding and session append
```

Loop-owned tools run first because they manipulate private loop state. Host
tools are capabilities of the active frontend — for example waiting for a child
agent — and the frontend itself decides whether it implements them, but they
travel through the same step as any other call.

Batch preparation may overlap reversible reads, but it sees only calls a hook
did not refuse. It must not write or initiate irreversible I/O. This prevents a
denied request from causing an effect during speculative preparation.

`ToolOutcome` carries all facts learned by execution:

- output and success status;
- an optional auxiliary completion request;
- command evidence classified by the owning plugin;
- pictures the tool took for the model to look at.

Keeping these together avoids querying mutable handlers again after execution.

## Finishing a turn

A model response without tool calls is not automatically final. The kernel
checks, in order:

1. background events that arrived while the turn ran;
2. `BeforeFinish` middleware such as project completion hooks;
3. required non-stale evidence for the current workspace fingerprint;
4. `TurnFinished` middleware such as a standing-goal continuation.

A repeated byte-identical finish-gate complaint may let a stuck turn return an
answer; that escape is cleared on every new user prompt. This prevents an
unsatisfiable check from hanging forever without weakening later turns.

A standing goal claims completion through its own handler. Core runs the
`BeforeFinish` gates, asks the host to refute the claim, then tells the exact
owning handler to commit the state transition. The remaining finish gates —
evidence and `TurnFinished` — run later, when the model stops calling tools, so
a committed objective still cannot end a turn that fails them. The objective
itself never enters core.

## The system prompt

The system prompt is the application's. `apps/elmee` starts from the coding
identity in `plugins/project` and adds what the project teaches. No plugin
contributes to it:
a plugin says what it needs to say in the description of the tool it is about,
which every request carries anyway, and only when that tool is offered.

That conditionality is what a hand-written append used to imitate.
`plugins/host` exported a prompt fragment which `apps/elmee` appended whenever a
host tool survived the tool allowlist — a convention, not a contract, and a
second application would have omitted it silently. The fragment is gone: its
advice on delegating now lives in `spawn_agent`, `wait_agents` and
`send_to_agent`, and its facts about the envelopes the harness writes around
content live in the application prompt. No plugin exports a prompt fragment. A prompt handed to a child *process* is not a fragment of a parent's
prompt and stays where it is.

Two things bound that prompt, and the application owns both:

- a byte ceiling asserted in a test beside the prompt — 1536 for the coding
  base. It is a forcing function: it moves by decision, and the test comment
  records why it moved;
- a checked-in rendering of the whole assembled prompt for one fixed minimal
  composition, in `apps/elmee/tests/system-prompt.txt`. A ceiling says how much prose there
  may be; the snapshot says what the prose became once environment, project
  context, skills and memory were folded around it, and it makes any change a
  diff a person reads. Machine-specific parts — working directory, platform, a
  developer's own global `AGENTS.md` — are normalised, so the file is the same
  everywhere. The snapshot is taken off the wire from a real run, so a
  fragment appended anywhere on the way to the provider shows up in it.

A registry of prompt sections contributed by plugins, of the kind a comparable
Cordis-based harness ships, was considered and declined. Its ordering field is
an unowned global integer namespace that already carries collisions and a
fractional entry, its own retrospective calls per-plugin ordering a hand-paid
coordination cost, and it has no prompt budget at all. The stability gate asks
for two independent consumers before a new universal role; the single candidate
here dissolved into tool descriptions. Reopen it when a second, genuinely
independent contributor appears whose content is not about one tool.

## Coding-agent composition

`apps/elmee` builds a fresh graph from configuration:

```text
selected model provider
    + built-in coding ToolSet
    + optional web search
    + project hooks
    + workspace service with snapshots
    + project memory
    + per-directory project directives
    + standing goals
    + optional web fetch
    + JSONL or memory session
    + optional shared token ledger
    + snapshots
    + MCP discovered last
    + one global tool allowlist
    -> Agent
    -> host Driver
    -> TUI | one-shot | ACP
```

Application policy is resolved before runtime:

- the selected model and its context window determine provider and compaction
  defaults;
- host tool schemas are offered only when the selected frontend can answer
  them and the build has the `agents` feature; what a background agent needs said about it rides in those schemas'
  own descriptions, so nothing is appended to the prompt for them;
- MCP construction is last because live discovery may start processes;
- `allow_tools` is applied after every namespace exists and rejects unknown
  names.

The frontend is selected outside the registry because it owns process lifetime
rather than agent semantics. One-shot, ACP and TUI runs use the same `Driver`
and the same inner turn behavior.

## Ownership and concurrency

An agent belongs to one local async task. Mutable services use `Rc`/`RefCell`;
model requests and handler state are serialized by that owner. Parallel work is
introduced at explicit boundaries:

- independent agents run in independent local tasks or processes;
- a handler may overlap authorized read preparation;
- `Pending` barriers may be produced on sendable futures;
- external commands and MCP servers are child processes.

The application owns the task tree. Dropping process adapters initiates child
shutdown and reap.

## Trust and authority

There are two kinds of code here, and the difference is not enforced by elmee:

1. **Core, linked plugins and anything a project declares** run as the account
   that started elmee, with its full authority. Registry contracts constrain
   architecture, not what native code or a declared hook may do.
2. **Remote and model-controlled data** — web pages, MCP descriptions, tool
   output, peer messages — crosses a process or protocol boundary and is marked
   untrusted when fed back to the model. That is provenance, not restriction.

What bounds a running agent is the operating-system account, container or VM it
was started in. See [SECURITY.md](../SECURITY.md).

## Architectural diagnostics

The binary exposes an inert explanation path:

```sh
cargo run -p elmee -- --explain-plugins
```

It resolves a memory session and the declared MCP namespace without opening
servers or a frontend.

Any behavioral difference between two runs should be explainable from these
manifests and the configuration each was given. If it is not, the architecture
contains a hidden dependency.
