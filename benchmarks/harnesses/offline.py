#!/usr/bin/env python3
"""Harness-only costs: every harness against the same instant model.

Runs are interleaved in random order so drift on the machine lands on
everyone alike. For each run: wall time, CPU time and peak memory from
/usr/bin/time, and from the stub's log when the first request arrived
and how large each request was.
"""
import json, os, random, re, statistics, subprocess, sys, tempfile, time

B = os.path.dirname(os.path.abspath(__file__))
LOG = os.path.join(B, "log.jsonl")
HARNESSES = ["elmee", "pi", "codex", "claude"]
VERSION = {"elmee": ["elmee", "--version"], "pi": ["pi", "--version"],
           "codex": ["codex", "--version"], "claude": ["claude", "--version"]}
N = int(sys.argv[1]) if len(sys.argv) > 1 else 15

def timed(argv, cwd, env=None):
    start = time.time()
    done = subprocess.run(["/usr/bin/time", "-l"] + argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, timeout=120)
    wall = time.time() - start
    err = done.stderr
    grab = lambda pattern: float(re.search(pattern, err).group(1)) if re.search(pattern, err) else None
    return {"start": start, "wall": wall, "rc": done.returncode,
            "user": grab(r"([\d.]+) user"), "sys": grab(r"([\d.]+) sys"),
            "rss": grab(r"(\d+)\s+maximum resident set size"), "out": done.stdout.strip()[-80:]}

def turn(harness, prompt):
    work = tempfile.mkdtemp(prefix=f"bench-{harness}-")
    open(LOG, "w").close()
    run = timed([os.path.join(B, "run.sh"), harness, prompt], work)
    requests = [json.loads(line) for line in open(LOG)]
    run["requests"] = len(requests)
    if requests:
        run["first_request"] = requests[0]["t"] - run["start"]
        run["after_last"] = run["start"] + run["wall"] - requests[-1]["t"]
        run["bytes"] = requests[0]["bytes"]
        run["last_bytes"] = requests[-1]["bytes"]
    produced = os.path.join(work, "bench-out.txt")
    run["lines"] = len(open(produced).read().splitlines()) if os.path.exists(produced) else 0
    subprocess.run(["rm", "-rf", work])
    return run

results = {h: {"version": [], "text": [], "steps8": []} for h in HARNESSES}
for h in HARNESSES:                       # one unmeasured run each: page cache, first-run setup
    turn(h, "warm up. BENCH:text")
jobs = [(h, kind) for h in HARNESSES for kind in ("version", "text", "steps8")] * N
random.Random(7).shuffle(jobs)
for index, (h, kind) in enumerate(jobs):
    if kind == "version":
        results[h][kind].append(timed(VERSION[h], B))
    elif kind == "text":
        results[h][kind].append(turn(h, "Say the answer. BENCH:text"))
    else:
        results[h][kind].append(turn(h, f"Do the steps. BENCH:steps8 run{index}"))
    print(f"\r{index + 1}/{len(jobs)}", end="", file=sys.stderr)
json.dump(results, open(os.path.join(B, "offline.json"), "w"))

med = lambda rows, key: statistics.median(r[key] for r in rows if r.get(key) is not None)
p90 = lambda rows, key: sorted(r[key] for r in rows if r.get(key) is not None)[int(len(rows) * 0.9) - 1]
print(f"\n\nN = {N} per cell, medians (p90 in brackets), ms unless said\n")
print(f"{'':8}{'--version':>12}{'text turn':>16}{'to 1st req':>12}{'after last':>12}{'8-step turn':>16}{'per step':>10}{'CPU text':>10}{'RSS MB':>8}{'req KB':>8}{'ok':>5}")
for h in HARNESSES:
    r = results[h]
    text, steps = med(r["text"], "wall") * 1000, med(r["steps8"], "wall") * 1000
    ok = sum(1 for x in r["steps8"] if x["lines"] == 8 and x["requests"] == 9 and x["rc"] == 0)
    print(f"{h:8}{med(r['version'], 'wall') * 1000:>12.0f}"
          f"{text:>9.0f} [{p90(r['text'], 'wall') * 1000:>4.0f}]"
          f"{med(r['text'], 'first_request') * 1000:>12.0f}{med(r['text'], 'after_last') * 1000:>12.0f}"
          f"{steps:>9.0f} [{p90(r['steps8'], 'wall') * 1000:>4.0f}]"
          f"{(steps - text) / 8:>10.1f}"
          f"{(med(r['text'], 'user') + med(r['text'], 'sys')) * 1000:>10.0f}"
          f"{med(r['text'], 'rss') / 1048576:>8.0f}{med(r['text'], 'bytes') / 1024:>8.1f}{ok:>3}/{N}")
