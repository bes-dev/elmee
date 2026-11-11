#!/usr/bin/env python3
"""The harnesses as long-lived servers, against the instant model.

elmee speaks ACP itself; Codex and Claude Code through their ACP
adapters; pi through its own RPC mode. Measured per harness, in one
process: time until it can take a prompt, the first turn, warm turns,
a second session, an eight-tool-call turn, and the memory it holds idle.
"""
import json, os, select, statistics, subprocess, sys, tempfile, time

B = os.path.dirname(os.path.abspath(__file__))
NPM = os.path.join(B, "npm/node_modules/.bin")
WARM = int(sys.argv[1]) if len(sys.argv) > 1 else 20
base = dict(os.environ)
AGENTS = {
    "elmee": (["elmee", "--acp", "--no-session", "-m", "stub/bench"], {"HOME": B + "/homes/elmee"}),
    "codex": ([NPM + "/codex-acp"], {"CODEX_HOME": B + "/homes/codex"}),
    "claude": ([NPM + "/claude-agent-acp"], {"CLAUDE_CONFIG_DIR": B + "/homes/claude", "ANTHROPIC_BASE_URL": "http://127.0.0.1:18801",
                                          "ANTHROPIC_API_KEY": "sk-bench", "ANTHROPIC_MODEL": "bench",
                                          "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}),
}

def tree_rss(pid):
    rows = [line.split() for line in subprocess.run(["ps", "-axo", "pid=,ppid=,rss="], capture_output=True, text=True).stdout.splitlines()]
    kids, rss = {}, {}
    for p, pp, r in rows:
        kids.setdefault(int(pp), []).append(int(p)); rss[int(p)] = int(r)
    total, stack, count = 0, [pid], 0
    while stack:
        p = stack.pop(); total += rss.get(p, 0); count += 1; stack.extend(kids.get(p, []))
    return total / 1024, count

class Lines:
    def __init__(self, argv, env, cwd):
        self.proc = subprocess.Popen(argv, env={**base, **env}, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, bufsize=0)
        self.buffer = b""
    def send(self, message):
        self.proc.stdin.write((json.dumps(message) + "\n").encode()); self.proc.stdin.flush()
    def read(self, timeout=60):
        deadline = time.time() + timeout
        while b"\n" not in self.buffer:
            ready, _, _ = select.select([self.proc.stdout], [], [], max(0, deadline - time.time()))
            if not ready: raise TimeoutError("no message")
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk: raise EOFError("agent closed")
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line) if line.strip() else self.read(timeout)
    def close(self):
        try: self.proc.stdin.close(); self.proc.wait(timeout=5)
        except Exception: self.proc.kill()

class Acp(Lines):
    next_id = 0
    def call(self, method, params, on_update=None):
        self.next_id += 1; mine = self.next_id
        self.send({"jsonrpc": "2.0", "id": mine, "method": method, "params": params})
        while True:
            message = self.read()
            if message.get("id") == mine and "method" not in message:
                if "error" in message: raise RuntimeError(f"{method}: {message['error']}")
                return message["result"]
            if "method" in message and "id" in message:           # the agent asks the client
                if message["method"] == "session/request_permission":
                    options = message["params"].get("options", [])
                    allow = next((o for o in options if "allow" in o.get("kind", "")), options[0] if options else {})
                    self.send({"jsonrpc": "2.0", "id": message["id"], "result": {"outcome": {"outcome": "selected", "optionId": allow.get("optionId")}}})
                else:
                    self.send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "not supported"}})
            elif on_update and message.get("method") == "session/update":
                on_update(message["params"]["update"])
    def prompt(self, session, text):
        start, first = time.perf_counter(), [None]
        def seen(update):
            if first[0] is None and update.get("sessionUpdate") == "agent_message_chunk":
                first[0] = time.perf_counter() - start
        result = self.call("session/prompt", {"sessionId": session, "prompt": [{"type": "text", "text": text}]}, seen)
        return (first[0] or 0) * 1000, (time.perf_counter() - start) * 1000, result.get("stopReason")

