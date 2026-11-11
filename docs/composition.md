# Plugin composition

## Scope of the framework

The plugin framework solves one problem: assemble a fixed set of
Rust capabilities into an agent or application runtime. It provides atomic
declaration, typed services, checked namespaces and deterministic middleware.

It is deliberately not:

- a runtime native-library loader;
- a package manager;
- a global mutable service locator;
- a general event bus;
- a promise that arbitrary plugin constructors are free of side effects.

The complete public model has three roles. A feature should combine these roles
instead of adding a fourth feature-specific slot.

## The `Plugin` contract

```rust,no_run
use elmee_core::{Plugin, Registrar, Result};

struct MyPlugin;

impl Plugin for MyPlugin {
    fn id(&self) -> &'static str {
        "my-plugin"
    }

    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_handler(self)
    }
}
```

`id` is a stable owner identity within one registry. `declare` consumes the
plugin and writes to an isolated `Registrar`. It cannot inspect the registry,
ask which neighbors have already been installed or fetch services.

Declaration may:

- provide typed services;
- provide handlers and middleware;
- reject invalid local configuration.

Declaration must not:

- open a connection;
- start a process or task;
- mutate application files;
- change its contribution based on a previously installed neighbor;
- retain a reference to the registrar.

Rust cannot prove that arbitrary `declare` code is pure. This is a reviewed
contract, while registry atomicity is mechanically enforced. Construction and
external discovery remain visibly separate from declaration.

## Atomic installation

`Registry::install` follows this sequence:

1. reject a duplicate plugin ID;
2. execute `declare` against an isolated registrar;
3. snapshot and validate local handler schemas and commands;
4. compare the complete contribution with existing global names;
5. merge all roles as one unit;
6. sort every order-sensitive collection by stable semantic keys.

If any step fails, none of the registrar's services or runtime roles become
visible. This atomicity covers registry contributions, not external effects
performed by arbitrary Rust code outside the registrar.

## Services and dependencies

Services are indexed by Rust `TypeId` and returned as typed `Rc<T>` handles.
After resolution the application and the kernel read them with
`Services::one`, which rejects an ambiguous provider, or `Services::all`.

A handler or middleware is never handed the registry, and a plugin declares no
requirements. What it needs of another plugin it receives from the composition
root, as a constructor argument. This is an architectural boundary, not a
security mechanism. It prevents incidental reach into unrelated registry state
and makes every dependency visible where the graph is assembled.

Prefer a narrow capability trait:

```rust,no_run
use std::rc::Rc;
use elmee_core::{Plugin, Registrar, Result};

trait Clock {
    fn now(&self) -> u64;
}

// `Handler` implementation omitted.
struct Consumer {
    clock: Rc<dyn Clock>,
}

impl Plugin for Consumer {
    fn id(&self) -> &'static str { "consumer" }

    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_handler(self)
    }
}
```

The consumer depends on `Clock`, not `SystemClockPlugin`. In tests, an in-memory
implementation can satisfy the same contract.

If the combination of two capabilities creates new semantics, represent it as
an explicit integration plugin rather than having either neighbor silently
detect the other.

## Role 1: services

A service is a typed capability used by the kernel or by the application
after resolution. Examples include:

- `ModelService`;
- `Workspace`;
- `TokenLedger`;
- `Snapshots`.

Services should contain a cohesive capability, not expose a plugin's entire
internal state. Singular capabilities the kernel consumes are checked by
kernel validation. Multi-provider capabilities use `Services::all`
deliberately.

The agent kernel adds its own cardinality checks before resolution:

- exactly one `ModelService`;
- at most one `Workspace`;
- at most one `TokenLedger`;
- at most one `SessionSeed`.

A registry resolved directly with `resolve`, outside an agent, has no
agent-specific assumptions.

## Role 2: handlers

A handler owns a set of model-tool definitions and may also own runtime slash
commands. The registry snapshots both namespaces at installation so dispatch
does not depend on later dynamic answers from `definitions()` or `commands()`.

The handler contract covers the lifecycle of one owned operation:

- `definitions` declares model schemas;
- `execute` returns a complete `ToolOutcome`;
- `prepare` may overlap reversible work for an authorized batch;
- `commands` and `command` expose plugin-owned runtime commands;
- `completion_claim` and `accept_completion` support a plugin-owned objective;
- `report` exposes concise diagnostics;

Most handlers implement only the methods relevant to their operation. Do not
add another default method for a feature unless the information belongs to the
lifecycle of every handler-shaped operation. Otherwise use a service,
middleware or a plugin-private helper.

### One outcome, one owner

`ToolOutcome` carries output, status, optional auxiliary completion, evidence
and images. The loop does not call a series of post-execution
callbacks that could disagree after handler state changes.

Tool ownership is exact: after resolution, one visible tool name maps to one
handler. Unknown or duplicate ownership is a configuration error, never
first-match behavior.

### Global filtering

`Registry::allow_tools` records an application-wide allowlist. Resolution
validates it against the complete namespace, including kernel tools and MCP,
then removes disallowed schemas and ownership entries. A typo is rejected as
an unknown tool instead of silently producing an agent with no tools.

The filter may be recorded at any point before resolution, which is when it is
applied to the complete namespace.

Kernel tools are held apart from handler schemas: they belong to the loop,
which no handler can replace or shadow.

## Role 3: middleware

