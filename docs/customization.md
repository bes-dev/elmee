# Project customization

Elmee discovers project instructions from `AGENTS.md` or `CLAUDE.md` in the
working directory and its ancestors, plus `~/.elmee/AGENTS.md`. Skills are
discovered recursively from `.agents/skills/` and `.elmee/skills/`; only their
metadata enters the initial prompt and `SKILL.md` is loaded on demand.

## Custom commands and tools

Choose the smallest extension surface that fits:

- **Shell + skill** for ordinary command-line programs and reusable workflows:
  no new model-tool schema or protocol is needed. A skill is
  `NAME/SKILL.md` under `.elmee/skills/` or `.agents/skills/`, in the project
  or in your home directory.
- **MCP** for typed model-callable tools, stateful servers and remote services.

A skill is also a command. The agent picks one up on its own from the list in
its system prompt, and a person invokes the same file as `/NAME arguments` in
any frontend: the skill is sent as the prompt with `$ARGUMENTS` (and `$1`…)
filled in, a `` !`command` `` block replaced by its output, and arguments the
skill has no place for said after it. `argument-hint:` in its metadata block
tells the menu what to ask for.

## MCP

MCP servers are read from machine-owned `~/.elmee/mcp.json` and project
`.elmee/mcp.json`. User declarations take precedence. Elmee supports stdio and Streamable HTTP — and, embedded, a server the client itself is — paginated tool discovery,
environment expansion, bounded frames and timeouts.

```json
{
  "mcpServers": {
    "notes": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/me/Documents"]
    },
    "issues": {
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer ${ISSUES_TOKEN}" },
      "timeoutMs": 60000
    }
  }
}
```

Plaintext remote HTTP is refused; loopback HTTP remains available for local
servers.

## Hooks and verification

A project defines hooks in `.elmee/hooks.json`. Each is named for what it
straddles — `before_tool_call`, `after_tool_call`, `before_completion`,
`after_session` — and a name that is not one of those is refused rather than
ignored, so a misspelling cannot leave a project believing in a gate it does
not have:

```json
{
  "before_completion": [
    { "command": "cargo", "args": ["test", "--quiet"] }
  ],
  "after_session": [
    { "command": ".elmee/bin/distil" }
  ]
}
```

Hook events arrive as JSON on stdin. A non-zero exit blocks the event and sends
bounded output back to the model — except `after_session`, which is told about
something already finished and so can refuse nothing: its failure is reported
and the process leaves anyway. Its payload names the transcript rather than
summarising it, because everything a reader of it might want — how many turns,
what was edited, what it cost — is in that file already. It runs once per
session: when a headless or terminal run ends, and when an editor closes an ACP
session or the connection does. A hook that wants to outlive the process
backgrounds itself; one that does not is bounded by `ELMEE_TOOL_TIMEOUT_SECS`
(600 by default), the same room a completion check gets.

`examples/hooks/distil` is a worked one: it reads the finished transcript, and
if the session actually changed something or ran long enough to have learnt
something, spends one detached agent run writing what is worth keeping into
`.elmee/memory/`. Everything expensive sits behind a cheap test, because most
sessions teach nothing and re-reading those would cost more than the work did.

A command the model ran and that finished well is recorded against the tree
it ran on, and later writes make the record stale. That record says the command
passed, not what it tested: its level is read off the words of the command
line, so it never counts as proof. Proof is a check the harness runs itself —
a `before_completion` hook, which may say what it proves with
`"verifies": "static" | "integration" | "e2e"` — or an independent refutation
of a goal claim. `--require-verification static|integration|e2e|external`
refuses a completion the current tree has no such proof for. It refuses once
per prompt: an agent that cannot produce the proof is told to say so and is
then let go, and a one-shot run reports `verification_satisfied: false`.
