# Philosophy

## The objective

Elmee is designed around a deliberately strong constraint:

> The agent kernel should become boring and stable; new product behavior should
> normally be added as a small, autonomous plugin without editing the kernel.

This is not modularity for its own sake. Agentic codebases tend to accumulate
features across the main loop: a new callback, a provider-specific branch, a
frontend flag, then a second callback to repair the interaction between them.
That shape is especially hostile to AI-assisted development because every
extension requires a large context and invites another locally plausible but
globally inconsistent special case.

Elmee instead tries to make the correct extension unit the easiest one to add.
The architecture should mechanically reject common forms of accidental
coupling and make the resolved behavior explainable before anything runs.

## Minimalism means few concepts, not one file

Minimalism is evaluated by the number of concepts a contributor must hold and
the amount of code a feature can accidentally affect. It is not evaluated by
minimizing directories or Cargo packages.

A small crate is justified when it creates a useful boundary in at least one
of these dimensions:

- a distinct dependency or compile-time cost;
- a distinct authority or security boundary;
- independent reuse or replacement;
- independent distribution or product selection.

Conversely, every noun does not need its own crate. Closely coupled roles can
live together when splitting them would create an integration protocol larger
than the implementation. A tool family and the workspace it reads and writes
through are one example: what was read and what is about to be overwritten are
shared state.

The project is therefore best described as a **statically composed modular
monolith**, not an ecosystem of dynamically loaded native plugins.

## What may enter core

Core admits only behavior required to define one provider-neutral agent turn:

- provider/tool conversation state;
- message and session semantics;
- the host interaction boundary;
- context compaction;
- loop-owned plan state;
- evidence bound to the current workspace state;
- the three-role composition contract;
- tiny dependency-free boundary primitives.

The test is not whether a feature is important. Project memory and standing
objectives are important, but a valid agent loop can exist without them, so
they are plugins.

There are two intentional kernel tools:

- `plan` manipulates the loop's live plan and emits plan events;
- `ask_user` performs the loop's human interaction round trip.

Their schemas live beside their implementation in `core/agent`. A schema must
never be declared in a plugin while execution is secretly intercepted by core.

## What must not enter core

Core must not acquire knowledge of:

- model vendors, authentication or wire protocols;
- HTTP clients or async runtimes;
- shell programs, test frameworks or package managers;
- terminal, JSONL, ACP or messaging frontends;
- persistence formats and filesystem layout policy;
- MCP, web search or scheduling;
- feature-specific plugin identities or tool names.

If core needs a fact about one of these worlds, the component that understands
that world must classify it and pass a neutral value across an existing
boundary. For example, the process plugin classifies a successful command into
a verification level; core only binds that observation to the workspace
fingerprint.

## Composition is the hard part

Moving behavior out of core does not remove complexity. It moves complexity
into the algebra of composing components. A plugin architecture has failed if:

- registration order changes behavior;
- a plugin silently discovers a neighbor and changes mode;
- declaration starts I/O before the whole graph is valid;
- two individually valid plugins collide only at runtime;
- a missing optional service silently activates an undocumented fallback;
- long-lived tasks escape application ownership;
- the composition cannot explain why a tool or policy is active.

Elmee's registry exists to prevent those failures, not to provide a general
dependency-injection framework.

## The composition laws

### Declaration-order independence

Independent plugins commute. Reordering their `plugin(...)` or `install(...)`
calls must produce the same resolved manifest and behavior. Semantic order is
derived from stable metadata, never source position.

### Explicit dependency

A runtime role is never handed the registry and cannot enumerate unrelated
global state. What one plugin needs of another it receives from the
composition root, as a constructor argument.

### Atomic contribution

A plugin's registry contribution appears as a unit or not at all. Duplicate
plugin IDs, tools and commands fail before a partial contribution is merged.

### Frozen native graph

The in-process graph resolves once. Handler schemas and command ownership are
snapshotted, middleware order is fixed, and runtime code cannot add arbitrary
neighbors.

### Observable global behavior

Anything that modifies the whole loop is visible as middleware. The resolved
manifest names ownership and ordering.

### Structured lifetime

The application owns every started task. Process and MCP adapters own their children and stop them on drop. No plugin is
allowed to hide an orphan background lifetime.

### Honest effects

External effects are not transactional merely because an in-process object is,
and nothing here pretends to prevent one. Durable records, stable IDs,
idempotency and working-tree snapshots handle failure after the fact, which is
the only honest place to handle it.

## Cordis influence

Elmee borrows two useful ideas from the
[Cordis paper](https://github.com/cordiverse/paper/blob/main/paper.pdf):

- **spatial coeffects** — a component's environment is explicit: what it needs
  is passed to its constructor by the composition root, never discovered;
- **temporal effects** — resources have explicit ownership and activation
  phases rather than appearing during arbitrary registration code.

Elmee intentionally does not reproduce a full dynamic component runtime. Its
native graph is known at compile time and resolved once per application.
Dynamic or model-generated code has a different trust and lifecycle problem;
it belongs behind MCP, process, or a future Wasm boundary where unloading and
authority restriction are real.

The resulting lifecycle is:

```text
construct/discover -> declare -> validate/resolve -> freeze -> run -> stop
```

`Plugin::declare` is contractually inert, but Rust cannot prove arbitrary code
pure. Constructors and discovery must therefore remain visibly separate. Live
MCP discovery is performed as the last construction step; explain-only paths
use an inert declaration and start nothing.

## Local-task agents are intentional

`Agent`, `Registry` and in-process services use `Rc` and `RefCell`. The shipped
applications run them on Tokio's current-thread runtime and `LocalSet`.

This is a conscious performance and complexity choice:

- one agent serializes its own model and tool state;
- the hot loop avoids synchronization on every service access;
- provider adapters may hold local borrowed state;
- concurrency occurs between agents, processes and I/O operations, not by
  sharing one mutable agent across worker threads.

`Pending: Send` means an individual background future may be computed on
another thread. It does not make the containing `Agent` sendable. A server
should create one agent per concurrent conversation or request, keep it on one
local task and communicate with it through channels.

## The stability gate

There is no mathematically unique optimal architecture. Reviews optimize for
different local objectives — fewer crates, fewer trait methods, stricter
isolation, less duplication — and every refactor exposes a different tradeoff.
Chasing a perfect review score would keep the architecture permanently fluid.

Core and the universal plugin contract should change only when at least one of
these is true:

1. a documented composition law is violated;
2. an observed production failure cannot be fixed behind the current boundary;
3. two unrelated production consumers need a capability the existing three
   roles cannot express.

Crate count, method count and hypothetical flexibility are useful diagnostics,
not sufficient change requests. A new universal role or lifecycle phase needs
two independent production consumers. Feature-specific convenience belongs in
a plugin or application helper.

## Non-goals

Elmee does not aim to be:

- a runtime marketplace for arbitrary native libraries;
- a distributed service container;
- a general-purpose dependency solver;
- a single shared multithreaded agent object;
- a framework where every implementation detail is replaceable;
- a promise that anything running inside the process is contained.

The goal is narrower: a small, stable agent kernel and predictable composition
of capabilities. What bounds a running agent is the operating-system account it
was started in, not anything the kernel does.

