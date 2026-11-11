#!/usr/bin/env python3
"""A model that answers instantly, in three dialects, and writes down what it was asked.

Chat Completions (elmee, pi), Responses (codex) and Anthropic Messages
(claude). A prompt containing BENCH:steps<N> gets N shell tool calls and
then a final answer; anything else gets one line of text. Every request
is appended to the log as JSON: when it arrived, how large it was, how
many tools and how much system prompt it carried.
"""
import http.server, json, re, sys, time, threading

PORT, LOG = int(sys.argv[1]), sys.argv[2]
DELAY = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0
lock = threading.Lock()
SHELLS = ("exec", "bash", "Bash", "shell", "shell_command", "exec_command", "local_shell", "container.exec")

def text_of(content):
    if isinstance(content, str): return content
    if isinstance(content, list):
        return "\n".join(p.get("text", "") if isinstance(p, dict) else str(p) for p in content)
    return ""

def describe(path, body, raw):
    tools = body.get("tools") or []
    names = [t.get("name") or (t.get("function") or {}).get("name") or t.get("type") for t in tools]
    if path.endswith("/messages"):
        system = text_of(body.get("system"))
        msgs = body.get("messages", [])
        is_result = lambda m: isinstance(m.get("content"), list) and any(isinstance(p, dict) and p.get("type") == "tool_result" for p in m["content"])
        all_user = [text_of(m.get("content")) for m in msgs if m.get("role") == "user"]
        asked = [i for i, m in enumerate(msgs) if m.get("role") == "user" and not is_result(m) and "BENCH:" in text_of(m.get("content"))]
        user = [text_of(msgs[i].get("content")) for i in asked[-1:]]
        results = sum(1 for m in msgs[asked[-1] if asked else 0:] if isinstance(m.get("content"), list)
                      for p in m["content"] if isinstance(p, dict) and p.get("type") == "tool_result")
    elif path.endswith("/responses"):
        system = body.get("instructions") or ""
        items = body.get("input") if isinstance(body.get("input"), list) else []
        system += "".join(text_of(i.get("content")) for i in items if i.get("role") in ("system", "developer"))
        all_user = [text_of(i.get("content")) for i in items if i.get("role") == "user"]
        asked = [n for n, i in enumerate(items) if i.get("role") == "user" and "BENCH:" in text_of(i.get("content"))]
        user = [text_of(items[n].get("content")) for n in asked[-1:]]
        results = sum(1 for i in items[asked[-1] if asked else 0:] if i.get("type") in ("function_call_output", "local_shell_call_output", "custom_tool_call_output"))
    else:
        msgs = body.get("messages", [])
        system = "".join(text_of(m.get("content")) for m in msgs if m.get("role") in ("system", "developer"))
        all_user = [text_of(m.get("content")) for m in msgs if m.get("role") == "user"]
        asked = [i for i, m in enumerate(msgs) if m.get("role") == "user" and "BENCH:" in text_of(m.get("content"))]
        user = [text_of(msgs[i].get("content")) for i in asked[-1:]]
        results = sum(1 for m in msgs[asked[-1] if asked else 0:] if m.get("role") == "tool")
    every = " ".join(t for t in all_user)
    found = re.findall(r"BENCH:[a-z]+\d*(?: run\w+)?", every)
    return {"marker": found[-1] if found else "", "t": time.time(), "path": path, "bytes": len(raw), "n_tools": len(tools), "tools": names,
            "tools_bytes": len(json.dumps(tools)), "system_chars": len(system), "results": results,
            "model": body.get("model"), "stream": bool(body.get("stream")),
            "max_tokens": body.get("max_tokens") or body.get("max_output_tokens"), "user": user}

served = {}

def plan(info):
    """What to answer: a shell call while the marked prompt still has steps to go, then text.
    Steps are counted here, per marked prompt, because each harness lays a turn's
    tool results out differently and the count must not depend on that."""
    marker = info["marker"]
    steps = int(re.search(r"steps(\d+)", marker).group(1)) if "steps" in marker else 0
    shell = next((n for n in info["tools"] if n in SHELLS), None)
    if shell and steps:
        with lock:
            served[marker] = served.get(marker, 0) + 1
            step = served[marker]
        if step <= steps:
            return "shell", shell, f"echo step-{step} >> bench-out.txt; echo done-{step}"
    return "text", None, "The answer is forty-two."

