# Elmee architecture handbook

This directory is the detailed design reference for Elmee. The root
[README](../README.md) is the product overview; the user guides beside these
files cover [running it](usage.md), [models and configuration](models.md),
[skills, MCP and hooks](customization.md) and [embedding](embedding.md);
[AGENTS.md](../AGENTS.md) is the short form of the rules for changing the code;
[SECURITY.md](../SECURITY.md) says what bounds a running agent — which is the
operating system, not anything in here. These documents explain why the repository is shaped as it
is, which contracts extensions must preserve, and how the shipped product is
assembled from the kernel.

## Reading paths

For a first architectural pass, read:

1. [Philosophy](philosophy.md) — the problem Elmee is designed to solve and the
   rules used to keep its kernel small.
2. [Architecture](architecture.md) — layers, ownership, control flow and the
   coding-agent composition.
3. [Composition](composition.md) — the plugin roles, declaration lifecycle,
   deterministic resolution and runtime contracts.
4. [Core](core.md) — the kernel modules and the exact responsibilities that
   are allowed to live there.

When changing the project, continue with:

- [Components](components.md) — every shipped plugin, adapter and application,
  including why each boundary exists.
- [Runtime and state](runtime-and-state.md) — sessions, evidence, process
  ownership, supervision and recovery.
- [Extending Elmee](extending.md) — decision trees, implementation recipes and
  the required tests for a new capability.

## The architecture in one page

Elmee is a statically composed modular monolith:

```text
                apps/elmee
                    |
        reusable plugins and adapters
                    |
          provider-neutral core
```

The dependency rule is strict:

```text
apps -> plugins -> core
```

- **Core** owns one agent turn and the invariants required to finish it. It
  knows no concrete model vendor, HTTP protocol, terminal UI or persistence
  format.
- **Plugins** provide model adapters, tools, policies and durable stores.
  Trusted native components are linked
  statically; untrusted or dynamic components cross process or MCP boundaries.
- **Apps** choose a product composition and own the process lifetime. They are
  allowed to know which plugins exist; core is not.

Every in-process plugin contributes one or more of three roles:

| Role | Meaning |
| --- | --- |
| Service | A typed capability read by the kernel or the application. |
| Handler | Model-facing tools and plugin-owned runtime commands. |
| Middleware | Bounded policy at stable points in the agent loop. |

A plugin declares into a write-only transaction. The registry validates names
and the kernel's service cardinalities, then freezes the graph. What one
plugin needs of another it is handed by the composition root, as a
constructor argument. Independent plugin declarations commute: rearranging their
installation calls must not alter the resolved behavior.

The kernel includes two model tools, `plan` and `ask_user`, because only the
turn loop can access their private state. Standing goals, project memory and
every other product feature remain plugins.

## Vocabulary

**Agent**
: One local-task instance of the model/tool loop and its conversation state.

**Application / composition root**
: A binary under `apps/` that selects plugins, resolves a graph and owns the
  resulting runtime.

**Capability**
: A narrow typed service or operation offered by a component. Dependencies are
  expressed in terms of capabilities, not concrete neighboring plugins.

**Kernel tool**
: A model tool implemented by the loop because it requires loop-private state.
  Kernel tools use the same checked and filtered namespace as plugin tools.

**Plugin**
: A logical contribution implementing `Plugin`; it is not synonymous with a
  Cargo crate. One crate may contain several closely related plugins, while a
  mechanism-only crate may contain none.

**Adapter**
: A component that translates an external protocol or presentation surface
  into an Elmee capability, for example a model provider or the TUI.

**Registry**
: The declarative composition mechanism. It is not a dynamic package manager
  or an arbitrary global service locator.

**Resolved graph**
: The validated and frozen set of services, handlers and middleware for one
  agent.

**Host**
: Process-level machinery outside the kernel: frontends, human interaction,
  background agents and everything that surrounds a turn.

## Source of truth

Documentation describes the intended contracts, while executable tests remain
the final guard against drift. In particular, composition tests cover duplicate
names, cardinality, source-order independence and filtered namespaces.
If code and this
handbook disagree, treat the mismatch as a bug and correct both in the same
change.
