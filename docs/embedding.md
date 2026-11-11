# Embedding elmee: the Agent Client Protocol

`--acp` serves the [Agent Client Protocol](https://agentclientprotocol.com) on
stdio — the same protocol Zed, JetBrains, Neovim, Emacs (`agent-shell.el`) and a
hundred other clients already speak. It is the way to embed elmee in something
else, and it replaced a bespoke protocol of ours: two hand-written duplex
protocols for one job drift, and only one of them came with a published schema
and clients that already exist.

## One process, many sessions

One process serves every session the client opens. That is where embedding
speed actually lives: an editor with four projects pays one process start, one
plugin graph per project and keeps every prompt cache warm, instead of paying
all of it per conversation. Measured on an M-series Mac on 2026-11-10:
**2–3 ms** from spawn to `initialize` answered — the handshake does not wait
for an agent — **5 ms** for the first `session/new` and **1–2 ms** for each one
after it, with the session for the directory elmee was started in assembled
behind the handshake so it is usually already standing. A turn adds about
15 ms of its own to whatever the model takes.

Sessions run concurrently: one may think while another answers, and
`session/cancel` stops a turn, ends the command it was waiting on, and leaves
its session immediately usable.
A subagent's report, or a message from another elmee in the project, starts a
turn of its own and streams to the editor with nobody prompting — and a person
typing takes that turn over rather than being told the session is busy.
A session is a file, so its path is the id it was given: `session/list` offers
the ones on disk for a directory by the first thing asked of them,
`session/load` hands one back replayed as ordinary updates — tool calls
included — and `session/resume` takes it up without the replay.

Slow MCP initialization and session-end hooks do not hold the connection's
reader. Other sessions can still be listed or cancelled. Pending session setup
can be cancelled with `$/cancel_request`; disconnect releases pending builders.
The prewarmed agent is used only when its workspace and MCP configuration match.

## Forks and steering

`session/fork` copies an idle open conversation into its own transcript and
inherits its model, reasoning effort and workspace settings. The parent remains
independent. `_session/steering` accepts a correction for the running turn,
consumed at the next model/tool boundary. Its optional
`_meta.steering.idleBehavior: "promptRequired"` leaves idle input with the client;
without it, idle steering starts a new turn. Steering currently accepts text
and embedded context.

## What a client is given to draw

What an editor draws, it is given: an `apply_patch` arrives as a `diff` it can
render in place, a `read` names the file so the editor can follow along, every
model request reports how full the context is, and the model and its reasoning
effort are the session's config options, switched with
`session/set_config_option`. `session/new` does not wait for the model
catalogue: the model option lists what this machine already knows — the models
its files declare and what each endpoint said the last time it answered, kept
in `~/.elmee/model-catalogue.json` — and the endpoints are asked in the background
at most once in five minutes; a list that came back different arrives as a
`config_option_update`. The TUI's `/model` opens on the same list at once. The commands the agent's plugins own and the project's
skills are advertised: a prompt of
`/name arguments` runs the plugin command instead of reaching the model, or
reaches it as the prompt the skill expands to. The handshake offers one way to
sign in, `codex`: `authenticate` with it opens the browser sign-in the
`elmee login` command does and answers when it ends, and a turn that needs an
account nobody is signed in to fails with the protocol's `auth_required`
(-32000) so the client can offer it. A session's connection to its provider is
opened while the session is being set up, so the first prompt does not wait
for a TLS handshake. A picture pasted into the prompt is saved under `media/acp/` and named,
which is how a picture reaches the model. A question the model asks becomes an
`elicitation/create` form the editor draws, with the options it offered; a
client that cannot show one gets the question cancelled rather than a stuck
turn. MCP servers a client names in `session/new` — stdio, HTTP, or the client
itself — serve that session alongside the machine's own.

## What the handshake declares

What the handshake declares, and what each one is for:

| Declared | Request | What it gives the application |
|---|---|---|
| `list` | `session/list` | The conversations kept for a directory, by title, newest first. |
| `resume` | `session/resume`, `session/load` | A kept conversation taken up again, with or without the replay. |
| `fork` | `session/fork` | An independent copy of an idle conversation. |
| `close` | `session/close` | The agent, its MCP servers and its children released. |
| `delete` | `session/delete` | A closed conversation and everything kept beside it removed. |
| `status` | `session/status` | Whether a conversation is `open` in this process or only `saved`. |
| `title` | `session/set_title` | A name in place of the first prompt, for an open or a saved conversation. |
| `systemPrompt` | `session/new`, `load`, `resume`, `fork` | Instructions for one session: `append` to the agent's own, or `override` them. |
| `additionalDirectories` | the same four | More workspace roots than the one the session is in. |
| `subagents` | `subagent_update` | Each child as a session of its own on the same connection. |
| `auth.status` | `auth/status` | Who is signed in, before the client decides what to offer. |
| `auth.logout` | `logout` | The stored account forgotten. |
| `steering` | `_session/steering` | A correction for the turn that is running. |
| `rewind` | `_session/history`, `_session/rewind`, `_session/checkout` | The conversation as a tree the client can walk. |


## Three things other agents do not offer

None of these is among what the Codex and Claude adapters declare
(`codex-acp` 2.6.0 and `claude-agent-acp` 0.104.1, read on 2026-11-10).

**The application as an MCP server.** A server declared in `mcpServers` as
`{"type": "acp", "name": …, "serverId": …}` is the client itself: its tools
are listed and called with `mcp/message` requests over the connection that
already exists, so an editor offers the agent its selection, its diagnostics
or its debugger without starting a process or opening a port. Each request
carries the stateless MCP revision's metadata; there is no handshake to hold.

**Rewind over the protocol.** `_session/history` answers with the prompts on
the line the conversation stands on — each with an `id` and whether the files
were recorded before it — and every other line a rewind left behind.
`_session/rewind` with a `promptId` goes back to before that prompt and hands
its text back to be edited; `restore` is `everything` (the default),
`conversation` or `files`. `_session/checkout` with a branch's `leaf` returns
to a line that was left. Both answer with the tree as it is afterwards, and a
client that wants the transcript redrawn asks for it with `session/load`.

**Steering**, described above: a turn in flight takes a correction instead of
a second prompt being refused.

## What a client can ask to be shown

A client that says it can show them is told more. With
`clientCapabilities.session.compaction`, a compaction arrives as the summary
being written (`compaction_summary_chunk`) and the one that now stands in the
conversation's place (`compaction_update`). With `session.notices`, what went
wrong outside the conversation — a declared MCP server that would not start, a
turn nobody prompted that failed — is a `notice`. With `subagents`, each child
is announced on its parent's session and then streams under its own session
id, in the words it used: a child is another process speaking this protocol,
so nothing is translated. Every `tool_call` carries the tool's `name` beside
the title a person reads.

## Clients in one file

`clients/python/elmee.py` and `clients/typescript/elmee.ts` are the whole of
this from an application's side, each one file with no dependencies: start the
process, open sessions, stream a turn, and hand the model functions of your
own as tools. Both are run against the binary in the test suite.

```python
from elmee import Elmee

with Elmee(tools={"selection": ("What the user has selected.", {"type": "object"}, selected)}) as agent:
    session = agent.session("/path/to/project")
    print(session.prompt("explain what I have selected"))
```

## Kept from drifting

Every message elmee sends is checked in its tests against the published
schema — version 1 with its unstable additions, vendored under
`apps/elmee/tests/fixtures/` — and every capability the handshake declares is
exercised there, so a specification that moves every few weeks cannot drift
away unnoticed.

## What it deliberately does not do

It does not use the client's `fs/*` and
`terminal/*`: a file read through an editor's buffer and patched on disk are
two different files, and a command the editor runs is one the agent cannot
leave running. What a person has not saved reaches the model as embedded
context in the prompt, which every client can already send. It does not ask
`session/request_permission`: nothing here asks before acting, and a guard
attaches at the `before_tool` hook instead. It does not advertise modes,
because this agent has one way to behave and a control that changes nothing
is worse than an absent one; nor take provider credentials over the
connection, which belong to the machine.
