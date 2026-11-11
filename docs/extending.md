# Extending Elmee

This guide is for adding behavior without widening core or creating hidden
composition dependencies.

## First choose the boundary

Ask these questions in order.

### Does it define every valid agent turn?

If no, it does not belong in core. Most features — memory, search, scheduling,
channels, goals, provider adapters — are optional product capabilities.

If yes, verify that the behavior cannot be expressed through an existing
handler, middleware, service or `Interactor` operation. A new core hook
requires two independent production consumers and a named loop invariant.

### Is it reusable behavior or product selection?

- Reusable behavior belongs under `plugins/`.
- Selection, credentials, topology and process lifetime belong under `apps/`.
- A frontend adapter belongs in `plugins/`, but the application selects it
  directly because presentation owns lifetime rather than loop semantics.

### Is it in-process code?

- Small, reviewed, shipped Rust capability: native plugin.
- User/model supplied executable: process or MCP boundary.
- Remote protocol: adapter whose output is marked untrusted.
- Truly unloadable generated component: process/MCP or future Wasm, never a
  dynamically loaded native library in the core process.

### Does it need a new crate?

Create a crate when there is an independent dependency, authority, reuse or
distribution boundary. Otherwise add a module or another logical plugin to an
existing cohesive crate.

Do not split by noun alone. Do not merge merely to reduce crate count.

## Pick the smallest role

| Need | Role |
| --- | --- |
| Share typed data or an implementation | Service |
| Offer a model tool or plugin-owned command | Handler |
| Observe or modify a stable loop event | Middleware |
| Present/drive the whole process | Frontend adapter over the host, outside registry |

A logical plugin may provide several roles when they are one feature. Memory,
for example, provides a handler and middleware. Avoid separate plugin IDs for
roles that cannot be selected independently.

## Add a model tool

1. Define a handler that owns its schemas and execution.
2. Give every schema a stable unique function name.
3. Parse and validate arguments inside the handler.
4. Return one `ToolOutcome` containing all post-execution facts.
5. Mark remote or file-derived content untrusted where it enters model context.
6. Bound bytes, time, child output and concurrency.
7. Install it through a `Plugin` contribution.

Minimal shape:

```rust,no_run
use elmee_core::{Handler, Plugin, PluginFuture, Registrar, Result, ToolOutcome, message::ToolCall};

struct Echo {
    definitions: Vec<serde_json::Value>,
}

impl Handler for Echo {
    fn definitions(&self) -> &[serde_json::Value] {
        &self.definitions
    }

    fn execute<'a>(&'a mut self, call: &'a ToolCall) -> PluginFuture<'a, ToolOutcome> {
        Box::pin(async move { ToolOutcome::new(call.function.arguments.clone(), true) })
    }
}

impl Plugin for Echo {
    fn id(&self) -> &'static str { "echo" }

    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_handler(self)
    }
}
```

Do not start an effect in `prepare`. It may overlap calls and must remain
reversible. Execution is the effect boundary.

If the tool needs a second model pass over its data, return a
`CompletionRequest`; core accounts for it and invokes the provider without tool
schemas. Do not reach into `ModelService` from the handler unless the feature
really needs a lower-level model capability with a distinct contract.

## Add a service dependency

A plugin that needs another plugin's capability takes it as a constructor
argument, and the composition root passes it in:

```rust,no_run
use std::rc::Rc;
use elmee_core::{Handler, Registrar, Result};

trait Notes {
    fn lookup(&self, key: &str) -> Option<String>;
}

struct NoteTool {
    notes: Rc<dyn Notes>,
    // definitions omitted
}

impl Handler for NoteTool {
    // required methods omitted
}

fn declare(registrar: &mut Registrar, notes: Rc<dyn Notes>) -> Result<()> {
    registrar.provide_handler(NoteTool { notes })
}
```

Depend on the narrow trait, not the concrete provider. Publish a service with
`provide_service` only when the kernel or the application reads it after
resolution; do not introduce one solely to avoid a constructor parameter.