Middleware expresses bounded global policy at stable semantic events. One
plugin may contribute one middleware, so its identity and owner cannot drift.

Events currently cover:

| Event | Stable point |
| --- | --- |
| `UserPrompt` | Before the user message enters the model context. |
| `BeforeTool` | Before a tool call is prepared or executed. |
| `AfterTool` | After an executed handler returns. |
| `BeforeFinish` | Before a no-tool answer may finish. |
| `TurnFinished` | After finish gates, while continuation is still possible. |
| `BeforeCompaction` | Before the summarization request, with the messages it will replace. |
| `Compacting` | After a summary exists, before it is committed. |

Effects are deliberately bounded:

- `AddContext` augments a prompt, a tool result or the compaction request;
- `Carry` keeps a plugin's own facts verbatim after a summary;
- `Continue` schedules another loop turn;
- `ReplaceText` transforms a compaction summary;
- `RecordCommandEvidence` records a successful completion check.

Middleware is ordered by semantic phase, then plugin ID:

```text
Context -> Policy -> Transform -> Continuation
```

There is no numeric priority space and no general before/after dependency
graph. If two modifiers require a non-obvious integration, create one explicit
integration component or refine the semantic phase only after multiple real
consumers demonstrate the need.

## Resolution and freeze

`resolve` is idempotent for an already resolved ordinary registry. Agent
construction uses a stricter `resolve_agent` path so kernel cardinalities cannot
be skipped accidentally.

Resolution performs:

1. every kernel cardinality check;
2. complete tool-allowlist validation and filtering;
3. transition to the resolved state.

After resolution, plugins and tool filters cannot be installed. Services may be
read, middleware dispatched and handlers executed.

## Stable manifests

`Registry::manifest` emits a deterministic semantic description of:

- declared or resolved state;
- installed plugin IDs;
- typed service owners;
- the kernel's cardinality requirements;
- middleware order and phase;
- tools, commands and owners, with loop-answered kernel tools attributed to
  `core-agent` — an id no plugin may claim.

`Agent::composition` exposes the agent graph. Manifests use display type names
for diagnostics while runtime identity remains `TypeId`.

Manifest output is an architecture tool, not only a user feature. Stock tests
can compare graph permutations and explain an unexpected feature by ownership.

## Laws worth testing

Every change to the framework or a stock composition should preserve:

1. **Permutation:** reordering independent declarations yields the same
   manifest.
2. **Atomicity:** a rejected declaration leaves no contribution behind.
3. **Uniqueness:** duplicate plugin, tool and command names fail.
4. **Cardinality:** a missing or ambiguous service the kernel requires fails
   before runtime.
5. **Filtering:** all namespaces, including dynamic ones, obey one allowlist.
6. **Reload atomicity:** any candidate failure preserves every live handler.
7. **Inert explanation:** manifest commands start no session or MCP process.

These laws are more durable than a preferred count of traits or crates. They
describe observable behavior that future extensions must not weaken.

## Multi-registry applications

An application may use multiple graphs: the ACP frontend is given one agent
per session an editor opens. A shared capability between
graphs cannot be inferred by either registry; the composition root constructs
and passes the same typed value explicitly.

This is not hidden dependency injection. It is application wiring and must be
visible in diagnostics. If cross-graph bridges become numerous or dynamic,
that is evidence for a small application-level manifest abstraction — not a
reason to turn the core registry into a process-wide container preemptively.

## The laws, and when the contract may change

Elmee adopts the useful, enforceable part of the
[Cordis model](https://github.com/cordiverse/paper/blob/main/paper.pdf), without
turning a statically linked Rust application into a dynamic component loader:

- **Spatial composition (coeffects).** A plugin declares typed roles through a
  write-only registrar. Resolution is independent of source order, and what
  one plugin needs of another it is handed by the composition root, as a
  constructor argument.
- **Temporal ownership (effects).** Declaration is inert. The resolved graph
  owns handlers and middleware, and the application owns all running tasks as
  one structured lifetime. Dropping an MCP/process adapter owns and stops its
  child handles.
- **An honest system boundary.** Network requests, delivered messages and file
  writes cannot be rolled back merely because an in-process object is dropped.
  Those effects are made durable instead: records, stable event IDs and
  idempotent recovery after the fact.
- **Static quiescence.** A native in-process graph resolves once and then
  freezes. Truly
  dynamic, untrusted or model-generated extensions belong behind an MCP,
  process or future Wasm boundary where unload and authority are real.

These are the freeze rules for the kernel: a new universal role or lifecycle
phase needs at least two independent production consumers; core contains only
provider/tool-loop invariants and small dependency-free boundary primitives;
independent plugins must commute; and behavior that integrates two capabilities
must be installed explicitly rather than discovered from a neighbour at runtime.

An architectural review justifies changing this contract only by showing a
violated law, an observed production failure, or two independent consumers
that the present API cannot express. Crate count, method count and hypothetical
future flexibility are diagnostics, not change requests by themselves. This
gate is what lets the contract stabilize instead of oscillating between every
locally plausible decomposition.

A logical plugin is not automatically a Cargo crate. A crate is justified by
an independent dependency, authority, reuse or distribution boundary. Closely
coupled roles may be packaged together—as a tool family and the workspace
it reads and writes through are in `tools`—while selectable adapters with distinct
dependencies remain separate. This follows Cordis's own advice to bundle fine-grained components
when that reduces cognitive overhead; minimizing crate count is not itself an
architectural goal.
