#!/usr/bin/env python3
"""Medians of results.jsonl: one column per engine. usage: table.py results.jsonl"""
import json, statistics, sys

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
engines = ["v2-hot", "v2-cold", "v3"]
by = {e: [r for r in rows if r["engine"] == e] for e in engines}


def med(e, f):
    v = [f(r) for r in by[e]]
    return statistics.median(v) if v else None


steps = [
    ("first write (ms)", lambda r: r["init_ms"]),
    ("save of 10 files (ms, median of 10)", lambda r: statistics.median(r["save_ms"])),
    ("diff first to last state (ms)", lambda r: r["diff_ms"]),
    ("read all files, loose (ms)", lambda r: r["read_all_ms"][1]),
    ("checkout, loose (ms)", lambda r: r["checkout_ms"]),
    ("pack (ms)", lambda r: r["pack_ms"]),
    ("read all files, packed (ms)", lambda r: r["read_all_ms_packed"][1]),
    ("checkout, packed (ms)", lambda r: r["checkout_packed_ms"]),
    ("store before pack, allocated (MB)", lambda r: r["store_bytes"]["allocated"] / 1e6),
    ("store after pack, allocated (MB)", lambda r: r["packed_bytes"]["allocated"] / 1e6),
]
print(f"trials: " + ", ".join(f"{e} {len(by[e])}" for e in engines))
print("| step | " + " | ".join(engines) + " | v2-hot + git (today) |")
print("|---|" + "---:|" * (len(engines) + 1))
for name, f in steps:
    vals = [med(e, f) for e in engines]
    today = ""
    if vals[0] is not None and vals[2] is not None and ("write" in name or "save" in name or "MB" in name or name.startswith("pack")):
        today = f"{vals[0] + vals[2]:,.0f}"
    print(f"| {name} | " + " | ".join("" if v is None else f"{v:,.0f}" for v in vals) + f" | {today} |")
