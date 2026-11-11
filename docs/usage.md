# Using elmee

Without a prompt, Elmee starts the interactive TUI. With a prompt it performs
one headless run; prompt `-` reads stdin.

```sh
elmee -m MODEL
elmee -m MODEL "fix the failing test"
git diff | elmee -m MODEL --json --cwd /repo -
```

## Reasoning depth and request metrics

OpenAI Responses (including the Codex/ChatGPT subscription) and OpenAI-compatible
Chat Completions accept a reasoning-depth override:

```sh
elmee -m codex/gpt-5.6-sol --reasoning-effort high
```

Use `auto` (the default) to leave the parameter out and preserve the model's
own default. Supported levels are `none`, `minimal`, `low`, `medium`, `high`,
and `xhigh`; an endpoint may reject a level its selected model does not support.
In the TUI this can be changed between turns with `/reasoning LEVEL`; ACP clients
receive the same choice as the standard `thought_level` session option.

Responses requests report cache-hit tokens, first text/tool timing,
headers, total duration, attempts and the effective service tier in headless
`model_end.metrics` records. ACP includes the same observations under
`_meta.elmee` in usage updates and the completed turn. Missing observations are
omitted. Serialization/auth timings are local durations; the other timings are
elapsed since the request began. Credentials and request content are excluded.

Each conversation has its own routing/cache identity. Stored conversations keep
their identity across resume; paths are hashed before they reach the provider.
Responses may request several independent tools in one response; execution
retains hook authorization, ordering of effects and atomic patch behavior.

## More than one directory

`--add-dir DIR` can be repeated to name additional workspace directories.
ACP clients can supply the same list as `additionalDirectories`. The model is
told their absolute paths, directory instructions arrive when files there are
first touched, images can be attached from them, and fingerprints
and rewind checkpoints cover all configured roots. A checkpoint made with a
different root set is refused before restoring files. These directories do not
create a permission boundary; elmee retains the authority of its host process.

## Sessions, rewind and branches

Sessions persist under `~/.elmee/sessions` by default. Use `--continue` for the
newest session in the current project, `--session PATH` for an explicit file,
or `--no-session` for an ephemeral run.

`/clear` empties the active context but preserves transcript history. Queued
`/next` follow-ups return to the editor for explicit resubmission; they do not
automatically refill the cleared context. An existing draft is preserved too.
An already-running turn is not cancelled by this command. `/clear`,
`/rewind` and `/tree` record the branch they select as a line of the
transcript itself, so reopening without another message keeps that selection
and a session is one file to move or back up.

`/rewind` (or Esc Esc on an empty prompt) lists what was asked on the current
branch. Choosing a prompt goes back to just before it — the conversation, the
files as they were before that prompt's turn changed them, or both — and hands
the prompt back for editing. The working tree is recorded before each turn in
a store outside the project, and a rewind records the state it leaves first,
so nothing it removes is gone. What was rewound over stays in the session as
a branch, and `/tree` lists the branches and continues on the one chosen.
Opening a session, switching branch and going back all redraw the conversation
as it then stands. This is not permanent deletion:
history and mesh peers' copies are separate records.

Completion evidence is not stored: it describes the working tree one run
observed, so a resumed session starts with none and proves its claims again.

The ACP `session/delete` operation removes a closed session's transcript and
what is kept beside it (a torn tail, a token ledger). It requires
`session/close` first, and deleting a session that is not there is an error.
Listing errors are reported rather than silently treated as an empty session
directory.

Transcript paths must name regular files directly: file-level symlinks, pipes
and devices are rejected on open/read. This does not prohibit symlinked parent
directories or claim confinement against arbitrary ancestor replacement by
another process.

## PDF documents

PDF work uses the [PDF skill](../.agents/skills/pdf/SKILL.md) and ordinary shell
tools, not a bundled parser or a dedicated MCP server. Poppler supplies
`pdfinfo`, `pdftotext` and `pdftoppm`; Python libraries or OCR are optional
fallbacks for tasks that need them. The skill is discovered automatically
in this checkout. To use it in other projects, copy it from this checkout
into your user skills:

```sh
mkdir -p ~/.agents/skills/pdf
cp .agents/skills/pdf/SKILL.md ~/.agents/skills/pdf/SKILL.md
```

Install Poppler with `brew install poppler` on macOS or
`sudo apt-get install poppler-utils` on Debian/Ubuntu. The agent must ask
before installing dependencies. `view_image` sends rendered PNG, JPEG, GIF
or WebP files (up to 5 MiB each) to the current model; `read` remains
text-only. The skill distinguishes text extraction from visual review and
reports when image inspection is unavailable. Existing PDF MCP declarations
should be removed from your user/project configuration or the editor's server
list.

## Web pages

`web_fetch` returns extracted source text without an extra model request.
Output defaults to 8 KiB; use its returned byte `offset` to continue, or
request `max_output_bytes` up to 50 KiB. To deliberately spend an additional
model call answering a question from the page, set `summarize: true` and
provide `prompt`. A question alone does not enable summarization. Summary
input is bounded to 256 KiB and uses the same byte offsets for continuation.

## Process commands

```sh
elmee notify --to reviewer "inspect the latest change"
gh api repos/owner/repo/issues/1 | elmee notify --stdin "triage this issue"
```

## TUI

