# Components

This catalog describes logical responsibility, composition role and boundary
rationale. A crate may provide multiple plugins, an adapter, or only a reusable
mechanism. That distinction is intentional: **plugin is a runtime composition
concept; crate is a source and dependency boundary**.

## Applications

### `apps/elmee`

The installable coding agent and default workspace member.

It owns:

- CLI command dispatch and product configuration: the `Config` struct,
  argument and environment parsing and the help text (`src/config.rs`);
- provider/model selection and compaction defaults;
- selection and ordering-independent installation of coding capabilities;
- the final global tool allowlist;
- choice of TUI, one-shot or ACP frontend;
- the current-thread Tokio runtime and local task set.

It does not implement reusable agent behavior. If another application could use
a feature, that feature belongs in a plugin or mechanism crate.

The terminal interface is the cargo feature `tui`; subagents, the project
mesh and the `elmee notify` subcommand (`src/notify.rs`) are the feature
`agents`, which turns on the host's feature of the same name. Both are on by
default. `--no-default-features` builds the embeddable agent alone: ACP and
one-shot runs, one agent per session. CI lints that configuration too.

### PDF skill

`.agents/skills/pdf/SKILL.md` teaches PDF inspection, text extraction, search
and rendering through system tools (Poppler), with optional Python and OCR
utilities. It is discovered through the ordinary project skill mechanism,
not linked into the agent or served over a dedicated MCP protocol. Install
it in a user skill directory to use it outside this checkout; see the README.

## Model and knowledge adapters

### `plugins/provider`

**Role:** model service provider.

Translates neutral core messages and streaming turns for:

- OpenAI-compatible Chat Completions;
- OpenAI Responses, including Codex subscription authentication;
- Anthropic Messages.

It also owns model registry files, aliases, endpoint declarations, context
window metadata, credentials, refresh serialization, retry boundaries and
protocol terminal-event validation. Endpoint catalogues are asked concurrently,
each bounded to three seconds, so a silent endpoint costs only its own entries.

This is a separate crate because HTTP/auth dependencies and vendor protocols
must evolve without entering core.

`SessionContext` is a provider-owned typed service for conversation routing and
cache identity. The composition root sets it from the opened store, and the host
updates it when a terminal changes conversation. Request observations are neutral `RequestMetrics`; frontends
receive them through `ModelEnd` rather than inspecting vendor responses.

### `plugins/project`

**Roles:** handler and middleware, plus discovery mechanisms.

Owns what a repository teaches an agent:

- `AGENTS.md`/`CLAUDE.md` context assembly;
- skill discovery and on-demand loading;
- project roles;
- lifecycle hooks;
- durable project memory.

`Hooks` is loop middleware, and the one thing that may still refuse a tool call
— a repository's own say over its work, not a policy of the harness.
`MemoryPlugin` combines a model handler with prompt middleware because storage
and its injection policy form one feature.

### `plugins/search`

**Role:** handler.

Normalizes web-search results from explicitly configured SearXNG, Brave or
Tavily backends. Search-provider selection is declared rather than inferred by
probing. Other search mechanisms can enter through MCP.

### `plugins/web`

**Role:** handler.

Provides bounded `web_fetch` retrieval with DNS resolution checks, private
address rejection, per-hop redirect validation, address pinning, time and byte
limits, HTML extraction and untrusted-content fencing. By default it returns
bounded source text with byte offsets for continuation, without a model call.
Only explicit `summarize=true` with a question asks for an additional billed
model completion; that completion receives no tool schemas.

The crate is separate because SSRF safety and HTTP behavior form a substantial
network boundary independent of general search.

## Local work and extension mechanisms

### `plugins/tools`

**Roles:** handler and workspace service.

`ToolSet` exposes the stock local coding namespace: bounded text reading,
`view_image` for PNG/JPEG/GIF/WebP content (up to 5 MiB, loaded by signature), one
grammar-constrained `apply_patch` for every change to a file, a shell family
(`exec`, `write_run`, `stop_run`) over durable background runs. Listing and searching are commands rather than typed tools,
because a three-field schema is strictly less than `rg`. `write_run` both feeds
a run and collects what it has written since the last look, so following one
needs no second verb. A relative path is resolved against the working
directory and an absolute one is used as written; an edit built on a stale
read is refused.

`WorkspacePlugin` independently supplies workspace identity, image attachments
and fingerprints, optionally backed by explicit snapshots. An application can
therefore have a workspace without inheriting the full coding tool set.

This is an integration-heavy crate by design: the stock coding toolkit combines
process capture, project state paths, snapshots and run tracking. Its public boundary is
the cohesive model-tool set, not isolation from every mechanism it uses.

### `plugins/atomic`

**Kind:** reusable mechanism, not a model-facing plugin.

Crash-safe whole-file replacement: an unguessable `create_new` temporary, an
fsync of both the file and its parent directory — skipped for a record that
is only true while its writer lives — and an optional mode. It knows
a path and bytes and nothing else, so no persistence format and no filesystem
layout policy enters the primitive — which is what lets seven crates share
it without any of them learning about a neighbour.

