#!/usr/bin/env python3
"""Search parity: run every eval query through the Swift CLI and the Rust CLI on the SAME Swift-built index
(baseline/index) and compare top-10 paths (order) and scores. Usage: scripts/compare-swift-search.py [limit]"""
import json, subprocess, sys, os, glob
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
swift = f"{root}/baseline/DigUp.app/Contents/Helpers/digup"
rust = f"{root}/target/release/digup"
index = f"{root}/baseline/index"
env = dict(os.environ, DIGUP_MODELS=f"{root}/models")
limit = int(sys.argv[1]) if len(sys.argv) > 1 else 10**9

def run(exe, q, kinds=None):
    cmd = [exe, "search", "--index", index, "--json", "-n", "10"]
    if kinds: cmd += ["--kinds", kinds]
    cmd += q.split()
    r = subprocess.run(cmd, capture_output=True, text=True, env=env)
    out = r.stdout
    try: return json.loads(out[out.index("["):])
    except Exception: return None

total = same_order = same_set = top1 = 0
worst = []
queries = []
for f in sorted(glob.glob(f"{root}/reference/digup-swift/evals/*.json")):
    for q in json.load(open(f))["queries"]:
        queries.append((q["q"], q.get("kind")))
for q, kind in queries[:limit]:
    a, b = run(swift, q, kind), run(rust, q, kind)
    if a is None or b is None:
        print("SKIP (no JSON)", q); continue
    total += 1
    pa, pb = [h["path"] for h in a], [h["path"] for h in b]
    same_order += pa == pb
    same_set += set(pa) == set(pb)
    top1 += (pa[:1] == pb[:1])
    if pa != pb:
        d = max((abs(x["score"] - y["score"]) for x, y in zip(a, b) if x["path"] == y["path"]), default=0)
        worst.append((q, d, pa[:3], pb[:3]))
print(f"queries {total}: identical order {same_order}, same top-10 set {same_set}, same top-1 {top1}")
for q, d, pa, pb in worst[:15]:
    print(f"- {q!r}: max score diff {d:.4f}\n    swift {[os.path.basename(x) for x in pa]}\n    rust  {[os.path.basename(x) for x in pb]}")
