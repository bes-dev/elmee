# Runtime, state and reliability

## Runtime model

The `elmee` binary uses Tokio's current-thread runtime plus a `LocalSet`.
Each `Agent` has one task owner. This matches the mutable provider and handler
state, avoids synchronization in the hot loop and makes ordering within a
conversation explicit.

Concurrency exists around that owner:

- separate conversations use separate agents;
- child agents run behind host channels or processes;
- process and network adapters overlap I/O;
- authorized read batches may prepare concurrently;
- whole-turn barriers can be computed asynchronously and settled before the
  first tool effect.

Do not put an `Agent` behind a global `Mutex` to make a server. Create one agent
per durable conversation, keep each on one local task and route messages to it.

## State ownership map

| State | Semantic owner | Shipped persistence |
| --- | --- | --- |
| Active conversation tree | core `Session` | session JSONL adapter |
| Completion evidence | core `EvidenceManifest` | none: in memory, per run |
| Live plan | core `Agent` | conversation/compaction context |
| Project memory | project plugin | workspace `.elmee/memory` |
| Shared billed-token total | core budget | append-only token ledger |
| Working-tree checkpoints | snapshot plugin | external Git directory |

Semantic ownership and file-format ownership are deliberately separate. Core
defines a conversation entry; the session plugin defines how it is written.

## Sessions

Core sessions are append-only trees rather than mutable arrays. Branching,
checkout and compaction add entries and preserve history. The active provider
context is a derived branch.

The JSONL adapter provides:

- an exclusive writer lock so two live writers do not interleave records;
- serialization to one byte buffer followed by one `write_all` per record;
- a schema version on tree records;
- refusal of a newer unsupported schema;
- branch selection (`/clear`, `/rewind`, `/tree`) and the checkpoint of the
  working tree taken before each prompt, as records of the same journal, so
  there is no second file to disagree with the first;
- recovery that discards only a torn final line.

An interrupted model tool call is semantically repaired by core on resume: an
explicit interrupted tool result is appended before the next provider request.
This preserves the provider protocol invariant that every call has a result.

## Compaction

Making room is a tree operation, not destructive transcript rewriting, and it
has two stages, cheapest first.

**Shortening old tool output.** Tool output is most of a long context and the
least of what it means. When the context outgrows its room, output older than
the newest quarter of that room is shown shortened — its first and last
lines, with the middle left out — while the reasoning and the calls
themselves stay in view. No model is asked. The journal records only up to
which entry this applies, so nothing is lost from the file and the rendered
prefix is stable until the next time. Measured on coding agents, masking
observations this way keeps as many tasks solved as summarizing does at lower
cost, so a summary is written only when this did not bring the context under
three quarters of its room.

**Summarizing.** Then the older part of the conversation is replaced by a
summary:

1. only that older part is sent — the newest quarter stays verbatim, and a
   request that is the conversation's own prefix is one a prompt cache has
   already read;
2. the summary is written in fixed sections (goal, constraints, done, state,
   failed, next), and an earlier summary is updated rather than summarized
   again;
3. what a summary loses first is carried by the harness instead of trusted to
   it: every user prompt word for word, the files changed (read off the
   applied patches by the tools plugin), and the open steps of the plan;
4. compaction middleware may take notes out of the summary first;
5. a system entry records the summary and how many entries stay verbatim.

The room is the model's window less the reply it will be asked for, counted
with the tool schemas that travel in every request and scaled by what the
provider actually charged. No single tool result may take more than an eighth
of it: a longer one loses its middle on arrival. If making room frees less
than roughly ten percent, it is not attempted again that session and the
model is told why.

A request the provider refuses as larger than the model's context is not a
failure: the room is set below what was refused, made, and the request sent
again — once. That is how a window nobody declared is learned.

## Evidence and workspace state

Successful commands can return neutral `CommandEvidence`. Core records it with
the current workspace fingerprint. Project completion hooks can emit the same
neutral evidence at `BeforeFinish`.

Before accepting a required verification level, core refreshes the fingerprint
and marks evidence from another tree stale. Evidence is never written to
disk, so a resumed session proves its claims again.

Snapshots are separate from evidence. A snapshot captures restorable state;
evidence records an observation about a state. Neither substitutes for the
other.

## Process ownership

Hooks and stdio MCP servers are children of explicit adapter
handles. Correct shutdown follows a strict order:

1. stop producing input and drop child stdin;
2. request termination when needed;
3. keep draining/reaping until the child is collected;
4. treat EOF before the required protocol report as failure.

This order avoids pipe deadlocks and zombies. A child process must never outlive
the application merely because a plugin task was cancelled.

A command the model starts with `exec` is the deliberate exception: it is a
durable run in its own process group, with its log and exit status in the
project's state directory. A run that outlives its wait carries on in the
background; stopping the turn that was waiting on it ends it.

## Operational supervision

A long-running deployment — an editor's ACP process, a scheduled job — should run under
`systemd --user`, launchd, a container restart policy or an equivalent
supervisor. The division of labor is:

- plugins retry ordinary transient failures locally;
- durable state makes replay safe;
- unrecoverable failures exit visibly;
- the supervisor restarts the whole coherent process.

This is simpler and more reliable than trying to reconstruct a partially
failed graph in place.

## Recovery tests

Reliability changes should include real boundary tests where practical:

- torn JSONL final record;
- concurrent session writer rejection;
- child EOF before a required report;
- shutdown while child stdin is still open.

Pure unit tests are insufficient for file locking, real pipes and process
reaping; the repository uses real temporary files and child processes for these
contracts.
