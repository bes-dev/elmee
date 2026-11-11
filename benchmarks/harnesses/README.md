# Harness comparison: elmee, Codex, Claude Code, pi

Three layers, each answering a different question.

- `offline.py` — what the harness itself costs. Every harness runs headless
  against `stub.py`, a model that answers instantly in three API dialects and
  records each request. Measures process start, one text turn, an eight-tool-call
  turn, CPU, peak memory and request size. Runs are interleaved in random order.
- `embedded.py` — the harness as a long-lived server: elmee's own ACP, Codex and
  Claude Code through `@agentclientprotocol/codex-acp` and `claude-agent-acp`,
  pi through `--mode rpc`. Measures readiness, warm turns, a second session, the
  tool loop and idle memory of the process tree.
- `live.py` — the same local model behind all four through `proxy.py`, which
  records requests, tokens, cache hits and model time. Three tasks with checked
  results; one cold run and then warm ones per harness.

Each harness runs from an isolated home with a stock configuration, so the
user's skills, MCP servers and settings are not part of the measurement.

    python3 stub.py 18801 log.jsonl &          # then: python3 offline.py 15
    npm install --prefix npm @agentclientprotocol/codex-acp @agentclientprotocol/claude-agent-acp
    python3 embedded.py 20
    python3 proxy.py 18802 live.jsonl 8000 &   # 8000: the local model server
    python3 live.py 3

`homes/` (generated provider configs) and `npm/` are not kept here; the configs
are the few lines each script's harness needs to reach port 18801 or 18802.
Results of the run of 2026-11-10 are in `results/`.