It exists because the same replacement was written fourteen times, only three of
which fsynced the directory, so on the rest the rename itself was lost on power
failure; several used a predictable `.tmp` sibling that a planted symlink would
follow. It is a crate rather than a core primitive so that it needs no stability
gate, and its own dependency cost stays at zero.

### `plugins/process`

**Kind:** reusable mechanism, not a model-facing plugin.

Centralizes bounded child-process capture, stdin feeding, timeouts and
controlled environment construction. It also classifies known successful
verification commands into neutral evidence levels.

Higher-level components decide which program is meaningful and expose it as a
tool, hook, MCP server or custom command. This avoids four subtly different
subprocess implementations.

### `plugins/command`

**Kind:** reusable application/plugin mechanism.

Presents the skills of a project and its user as slash commands and expands
a prompt template: arguments, positions and `` !`command` `` blocks. Frontends
supply their own built-ins; a skill never adds a case to the agent loop. The
host `Driver` expands a prompt that names a skill, so `/NAME` works in every
frontend.

### `plugins/mcp`

**Role:** handler.

Discovers external MCP tools over stdio or Streamable HTTP, handles pagination,
bounded frames, timeouts and process ownership, and translates remote schemas
into the global tool namespace. A third transport is a channel the composition
root hands over for a server the embedding application itself is: a function
from an MCP request to its result, spoken to in the stateless revision, so the
plugin never learns what is on the other end. A project's declarations are read after the
user's and cannot shadow a server name the user has already given.

Servers are connected once, when the agent is composed; a server that cannot
start degrades to a warning in the report rather than failing the session.

### `plugins/goal`

**Roles:** handler and continuation middleware.

Owns standing-objective state, slash commands, the `goal_done` completion
claim, continuation prompts and turn limits. The kernel can test and commit a
generic completion claim without knowing this plugin or its tool name.

Goal is intentionally not core: a valid turn does not require a standing
objective.

## Persistence and observation

### `plugins/session`

**Role:** session seed/service implementation.

Persists the core conversation tree as append-only JSONL with an exclusive
writer lock, whole-record writes, schema checks and torn-final-line recovery.
A session is one file: a move between branches is a line of the same journal.

It also names a new transcript, lists the transcripts in a directory newest
first, and summarizes a past one by the first thing asked of it. That last
reads only the lines that can hold a prompt, but through the same reader, so a
session picker needs no second parser and cannot silently lose torn
transcripts when the format moves.

Core owns tree semantics; this crate owns the filesystem format and is its only
parser.

### `plugins/ledger`

**Role:** token-ledger service.

Provides append-only billed-token accounting shared by a root agent and child
processes. Core enforces the configured budget; this adapter supplies one total
without a daemon or database.

### `plugins/snapshot`

**Role:** snapshot service.

Records complete working-tree states in an external Git directory. State-based
snapshots see edits made by shell commands, formatters and child agents that
tool interception would miss. They never touch the project's own index, stash
or history.

Snapshotting surrounds an application-driven turn, outside the provider/tool
loop: the host driver asks this service directly.

### `plugins/paths`

**Kind:** reusable mechanism, not a model-facing plugin.

Where a project's state lives outside its checkout: which directory is the
project (`project_root`), the per-directory key that sessions and checkpoints
are filed under (`workspace_hash`), and where its sessions
(`session_directory`) and its inboxes and runs (`project_state_directory`) are
kept. It depends on core alone, so `mesh`,
`snapshot`, `tools`, `host` and `apps/elmee` share one path policy without
inheriting a command line.

## Frontends and process host

### `plugins/host`

**Kind:** application runtime mechanism.

Bridges an assembled agent to process-level interaction:

- `Driver` serializes prompts and steers one agent, expanding a prompt that
  names a command template;
- `Runtime` is built from the host's own `Options`, not an application's
  configuration. With the cargo feature `agents` (on by default, `src/agents/`)
  it manages background agents and mailbox events, driving each child
  as an ACP client — the one place in the tree that speaks the protocol's
  client side, which is what lets a declared foreign agent be a subagent.
  Without it (`src/solo.rs`) the runtime is that of a lone agent with the same
  method surface, no host tools and no mesh, and `mesh` and `project` are not
  linked. What both have — events, the responder, steering, the inbox —
  is `src/events.rs`;
- `Interactor` implementation answers questions, host tools and goal
  refutation;
- `session` is what a frontend may ask besides a turn: past and fresh
  transcripts, deletion, opening another session, plugin commands and
  command templates, working-tree checkpoints and running commands;
- the model a person chose, at start or by switching, is remembered for the
  next run.

Frontends therefore depend on the host and core, not on the session, paths or
snapshot crates beneath it. They take the host without its default features;
only `apps/elmee` turns `agents` on.

Keeping this outside core prevents TUI, child-process and application policy
from becoming loop invariants.

