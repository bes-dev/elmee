# Security

## Supported versions

Until 1.0, only the newest tagged release receives security fixes.

## The boundary is the operating system

Elmee has no permission model and no sandbox. A command the model runs has the
authority of the account that started elmee — the same authority that account's
own shell has. Treat running elmee as handing that account's shell to a model.

This is deliberate. A boundary a harness draws around itself is one the harness
can also draw wrongly, and ours did: it refused writes the operator had
explicitly allowed and reported them as the operating system's doing. It also
never contained a determined model, because reads and the network stayed open by
necessity — a coding agent that cannot read a toolchain or fetch a dependency is
one nobody keeps switched on.

So the boundary is the one that was always real:

- **an operating-system account** with only the credentials the work needs;
- **a container or VM**, which is also how to run anything unattended;
- **the project directory itself**, if you run elmee from a checkout you are
  willing to lose.

None of that is configured inside elmee, because none of it is elmee's to
enforce.

## What elmee does give you

**Undo rather than prevention.** `plugins/snapshot` checkpoints the working tree
before a turn and restores it after. It records state rather than intercepting
calls, so `rm`, `sed -i`, an installer and a subagent's writes are all covered.
Restoring puts changed files back and removes files created since, and records
the state it leaves first, so a rewind is itself something to rewind from. What
it does not cover: files git ignores, anything outside the working directory,
and effects that are not files — a pushed commit, a sent request. This is the
mechanism to reach for when a turn goes wrong, not a boundary.

**Honest provenance.** Web pages, tool output, MCP descriptions, file contents
and messages from other agents are labelled as untrusted where they enter the
model's context, and the system prompt says that what is read is data rather
than instruction. That is a statement about where text came from, not a
restriction on what the agent may do with it.

**A durable record.** Sessions, evidence and turn reports are written as they
happen, so what an agent did is reconstructable afterwards even when the process
died mid-turn.

## Where a guard attaches

Removing the policy did not remove the place to put one, and the distinction
matters: what proved unworkable was *asking a person mid-turn*, not the idea of
a decision point. An agent that has to stop and be confirmed cannot be given a
standing objective and left alone, which is most of what an agent is for. A
guard that decides by itself has no such problem — and a good one is a project
of its own, not a few hundred lines inside a harness that also has to write
code.

So elmee keeps the attachment point and declines to fill it:

**`before_tool_call` hooks refuse a call.** A project's `.elmee/hooks.json` runs a
command before every tool call; a non-zero exit blocks it and the reason goes
back to the model. The hook sees the call as JSON on stdin and decides however
it likes — a policy engine, a classifier, a list of paths, another model. This
is where an external guard belongs, and it is deliberately not a policy itself:
elmee ships none, and the default is that every call proceeds.

**`before_completion` hooks refuse an ending.** The same mechanism on the other end,
which is how a project makes "the tests pass" a condition of being done rather
than a claim the model makes.

**Snapshots make a wrong action survivable** rather than impossible, which is
the other half of the same posture: `plugins/snapshot` records tree state before
a turn, so recovery does not depend on having predicted the action.

Anything stronger — confinement, syscall filtering, network policy — is the
operating system's, for the reasons above. If you build a guard, build it
outside and hang it on `before_tool_call`; do not send a patch that puts a policy
back in the harness.

## What this means in practice

- Do not run elmee as root, and do not run it in an account holding credentials
  the task does not need.
- For anything unattended, put the whole process in a container. Isolating each
  command from inside the harness is the thing that did not work.
- Assume any file the agent can read may end up in a model request, and any file
  it can write may be changed. Both follow from the account it runs as.
- Prompt injection is a real risk and no policy here defends against it: content
  is labelled, and the model is told to treat it as data. Where hostile content
  meets consequential actions, put a person between them.

## Reporting a vulnerability

Use the repository's private **Security → Report a vulnerability** flow. Do not
put credentials, exploit details, or private transcripts in an ordinary issue.
Include the affected version, platform, reproduction, and the boundary you
expected to hold — noting that the expected boundary is the OS account, not a
setting inside elmee.
