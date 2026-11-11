# Working on elmee

This file is for whoever changes this repository — a person or a coding agent.
It says where things go, what has to stay true, and what was decided against.
The [architecture handbook](docs/README.md) explains why; this is the short
form you act on.

## What this is

elmee is an agent microkernel in Rust and the coding agent composed from it.
Its reason to exist is to be **small, fast to start and embeddable**: one
binary under 5 MB, ready in about ten milliseconds, driven by other programs
over the Agent Client Protocol, and economical enough with context to work on
local models. A change that makes it bigger, slower to start or harder to embed
has to pay for itself.

## Layout

```text
core/                 the provider-neutral turn loop: agent, session tree,
                      plugin registry, evidence, plan. No async runtime, no
                      HTTP, no frontend, no concrete provider.
plugins/<name>/       one capability or adapter each (see docs/components.md)
apps/elmee/           the composition root and the installed binary
apps/elmee/tests/     the product as a user or a client meets it
clients/              one-file ACP clients (Python, TypeScript), run in tests
docs/                 user guides and the architecture handbook
benchmarks/harnesses/ how the numbers in the README were measured
```

Dependencies point one way: `apps -> plugins -> core`. Only `apps/` may know
which plugins exist. A plugin that needs another plugin's capability is handed
it by the composition root as a constructor argument; it never looks it up.

## Before you say a change is done

All of these pass, with no warnings:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p elmee --no-default-features --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
```

CI also checks the minimum Rust version (`cargo +1.89 check --workspace
--all-targets --locked`), a static musl build and `cargo deny check`. A new
dependency has to pass `deny.toml` and has to be worth its weight in the
binary: check `stat` on `target/release/elmee` before and after.

Tests need no network and no credentials: a stub provider stands in for the
model. `python3` is needed for the client test; the TypeScript one skips
itself without Node 24.

## Rules for the code

- **Write the minimum that keeps the behaviour.** Before adding, look for what
  can be removed or reused. An addition is justified by what it replaces.
- **No dead code and no commented-out code.** Delete it; git remembers.
- **No compatibility shims.** There are no users of old formats to protect:
  change the format and the reader together, and remove the old one.
- **One file, one responsibility; composition over inheritance.** If a file
  needs "and" to describe it, split it.
- **Comments say why, not what.** The codebase's comments explain the reason a
  thing is the way it is — the failure it prevents, the alternative that was
  worse. Match that; do not narrate the code.
- **Tests are named as sentences that state the behaviour**:
  `a_cancelled_turn_stops_and_leaves_the_session_working`. A bug fix comes with
  the test that would have caught it.
- **Errors are for the person who has to act on them**: say what happened and
  what to do, not which function failed.

## Where a change goes

| You want to | It goes in |
| --- | --- |
| Add a tool the model can call | `plugins/tools`, or a plugin of its own if it has its own dependency or authority |
| Support another model API | `plugins/provider` |
| Add a way to drive the agent | a frontend adapter plugin, selected by `apps/elmee` |
| Change what a turn is | `core` — and read the gate below first |
| Add policy around tools or completion | a `Middleware` in the plugin that owns the policy |
| Wire two capabilities together | `apps/elmee/src/lib.rs`, explicitly |
| Add something untrusted or dynamic | not in-process: MCP, a process, or a skill |

Prefer, in this order: a skill (a `SKILL.md` and the shell), an MCP server, a
plugin, a change to `core`.

**The gate on `core`.** The kernel is meant to stop changing. A new universal
role, lifecycle phase or registry slot needs two unrelated production consumers
that the present contract cannot express, or a composition law shown to be
violated. "It would be more flexible" is not a reason. See
[docs/extending.md](docs/extending.md#architecture-review-gate).

## What has to stay true

- **The prompt prefix is byte-stable.** Tool definitions, skills and the
  system prompt are in the same order on every run; anything that varies goes
  at the end. One byte of drift at the front throws away a provider's cache.
- **Context is spent relative to the window.** Nothing a request carries is a
  fixed size: tool output ceilings, the skills listing and compaction all
  scale with the model's context window. A change must work at 8k tokens.
- **One agent, one thread.** An `Agent` and its registry are `Rc`/`RefCell`
  values on a current-thread runtime. Parallelism is separate agents, not
  shared ones.
- **The ACP reader never waits.** Building an agent, starting MCP servers and
  session-end hooks run as jobs; the loop that reads stdin stays free to
  cancel and to serve other sessions.
- **The handshake is a promise that is tested.** Every message sent is
  validated against the vendored schema (`apps/elmee/tests/fixtures/`). A
  capability added to `initialize` needs a line in
  `everything_the_handshake_declares_is_answered` and a mention in
  `docs/embedding.md`; the tests fail otherwise.
- **A client is only sent what it said it can show**: compaction updates,
  notices and subagent streams are gated on client capabilities.
- **A session is one append-only JSONL file.** Rewind, branches, checkpoints
  and titles are records in it, not side files.
- **Effects that cannot be rolled back are made durable instead**: stable
  ids, records on disk, idempotent recovery.
- **Untrusted text is fenced where it reaches the model** — tool output, MCP
  descriptions, peer messages, files.
- **Declaring a plugin is inert, and independent plugins commute.** Source
  order must not change behaviour.

## Decided against

These were considered, sometimes built, and removed. Do not bring them back
without new evidence.

- **Permissions, approval prompts, a sandbox.** The boundary is the operating
  system; the `before_tool_call` hook is where a guard attaches. See
  [SECURITY.md](SECURITY.md).
- **Windows.** File locks and process groups are load-bearing.
- **The client's `fs/*` and `terminal/*` in ACP, and session modes.** A file
  read through an editor buffer and patched on disk are two files; one mode of
  behaviour needs no switch.
- **Provider credentials over the protocol.** They belong to the machine.
- **A second wire protocol or an inter-harness mesh protocol.** ACP is the
  one; subagents are processes that speak it.
- **A browser plugin in this repository**, a WebSocket transport for
  Responses, and anything that exists only for backward compatibility.
- **A line budget.** The rule is minimum code, not a ceiling.

## When you change behaviour, change the words

- A new or changed flag: `apps/elmee/src/config.rs` help text is the CLI
  reference; `docs/usage.md` if a person would look it up.
- A new shipped crate: [docs/components.md](docs/components.md).
- Anything a client can see: [docs/embedding.md](docs/embedding.md).
- Anything about authority: [SECURITY.md](SECURITY.md).
- A number in the README: measure it again with
  [`benchmarks/harnesses`](benchmarks/harnesses) and update the date.

If the code and the documentation disagree, that is a bug in one of them; fix
both in the same change.

## Commits

One change, one commit, with a subject that is a sentence about what changed
and why it matters — not a list of files. No generated signatures or co-author
trailers.