## Add middleware

Use middleware only for behavior at a stable global point in the turn. Select
the semantic phase that describes its effect:

- `Context` adds trusted application context;
- `Policy` can reject an operation or finish;
- `Transform` changes a bounded representation such as compaction text;
- `Continuation` asks the loop to keep working.

Implement only documented event/effect combinations. Middleware should be
deterministic for the same event and owned state, and must not depend on source
installation order.

If two existing plugins interact to create a new policy, prefer a third
explicit integration plugin. Do not quietly change behavior when a neighbor
happens to be present.

## Add a provider adapter

Implement core's neutral `Provider` in `plugins/provider` when the protocol is
a supported first-class model wire format. The adapter must:

- translate all neutral message forms it claims to support;
- stream text/reasoning without duplicating visible output;
- recognize an explicit protocol terminal event;
- treat EOF before that terminal event as failure;
- surface in-stream error objects as errors;
- retry only when no visible output has been emitted;
- report input/output tokens consistently;
- bound response/frame size and time;
- keep credentials and refresh policy inside the adapter.

A new endpoint using an existing wire format belongs in model configuration,
not a new Rust adapter.

## Add an application

Create `apps/<name>` when the product has a meaningfully different composition
or lifetime. Keep the binary entry point thin:

1. parse application-specific configuration;
2. construct potentially effectful adapters last;
3. install reusable plugins;
4. apply global namespace policy;
5. resolve and optionally print the manifest;
6. activate the frontend under one owned runtime.

Do not add another `profiles/` layer. The application is the single composition
root. Factor repeated mechanism into a plugin or a narrow helper only when two
apps need exactly the same policy; do not hide product differences behind an
oversized universal recipe.

## Explain-only behavior

Every application should provide a way to resolve its composition without
external side effects. In that path:

- use memory sessions;
- use declared/inert MCP descriptors;
- do not create background tasks;
- do not open frontend input;
- print every graph and any explicit cross-graph shared capability.

Add a tripwire test that would leave a marker or fail if discovery starts an
external process. An explanation command is an architectural query and must be
safe to run in CI.

## Required tests

### For every plugin

- declaration succeeds in the minimal valid graph;
- duplicate IDs and owned names are rejected;
- a missing or ambiguous service the kernel requires fails at resolution;
- failures return bounded useful errors.

### For handlers

- schemas parse and have unique names;
- malformed arguments do not panic;
- global filtering includes the tool and rejects a typo;
- remote output is marked untrusted.

### For middleware

- correct semantic phase;
- unrelated events are inert;
- install permutation does not change the resolved order;
- no hidden dependency on another plugin's presence.

### For stock compositions

- normalized manifest is stable across installation permutations;
- every required service has the intended cardinality;
- tool allowlists cover kernel, built-in, web and MCP names;
- explain mode is inert;
- removing an optional plugin changes only its declared capability and explicit
  integrations.

## Documentation checklist

Every plugin or adapter crate begins with crate-level documentation that says:

1. what capability it provides;
2. why it is outside core;
3. which roles it contributes;
4. what authority and external effects it has;
5. how failures and shutdown are handled;
6. what application must configure.

Update [Components](components.md) for a new shipped crate and
[Architecture](architecture.md) for a new stock composition. Update
[SECURITY.md](../SECURITY.md) for any user-visible authority change.

## Architecture review gate

Before expanding the universal framework, include in the change description:

- the violated composition law or observed production failure;
- two unrelated production consumers if proposing a new role, phase or
  lifecycle callback;
- why an existing role or plugin-private abstraction is insufficient;
- how resolved behavior remains deterministic and explainable;
- tests that fail under the old contract and pass under the new one;
- the expected core and total production LOC impact.

This gate is intentionally harder than adding a plugin. The architecture works
only if feature growth flows outward while the kernel converges toward
stability.

