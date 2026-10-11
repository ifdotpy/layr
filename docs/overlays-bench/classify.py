#!/usr/bin/env python3
"""Classify the paths of a change list by owner, as a commit with overlays would.

Usage: classify.py <dump> <runs> <name> <new state> <overlay state>...   (the top overlay first)

<dump> is the output of `btrfs receive --dump` for a `btrfs send --no-data` stream that ends in
<new state>. Folders are left out, as in a commit. The owner of a file is the highest overlay
whose state has the path; else the highest overlay with a routing pattern that matches it; else
the shared set. Prints TSV rows like bench.sh.
"""

import fnmatch
import os
import re
import sys
import time

# Routing patterns of each overlay (the same for all of them here).
PATTERNS = ["*.env", ".env.*", "secrets/*", "*.pem", "*.key"]
TOKEN = re.compile(r"(?:\\.|\S)+")
ORPHAN = re.compile(r"^o\d+-\d+-\d+$")


def unescape(s):
    s = re.sub(r"\\([0-7]{3})", lambda m: chr(int(m.group(1), 8)), s)
    return re.sub(r"\\(.)", r"\1", s)


def paths(dump):
    out = set()
    prefix = None
    with open(dump, encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            t = TOKEN.findall(line)
            if len(t) < 2:
                continue
            if prefix is None:
                prefix = t[1].rstrip("/") + "/"  # the "snapshot ./<name>" line
                continue
            found = [unescape(t[1])]
            if t[0] in ("rename", "link") and len(t) > 2 and t[2].startswith("dest="):
                found.append(unescape(t[2][5:]))
            for p in found:
                p = p[len(prefix):] if p.startswith(prefix) else p
                p = p.strip("/")
                if p and not ORPHAN.match(p):
                    out.add(p)
    return sorted(out)


def owner(p, overlays):
    for i, root in enumerate(overlays):
        try:
            os.lstat(os.path.join(root, p))
            return i
        except FileNotFoundError:
            pass
    base = os.path.basename(p)
    for i, _ in enumerate(overlays):
        if any(fnmatch.fnmatchcase(base, g) or fnmatch.fnmatchcase(p, g) for g in PATTERNS):
            return i
    return -1


def main():
    dump, runs, name, new, overlays = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5:]
    counts = {}
    for r in range(1, runs + 1):
        t0 = time.perf_counter()
        ps = paths(dump)
        us = int((time.perf_counter() - t0) * 1e6)
        print(f"parse-dump\t{name}\t{r}\ttotal\t{us}")
        t0 = time.perf_counter()
        counts = {}
        for p in ps:
            if os.path.isdir(os.path.join(new, p)):
                continue
            o = owner(p, overlays)
            counts[o] = counts.get(o, 0) + 1
        us = int((time.perf_counter() - t0) * 1e6)
        print(f"classify\t{name}\t{r}\ttotal\t{us}")
    names = ["shared" if k < 0 else overlays[k] for k in sorted(counts)]
    detail = ", ".join(f"{n}={counts[k]}" for n, k in zip(names, sorted(counts)))
    print(f"# {name}: {len(ps)} paths, files by owner: {detail}", file=sys.stderr)


main()
