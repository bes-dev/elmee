# Models and configuration

Provider transports and credential sources are machine-owned in
`~/.elmee/models.json`. Project `.elmee/models.json` files may select a default
and add model IDs/context-window metadata for an already-known provider, but
cannot redirect credentials or declare a new transport.

Listing models is a decision about which ones you want, so a provider that
lists any offers exactly those. A provider that lists none is asked what it
serves — which is the only way a local runtime whose models change weekly stays
usable, and what would otherwise bury three useful models under a router's
four hundred. `"discover": true` merges an endpoint's own catalogue into a
declared list; `"discover": false` refuses to ask even when nothing is
declared. Declaring a model is also how it gets a `contextWindow`, which is
what turns the compaction threshold from a guess into the model's capacity.

```json
{
  "defaultModel": "vllm/qwen3-coder-32b",
  "providers": {
    "vllm": {
      "baseUrl": "http://gpu-box:8000/v1",
      "api": "openai-completions",
      "apiKeyEnv": "VLLM_KEY",
      "models": [
        { "id": "qwen3-coder-32b", "contextWindow": 262144 }
      ]
    }
  },
  "modelAliases": {
    "research": "ollama/qwen3-coder",
    "review": [
      "anthropic/claude-opus-5",
      "codex/gpt-5.6-terra"
    ]
  }
}
```

Supported provider protocol values are `openai-completions`,
`openai-responses` and `anthropic-messages`; Codex uses the Responses adapter
with subscription authentication. Endpoints are declared, never guessed by
probing.

Model resolution order is:

1. `-m` / `--model`;
2. `ELMEE_MODEL`;
3. project then machine `defaultModel`;
4. the model remembered from the previous run.

Relevant credentials are read from `ELMEE_API_KEY`, `OPENAI_API_KEY` and
`ANTHROPIC_API_KEY`, or from the environment variable named by `apiKeyEnv`.
`elmee --list-models` prints resolvable specs: every model declared in
`models.json` plus whatever each reachable endpoint reports it serves; the
endpoints are asked at once and each has three seconds to answer. No model
names are compiled into the binary — a subscription's catalogue is the list,
so a model that appeared this week needs no release here.

Endpoints that state a model's context window are believed, and what they said
is kept in `~/.elmee/model-catalogue.json` so the next start knows it without a
request: startup never needs the network. That number decides the compaction
threshold, so it is taken from the most authoritative source that has one —
`models.json` if this machine wrote one down, then the endpoint's own word.

## Local models

Everything a request carries by default is sized to the model's window rather
than fixed: the listing of skills, the most one tool result may add, and when
old output starts being shortened. A window of 8k tokens and one of 200k run
the same agent. Three things make that work without configuration:

- An endpoint that reports its window (`context_window`, `context_length`,
  `max_context_length` or `max_model_len` in `/models`) is believed.
- A local endpoint that reports none is assumed to have 32,000 tokens, and
  says so at start. Declare `contextWindow` to use more — a runtime that
  truncates silently instead of refusing would otherwise cut the system prompt
  off the front.
- A request refused as too large for the context is answered by making room
  and asking again, so a window smaller than assumed corrects itself.

A project narrows what every agent in it starts with by keeping a role called
`default` in `.elmee/roles/default.md`: its `skills:` and `tools:` lists are
what is offered when no `--role` is named, which is how skills installed for
other work stay out of this project's prompt.