The inline TUI keeps finished output in normal terminal scrollback: it draws
as a document in the main screen, never the alternate one, so the terminal's
own scrolling, searching and selecting keep working. A prompt stands against a
bar in the accent colour and a reply hangs from a dot, so a page of them reads
as a conversation. Replies are Markdown drawn as a terminal can: headings,
lists at any depth, quotes, rules, whole tables with their columns lined up,
highlighted code without its fence lines, links with their address beside
them; lines wrap between
words. A tool call is one row — what was done and how it went — and a run of
them stands together as one block. An edit names its file, a link a terminal
can open, with the lines it added and removed and the first of its diff
beneath; a command that failed shows its exit status and the end of what it
printed; a plan shows how far it has come and a question what was answered.
The status line says what the agent is waiting on — the model, or a tool by
name — and each frame is one synchronized update, so nothing tears.

`Enter` sends, `Ctrl+J` or `Alt+Enter` adds a line, `Ctrl+R` searches prompt
history, `Alt+←/→` and `Alt+Backspace` move and delete by word, `Ctrl+O` opens
tool output, and `Ctrl+D` quits. `Esc` twice clears what is typed, or stops
the turn — and stopping a turn ends the command it was waiting on; only a
command that had already outlived its wait carries on, as a background run
(`/runs`). With nothing typed and nothing running it opens
`/rewind` with your last prompt first, except in the moment after a turn ends,
when the keys were plainly meant for that turn. The pickers (`/model`,
`/resume`, `/rewind`, `/tree`) narrow as you type.
The window title says whether the agent is thinking, and a turn longer than
`ELMEE_TUI_NOTIFY_SECS` (15 by default, 0 for never) rings the bell and sends
the terminal's notification when it ends; `ELMEE_TUI_TITLE=off` leaves the
title alone. Colours are roles in `~/.elmee/theme.json` or the project's
`.elmee/theme.json`: `accent` (the prompt, selections, whatever is working),
`muted`, `border`, `warning`, `success`, `error`, `keyword`, `agent` and
`user_bg`, the background behind what you said (`"reset"` for none).

Type `/` for the live command menu. `Enter` on the start of a name runs the
highlighted command — or completes it and waits, when it takes an argument —
and `Tab` only completes. Reports that only read (`/usage`, `/agents`,
`/runs`, `/help`) answer at once even while a turn is running; a command that
needs the agent says it is busy instead of acting later. Important commands include `/model`,
`/compact`, `/goal`, `/agents`, `/runs`, `/resume`, `/rewind`,
`/tree`, `/mcp` and `/help`. `! command` runs a command
as the human in the current terminal and does not send its output to the model.

## Headless JSONL

`--json` emits a versioned JSONL stream ending in exactly one terminal record:
`result`, `error`, or resumable `paused`. Records include streamed text,
model/tool lifecycle, token use, duration, verification and evidence. This is
the stable surface for scripts and orchestrators — a log, not a protocol:
nothing is read back. A one-shot prompt of `/name arguments` runs the plugin
command of that name, or expands the prompt template of that name, as it does
in the other frontends.

One-shot runs do not offer `ask_user`, since this frontend cannot read an
answer. TUI and ACP sessions retain interactive questions.

SIGINT exits 130, SIGTERM 143, ordinary failures 1, and resumable provider
quota exhaustion 75.

## Goals, background agents and project peers

`--goal TEXT` or `/goal` installs a standing objective. When `--tools` is set,
it must include `goal_done` to use goals; otherwise the goal plugin and its
slash command are not installed, and `--goal` is rejected. The goal plugin keeps
working across turns until the agent declares completion and configured
verification accepts the claim, or the continuation limit is reached.

The coding host can spawn background agents. Each is a process driven over the
Agent Client Protocol — the same one an editor drives elmee with — so a
subagent is by default another elmee with its own session, and can be any
other program that speaks the protocol on stdio once the project names it:

```json
{ "agents": { "claude": { "command": "claude-code-acp" } } }
```

Declared in `.elmee/agents.json` or `~/.elmee/agents.json`, and named with
`spawn_agent`'s `agent` field, it is delegated to, watched and reported on
exactly like another elmee; a permission it asks for is granted, because
nothing inside elmee asks anyone. Independent Elmee processes in one working
directory can also register names and exchange durable messages through the
local mesh, each under the name it was started with:

```sh
elmee --name implementer
elmee --name reviewer
```

Presence uses process-held file locks and delivery uses atomic inbox files
under `~/.elmee/projects/<project-hash>/mesh-v2`, or an explicit shared
`ELMEE_MESH_ROOT`; no daemon or shared database is required. A project is the
repository: an agent in a worktree, or in a subdirectory, is in the same mesh as
one at the root, which is what an orchestrator that gives every agent its own
worktree needs. Records are written 0600 under 0700 directories: a mesh belongs
to the user who owns it, and any process running as that user is a peer.

A peer's request becomes its own turn, so its answer settles that request and
nothing else; the agent never polls for work. Replies carry the request they
answer, so an answer cannot itself be answered. A turn cancelled before it
answers reports its request as cancelled. An ACP client is told the session's
mailbox in the `session/new` and `session/load` results, under
`_meta.elmee.mesh` (`root`, `name`, `incarnation`); a build without the
`agents` feature holds no mailbox and sends none.

A fresh start does not execute the previous session's pending messages: they
are retained as cancelled. A dispatch interrupted without a saved result is recorded as failed
with an unknown-execution warning, never retried automatically; a result that
was saved is recovered with its original ID.

The `mesh-v2` directory keeps this format apart from older builds that cannot
tell a request from a result; run one build across a project's agents.