### `plugins/tui`

**Kind:** frontend adapter.

Implements the interactive inline terminal surface. Finished output remains in
normal scrollback; an OS input thread feeds the async local runtime through
channels. It renders streamed text, reasoning, tool state, plans, dialogs,
commands and background agents. It depends on the terminal alone: its styled
text is five types of its own, drawn to ANSI by its own renderer, which is
also what lets it wrap between words, mark links and diff the live area a row
at a time.

TUI is selected directly by `apps/elmee`, not installed into the agent registry,
because it owns presentation and process lifetime rather than model behavior.

### `plugins/acp`

**Kind:** frontend adapter, and the embedding surface.

The Agent Client Protocol, whole, in one crate: JSON-RPC framing over stdio,
the handshake, the session pool and the translation of a turn's events into
`session/update` notifications. The kernel receives neutral message identities,
events and metrics; the wire shapes stay in the adapter. One file per responsibility: `lib.rs`
(public API, serve loop, dispatch), `wire.rs` (the JSON-RPC client handle,
answers, error codes), `pool.rs` (open sessions, building agents, listing,
replay), `turn.rs` (who may take a session, running and cancelling a turn),
`ask.rs` (elicitation forms), `content.rs` (prompt blocks, declared MCP
servers and a session's system prompt) and `update.rs` (event to
`session/update` translation).

A session takes one turn at a time, as the protocol says, with one exception it
does not have to know about: a turn nobody asked for — a subagent reporting, a
peer writing — yields to a person, because they waited for neither and what
woke it is already in the session.

The v1 agent surface: `initialize`, `session/new`, `session/load`,
`session/resume`, `session/prompt`, `session/cancel`, `session/list`,
`session/delete`, `session/close`, `session/fork`, `session/set_title`,
`session/status`, `auth/status`, and `session/set_config_option` for the
model and its reasoning effort. Three extensions carry what the protocol has
no method for: `_session/steering`, and `_session/history`, `_session/rewind`
and `_session/checkout` for the conversation tree. `session/new` does not wait for the model
catalogue: it answers with the models the machine already knows, the host asks
the endpoints in the background when they were not asked recently, and a list
that changed arrives as a `config_option_update`. Edits are sent
as diffs, plugin commands and command templates are
advertised and run, and questions from the model become `elicitation/create`
forms.
`_session/steering` uses the host's existing steering inbox. Session setup runs
as a connection-owned cancellable job; closing a session runs its hook outside
the reader. The prewarm compatibility check includes client MCP and roots.
Client capabilities that would put an editor between a tool and the filesystem
remain absent. Every message is validated against the vendored schema in the
application's tests, and every declared capability is exercised there.

What the client can show decides what it is sent: compaction updates, notices
and a child's own stream each go only to a client that declared it. A child
is relayed rather than translated — the host hands over the updates the child
already spoke in this protocol, under the child's own session id. A server the
client declares as `acp` reaches the composition root as a `Served` handle
whose one method puts an MCP request to the client; the MCP plugin is given a
closure over it and learns nothing about the protocol.

Unlike the TUI and headless adapters, it is not handed an agent already
built: building one is the expensive thing. Here the handshake is
answered from nothing while the agent for the starting directory is assembled
behind it, and every further session is composed by a closure the application
passes in — the only thing this crate is told about composition.

### `plugins/headless`

**Kind:** frontend adapter.

Implements one-shot text output and versioned JSONL event output. Every JSONL
request ends with exactly one terminal result, error or resumable paused
record. A prompt that names a plugin command runs it, and one that names a
command template is expanded. It is a log, not a protocol: a spawned agent serves its parent over
ACP instead, so there is no private channel to keep.

Like TUI, it is selected by the application outside the registry.

## Multi-agent coordination

### `plugins/mesh`

**Kind:** local coordination mechanism.

Allows independent Elmee processes in one workspace to discover names and
exchange durable messages using file locks and atomic inbox records. A held
lock is presence; no heartbeat, daemon or clock-based lease is needed. A
request carries who to answer, a result carries the request it answers, and a
peer's work becomes its own turn, so an outcome settles exactly one operation.
An execution journal and completion history make a delivered request run at
most once across restarts. Only `plugins/host` and `apps/elmee` depend on it,
each behind its `agents` feature.

The mesh supports human-directed collaboration between already-running agents.
It is not an autonomous chatter bus.

## Why some dependencies form a mesh

Not every plugin crate is independently replaceable in isolation. `tools`,
`host` and `tui` are integration layers and therefore know several neighboring
mechanisms. The architectural rule is not “every plugin depends only on core.”
It is:

- application-specific selection stays in `apps/`;
- reusable product behavior does not depend upward on an app;
- a dependency represents a declared mechanism, not hidden neighbor discovery;
- high-authority or heavy optional adapters retain visible boundaries;
- core never learns the concrete dependency graph.

This admits a practical modular monolith without pretending that physical
crate separation alone guarantees logical independence.
