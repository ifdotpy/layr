#!/usr/bin/env python3
"""Medians of results.jsonl (bench.sh): one row per step, one column per kind."""
import json, statistics, sys

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
kinds = ["plain", "classic", "fast"]
steps, data = [], {}
for r in rows:
    if r["step"] not in steps:
        steps.append(r["step"])
    data.setdefault((r["step"], r["kind"]), []).append(r["ms"])
print("| Step | " + " | ".join(kinds) + " |")
print("|---|" + "---:|" * len(kinds))
for s in steps:
    cells = []
    for k in kinds:
        v = data.get((s, k))
        cells.append(f"{statistics.median(v):.0f} ({min(v)} to {max(v)}, n={len(v)})" if v else "")
    print(f"| {s} | " + " | ".join(cells) + " |")
loads = [r["load"] for r in rows]
print(f"\nload average: {min(loads)} to {max(loads)}; rounds: {max(r['round'] for r in rows)}")
