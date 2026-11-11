#!/usr/bin/env python3
"""Sit between a harness and the real model, and write down each request:
when it came, how large it was, how long the model took, what it charged."""
import http.client, http.server, json, re, sys, threading, time

PORT, LOG = int(sys.argv[1]), sys.argv[2]
UPSTREAM = ("127.0.0.1", int(sys.argv[3]) if len(sys.argv) > 3 else 8000)
lock = threading.Lock()

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def forward(self, body=None):
        started = time.time()
        upstream = http.client.HTTPConnection(*UPSTREAM, timeout=900)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection", "accept-encoding")}
        upstream.request(self.command, self.path, body=body, headers=headers)
        response = upstream.getresponse()
        self.send_response(response.status)
        for key, value in response.getheaders():
            if key.lower() not in ("transfer-encoding", "connection", "content-length"):
                self.send_header(key, value)
        self.send_header("connection", "close"); self.end_headers()
        first, tail, size = None, b"", 0
        while True:
            chunk = response.read1(65536)
            if not chunk: break
            if first is None: first = time.time()
            size += len(chunk); tail = (tail + chunk)[-6000:]
            try: self.wfile.write(chunk); self.wfile.flush()
            except Exception: break
        upstream.close(); self.close_connection = True
        if body is not None and not self.path.endswith("count_tokens"):
            text = tail.decode("utf-8", "replace")
            grab = lambda name: (lambda found: int(found[-1]) if found else None)(re.findall(rf'"{name}":\s*(\d+)', text))
            tools = 0
            try: tools = len(json.loads(body).get("tools") or [])
            except Exception: pass
            with lock:
                open(LOG, "a").write(json.dumps({
                    "t": started, "path": self.path.split("?")[0], "bytes": len(body), "n_tools": tools,
                    "ttfb": (first or time.time()) - started, "took": time.time() - started, "status": response.status,
                    "in": grab("prompt_tokens") or grab("input_tokens"), "out": grab("completion_tokens") or grab("output_tokens"),
                    "cached": grab("cached_tokens") or grab("cache_read_input_tokens")}) + "\n")
    def do_GET(self): self.forward()
    def do_POST(self): self.forward(self.rfile.read(int(self.headers.get("content-length", 0))))

class S(http.server.ThreadingHTTPServer):
    daemon_threads = True
S(("127.0.0.1", PORT), H).serve_forever()