def shell_args(name, command):
    if name == "shell": return {"command": ["bash", "-lc", command]}
    if name == "exec_command": return {"cmd": command}
    return {"command": command}

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        out = json.dumps({"object": "list", "data": [{"id": "bench", "object": "model", "context_length": 200000}]}).encode()
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out))); self.end_headers(); self.wfile.write(out)
    def sse_start(self):
        self.send_response(200); self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache"); self.send_header("connection", "close"); self.end_headers()
    def ev(self, data, event=None):
        if event: self.wfile.write(f"event: {event}\n".encode())
        self.wfile.write(f"data: {data if isinstance(data, str) else json.dumps(data)}\n\n".encode()); self.wfile.flush()
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        path = self.path.split("?")[0]
        try: body = json.loads(raw or b"{}")
        except Exception: body = {}
        if path.endswith("count_tokens"):
            out = json.dumps({"input_tokens": len(raw) // 4}).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(out))); self.end_headers(); self.wfile.write(out); return
        info = describe(path, body, raw)
        if path.endswith("/messages") and info["n_tools"]: open(LOG + ".last", "wb").write(raw)
        with lock:
            open(LOG, "a").write(json.dumps(info) + "\n")
        kind, tool, payload = plan(info)
        if DELAY: time.sleep(DELAY)
        usage_in, usage_out = len(raw) // 4, 12
        if path.endswith("/messages"): self.anthropic(body, kind, tool, payload, usage_in, usage_out)
        elif path.endswith("/responses"): self.responses_api(body, kind, tool, payload, usage_in, usage_out)
        else: self.chat(body, kind, tool, payload, usage_in, usage_out)
        self.close_connection = True

    def chat(self, body, kind, tool, payload, i, o):
        self.sse_start()
        base = {"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "bench"}
        if kind == "text":
            self.ev({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": payload}, "finish_reason": None}]})
            fin = "stop"
        else:
            call = {"index": 0, "id": f"call_{time.time_ns()}", "type": "function",
                    "function": {"name": tool, "arguments": json.dumps(shell_args(tool, payload))}}
            self.ev({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [call]}, "finish_reason": None}]})
            fin = "tool_calls"
        self.ev({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": fin}],
                 "usage": {"prompt_tokens": i, "completion_tokens": o, "total_tokens": i + o}})
        self.ev("[DONE]")

    def responses_api(self, body, kind, tool, payload, i, o):
        self.sse_start()
        rid = f"resp_{time.time_ns()}"
        resp = {"id": rid, "object": "response", "status": "in_progress", "model": "bench", "output": []}
        self.ev({"type": "response.created", "response": resp}, "response.created")
        if kind == "text":
            item = {"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": payload, "annotations": []}]}
            self.ev({"type": "response.output_item.added", "output_index": 0, "item": {**item, "content": [], "status": "in_progress"}}, "response.output_item.added")
            self.ev({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": payload}, "response.output_text.delta")
        elif tool == "local_shell":
            item = {"id": "ls_1", "type": "local_shell_call", "call_id": f"call_{time.time_ns()}", "status": "completed",
                    "action": {"type": "exec", "command": ["bash", "-lc", payload]}}
            self.ev({"type": "response.output_item.added", "output_index": 0, "item": item}, "response.output_item.added")
        else:
            item = {"id": "fc_1", "type": "function_call", "call_id": f"call_{time.time_ns()}", "name": tool,
                    "arguments": json.dumps(shell_args(tool, payload)), "status": "completed"}
            self.ev({"type": "response.output_item.added", "output_index": 0, "item": {**item, "arguments": ""}}, "response.output_item.added")
            self.ev({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0, "delta": item["arguments"]}, "response.function_call_arguments.delta")
        self.ev({"type": "response.output_item.done", "output_index": 0, "item": item}, "response.output_item.done")
        done = {**resp, "status": "completed", "output": [item],
                "usage": {"input_tokens": i, "output_tokens": o, "total_tokens": i + o,
                          "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}
        self.ev({"type": "response.completed", "response": done}, "response.completed")

    def anthropic(self, body, kind, tool, payload, i, o):
        if not body.get("stream"):
            content = [{"type": "text", "text": payload}] if kind == "text" else [{"type": "tool_use", "id": f"toolu_{time.time_ns()}", "name": tool, "input": shell_args(tool, payload)}]
            out = json.dumps({"id": "msg_1", "type": "message", "role": "assistant", "model": "bench", "content": content,
                              "stop_reason": "end_turn" if kind == "text" else "tool_use", "stop_sequence": None,
                              "usage": {"input_tokens": i, "output_tokens": o}}).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(out))); self.end_headers(); self.wfile.write(out); return
        self.sse_start()
        self.ev({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "bench",
                 "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": i, "output_tokens": 0}}}, "message_start")
        if kind == "text":
            self.ev({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}, "content_block_start")
            self.ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": payload}}, "content_block_delta")
            stop = "end_turn"
        else:
            self.ev({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": f"toolu_{time.time_ns()}", "name": tool, "input": {}}}, "content_block_start")
            self.ev({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json.dumps(shell_args(tool, payload))}}, "content_block_delta")
            stop = "tool_use"
        self.ev({"type": "content_block_stop", "index": 0}, "content_block_stop")
        self.ev({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": o}}, "message_delta")
        self.ev({"type": "message_stop"}, "message_stop")

class S(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 64
S(("127.0.0.1", PORT), H).serve_forever()
