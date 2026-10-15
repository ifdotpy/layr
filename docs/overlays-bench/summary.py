#!/usr/bin/env python3
"""Print median, minimum and maximum (ms) of each experiment, size and step of bench.sh rows."""

import statistics
import sys

groups = {}
for line in sys.stdin:
    if line.startswith("#") or not line.strip():
        continue
    exp, size, _run, step, us = line.rstrip("\n").split("\t")
    groups.setdefault((exp, size, step), []).append(int(us) / 1000)
print("| experiment | size | step | runs | median ms | min ms | max ms |")
print("| --- | ---: | --- | ---: | ---: | ---: | ---: |")
for (exp, size, step), v in groups.items():
    print(f"| {exp} | {size} | {step} | {len(v)} | {statistics.median(v):.1f} | {min(v):.1f} | {max(v):.1f} |")
