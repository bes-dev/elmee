#!/usr/bin/env python3
"""The same local model behind every harness, three tasks, checked results.

Per harness and task: one cold run, then warm ones back to back, so the
model's prompt cache is in the state a person working in one harness
would have it. The proxy's log says how many requests a run made, how
many tokens it sent and how long the model took.
"""
import hashlib, json, os, re, subprocess, sys, tempfile, time

B = os.path.dirname(os.path.abspath(__file__))
LOG = os.path.join(B, "live.jsonl")
RUNS = int(sys.argv[1]) if len(sys.argv) > 1 else 3
HARNESSES = sys.argv[2].split(",") if len(sys.argv) > 2 else ["pi", "elmee", "codex", "claude"]
TEST = "from calc import mean\nassert mean([2, 4, 6]) == 4, mean([2, 4, 6])\nassert mean([5]) == 5\nprint('OK')\n"

def setup(task, work):
    if task == "read":
        open(os.path.join(work, "data.txt"), "w").write("".join(f"item-{n}: value-{n * 7919 % 1000}\n" for n in range(1, 41)))
    if task == "fix":
        open(os.path.join(work, "calc.py"), "w").write("def mean(xs):\n    return sum(xs) / (len(xs) - 1)\n")
        open(os.path.join(work, "test_calc.py"), "w").write(TEST)

PROMPT = {
    "pong": "Reply with exactly one word: pong",
    "read": "What is the value of item-17 in data.txt? Reply with just the value.",
    "fix": "The test in test_calc.py fails. Fix calc.py so that `python3 test_calc.py` passes, then confirm it passes.",
}

def passed(task, work, out):
    if task == "pong": return "pong" in out.lower()
    if task == "read": return f"value-{17 * 7919 % 1000}" in out
    same = open(os.path.join(work, "test_calc.py")).read() == TEST
    ran = subprocess.run([sys.executable, "test_calc.py"], cwd=work, capture_output=True, text=True)
    return same and ran.returncode == 0 and "OK" in ran.stdout

rows = []
for task in ["pong", "read", "fix"]:
    for harness in HARNESSES:
        for run in range(RUNS):
            work = tempfile.mkdtemp(prefix=f"live-{harness}-")
            setup(task, work)
            open(LOG, "w").close()
            start = time.time()
            try:
                done = subprocess.run([os.path.join(B, "live.sh"), harness, PROMPT[task]], cwd=work, stdin=subprocess.DEVNULL,
                                      capture_output=True, text=True, timeout=900)
                out, rc = done.stdout, done.returncode
            except subprocess.TimeoutExpired:
                out, rc = "", -1
            wall = time.time() - start
            requests = [json.loads(line) for line in open(LOG)]
            row = {"task": task, "harness": harness, "run": run, "wall": wall, "rc": rc, "ok": rc == 0 and passed(task, work, out),
                   "requests": len(requests), "model_s": sum(r["took"] for r in requests),
                   "in": sum(r["in"] or 0 for r in requests), "out": sum(r["out"] or 0 for r in requests),
                   "cached": sum(r["cached"] or 0 for r in requests), "first_bytes": requests[0]["bytes"] if requests else 0,
                   "errors": sum(1 for r in requests if r["status"] != 200)}
            rows.append(row)
            print(json.dumps(row), flush=True)
            json.dump(rows, open(os.path.join(B, f"live-{'-'.join(HARNESSES)}.json"), "w"))
            subprocess.run(["rm", "-rf", work])