def bench_acp(name):
    argv, env = AGENTS[name]
    work = tempfile.mkdtemp(prefix=f"bench-{name}-")
    t0 = time.perf_counter()
    agent = Acp(argv, env, work)
    hello = agent.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    t_init = time.perf_counter()
    session = agent.call("session/new", {"cwd": work, "mcpServers": []})["sessionId"]
    t_ready = time.perf_counter()
    first = agent.prompt(session, "Say the answer. BENCH:text")
    warm = [agent.prompt(session, "Say the answer. BENCH:text") for _ in range(WARM)]
    steps = [agent.prompt(session, f"Do the steps. BENCH:steps8 run{name}{k}") for k in range(5)]
    lines = len(open(os.path.join(work, "bench-out.txt")).read().splitlines()) if os.path.exists(os.path.join(work, "bench-out.txt")) else 0
    t = time.perf_counter(); agent.call("session/new", {"cwd": work, "mcpServers": []}); second = (time.perf_counter() - t) * 1000
    time.sleep(1.0)
    rss, processes = tree_rss(agent.proc.pid)
    agent.close()
    caps = hello.get("agentCapabilities", {})
    return {"init_ms": (t_init - t0) * 1000, "new_ms": (t_ready - t_init) * 1000, "ready_ms": (t_ready - t0) * 1000,
            "first_turn_ms": first[1], "warm_first_chunk_ms": statistics.median(w[0] for w in warm),
            "warm_turn_ms": statistics.median(w[1] for w in warm), "warm_p90_ms": sorted(w[1] for w in warm)[int(WARM * .9) - 1],
            "steps8_ms": statistics.median(s[1] for s in steps), "steps_ok": lines == 40 and all(s[2] == "end_turn" for s in steps),
            "second_new_ms": second, "idle_rss_mb": rss, "processes": processes,
            "protocol": hello.get("protocolVersion"), "capabilities": caps, "auth": [m.get("id") for m in hello.get("authMethods", [])]}

def bench_pi():
    work = tempfile.mkdtemp(prefix="bench-pi-")
    t0 = time.perf_counter()
    agent = Lines(["pi", "--mode", "rpc", "--no-session", "--provider", "stub", "--model", "bench"],
                  {"HOME": B + "/homes/pi", "PI_OFFLINE": "1"}, work)
    def prompt(text):
        start, first = time.perf_counter(), None
        agent.send({"type": "prompt", "message": text})
        while True:
            event = agent.read()
            if first is None and event.get("type") == "message_update" and (event.get("assistantMessageEvent") or {}).get("type") == "text_delta":
                first = time.perf_counter() - start
            if event.get("type") == "agent_end":
                return (first or 0) * 1000, (time.perf_counter() - start) * 1000, "end_turn"
    first = prompt("Say the answer. BENCH:text")
    ready = (time.perf_counter() - t0) * 1000 - first[1]
    warm = [prompt("Say the answer. BENCH:text") for _ in range(WARM)]
    steps = [prompt(f"Do the steps. BENCH:steps8 runpi{k}") for k in range(5)]
    lines = len(open(os.path.join(work, "bench-out.txt")).read().splitlines()) if os.path.exists(os.path.join(work, "bench-out.txt")) else 0
    time.sleep(1.0)
    rss, processes = tree_rss(agent.proc.pid)
    agent.close()
    return {"init_ms": None, "new_ms": None, "ready_ms": None, "first_turn_ms": (time.perf_counter() - t0) * 0 + first[1],
            "cold_to_first_answer_ms": ready + first[1],
            "warm_first_chunk_ms": statistics.median(w[0] for w in warm), "warm_turn_ms": statistics.median(w[1] for w in warm),
            "warm_p90_ms": sorted(w[1] for w in warm)[int(WARM * .9) - 1], "steps8_ms": statistics.median(s[1] for s in steps),
            "steps_ok": lines == 40, "second_new_ms": None, "idle_rss_mb": rss, "processes": processes, "protocol": "pi-rpc"}

out = {}
for name in ["elmee", "codex", "claude"]:
    try: out[name] = bench_acp(name)
    except Exception as error: out[name] = {"error": repr(error)}
    print(name, json.dumps({k: (round(v, 1) if isinstance(v, float) else v) for k, v in out[name].items() if k != "capabilities"}), flush=True)
try: out["pi"] = bench_pi()
except Exception as error: out["pi"] = {"error": repr(error)}
print("pi", json.dumps({k: (round(v, 1) if isinstance(v, float) else v) for k, v in out["pi"].items()}), flush=True)
json.dump(out, open(os.path.join(B, "embedded.json"), "w"), indent=1)
