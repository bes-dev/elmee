# elmee

[![CI](https://github.com/bes-dev/elmee/actions/workflows/ci.yml/badge.svg)](https://github.com/bes-dev/elmee/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

**A coding agent small enough to put inside something else.**

elmee is one 5 MB binary with no runtime to install. Run it in a terminal and
it is a coding agent; start it with `--acp` and it is an agent *engine* that an
editor, an IDE plugin or your own product drives over the
[Agent Client Protocol](https://agentclientprotocol.com) — ready in ten
milliseconds, holding seven megabytes while it waits, serving as many sessions
as you open from the one process.

It is written in Rust as an agent microkernel: a provider-neutral loop in
`core`, every capability a plugin, and one composition root that decides what a
product is made of.

```sh
elmee -m ollama/qwen3-coder                       # a terminal agent
elmee -m anthropic/claude-opus-5 "fix the failing test"   # one run, then exit
elmee --acp                                       # an engine for your application
```

## Why another agent

Because the others are applications, and this one is a part.

| Measured on one machine, 2026-11-10 | elmee | pi | Codex | Claude Code |
| --- | ---: | ---: | ---: | ---: |
| On disk | 4.8 MB | 149 MB + Node | 271 MB | 324 MB |
| Ready to take a prompt, as a server | 9 ms | ≈150 ms | 128 ms | 291 ms |
| A second session in the same process | 0.9 ms | — | 5.4 ms | 110 ms |
| Memory while idle, as a server | 7 MB | 173 MB | 235 MB | 645 MB |
| Overhead of one text turn | 34 ms | 148 ms | 236 ms | 160 ms |

It is not the fastest at everything: a tool call costs elmee about 24 ms of
its own against pi's 4, because every command's result is recorded against the
state of the working tree. The scripts, the method and the raw results are in
[`benchmarks/harnesses`](benchmarks/harnesses) (Codex 0.167, Claude Code
2.1.322, pi 1.4.0; Apple M4 Max).

Three things follow from being small and being a part:

- **It embeds.** One process, any number of sessions, a published protocol
  with a schema, and two [one-file clients](clients). Your application can
  hand the agent its own functions as tools over the same connection — no
  server process, no port.
- **It runs on local models.** Everything a request carries is sized to the
  model's context window rather than fixed, old tool output is shortened before
  anything is summarized, and the prompt stays byte-stable so a local server's
  cache keeps hitting. An 8k window and a 200k one run the same agent.
- **It stays out of the way.** No daemon, no telemetry, no account. State is
  plain files under `~/.elmee`; a session is one JSONL file you can move.

## Install

From source, with Rust 1.89 or newer:

```sh
cargo install --git https://github.com/bes-dev/elmee --locked elmee
```

A tagged release is built as checksummed archives for Linux (x86_64 and
aarch64, each as a glibc build and as a statically linked musl build — the one
to put in a container or ship inside another product) and macOS (Intel and
Apple Silicon), with an SPDX SBOM. Until one is tagged, build from source.

elmee runs on Unix. File locks and process groups are what it is built on, and
Windows is not supported.

## Use it

Pick a model — a local server, an API key, or a ChatGPT subscription:

```sh
elmee -m ollama/qwen3-coder:latest

export ANTHROPIC_API_KEY=sk-ant-...
elmee -m anthropic/claude-opus-5

elmee login codex && elmee -m codex/gpt-5.6-terra
```

Without a prompt you get the terminal interface; with one, a single run that
prints the answer and exits; with `--json`, a JSONL stream for scripts:

```sh
elmee "explain what this repository does"
git diff | elmee --json --cwd /repo -
```

`elmee --help` is the complete CLI reference. The [user guide](docs/usage.md)
covers sessions, rewinding a conversation and the files with it, the terminal
interface, background agents and goals; [models and configuration](docs/models.md)
covers providers, `models.json` and local models.

## Embed it

```python
from elmee import Elmee            # clients/python/elmee.py — one file, no dependencies

def selection(arguments):
    return editor.selected_text()

tools = {"selection": ("What the user has selected.", {"type": "object"}, selection)}

with Elmee("-m", "ollama/qwen3-coder", tools=tools) as agent:
    session = agent.session("/path/to/project")
    for update in session.stream("explain what I have selected"):
        if update["sessionUpdate"] == "agent_message_chunk":
            print(update["content"]["text"], end="")
```

That is the whole integration: start the process, open sessions, stream turns,
and serve tools from your own code. The same in
[TypeScript](clients/typescript/elmee.ts). Over the protocol a client can also
give a session its own system prompt, list and resume past conversations, fork
one, rename it, steer a turn that is running, rewind the conversation and the
files to before any prompt, and watch subagents work as sessions of their own.

Every message elmee sends is checked in its tests against the published
schema, and every capability it declares is exercised there. The
[embedding guide](docs/embedding.md) has the full surface;
[genie.el](https://github.com/bes-dev/genie.el) is a complete product built on
it, with not one line of agent loop of its own.

## What is in the box

- **Tools**: read, patch, shell with background runs, image viewing, web
  fetch and search.
- **[MCP](docs/customization.md#mcp)** over stdio and Streamable HTTP — and,
  embedded, over the connection the client already has.
- **[Skills](docs/customization.md)** as plain `SKILL.md` files that are also
  slash commands; project instructions from `AGENTS.md`.
- **[Hooks](docs/customization.md#hooks-and-verification)** before a tool
  call, after one, before the agent may call something done, and when a
  session ends.
- **Sessions** as single files, with [rewind and branches](docs/usage.md#sessions-rewind-and-branches):
  go back to before any prompt, files included.
- **Compaction in two stages**, so a long task survives a small window.
- **[Subagents](docs/usage.md#goals-background-agents-and-project-peers)**
  that are separate processes speaking the same protocol — another elmee, or
  any other ACP agent — and a local mesh for independent agents in one project.
- **Standing goals** with evidence that a completion claim is true of the
  tree as it is now.

## What bounds the agent

Nothing inside elmee. There is no permission prompt and no sandbox: a command
the model runs has the authority of the person who started elmee. That is a
decision — a boundary a harness draws around itself is one it can also get
wrong, and the ones that hold are outside the process: an account, a
container, a VM.

What you get instead is undo, and a place to attach a guard. The working tree
is checkpointed before every turn; a project's `before_tool_call` hook runs
before every tool call and can refuse it without asking anyone.
[SECURITY.md](SECURITY.md) says what this means for deployment — read it before
giving elmee anything you care about.

## How it is built

```text
apps/elmee          the product: which plugins, and how long the process lives
    |
plugins/*           capabilities and adapters, each replaceable
    |
core                the provider-neutral turn loop, and nothing else
```

Dependencies point downward only. A plugin declares typed roles — a service, a
handler, a middleware — into a registry that is resolved once and frozen;
what one plugin needs of another it is handed by the composition root.
`elmee --explain-plugins` prints the resolved graph.

The [architecture handbook](docs/README.md) is the design reference:
[philosophy](docs/philosophy.md), [layers and flows](docs/architecture.md),
the [composition contract](docs/composition.md), [core](docs/core.md), the
[component catalog](docs/components.md), [state and recovery](docs/runtime-and-state.md)
and a guide to [extending it](docs/extending.md).

## Documentation

| | |
| --- | --- |
| [User guide](docs/usage.md) | Running it: sessions, rewind, the terminal interface, JSONL, goals, subagents |
| [Models and configuration](docs/models.md) | Providers, `models.json`, model resolution, local models |
| [Customization](docs/customization.md) | Skills and commands, MCP, hooks and verification |
| [Embedding](docs/embedding.md) | The Agent Client Protocol surface, clients, what is declared |
| [Architecture handbook](docs/README.md) | Why the code is shaped the way it is |
| [SECURITY.md](SECURITY.md) | What bounds a running agent, and how to report a vulnerability |
| [AGENTS.md](AGENTS.md) | Working on elmee itself — for people and for coding agents |

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
```

[AGENTS.md](AGENTS.md) is the guide to the codebase: where things go, what must
stay true, and what was decided against. [CONTRIBUTING.md](CONTRIBUTING.md)
says how to send a change.

## License

Apache License 2.0 — see [LICENSE](LICENSE). Contributions are accepted under
the same terms, as its section 5 states.
