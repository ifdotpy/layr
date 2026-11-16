#!/usr/bin/env python3
"""Enforce workspace edges and transitive leaf dependencies over all manifest targets/features."""
import json
import re
from pathlib import Path
import subprocess
import sys

m = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version=1", "--all-features"]))
policy = m["metadata"]["layers"]
packages = {p["id"]: p for p in m["packages"]}
workspace = [packages[i] for i in m["workspace_members"]]
names = {p["name"] for p in workspace}
if set(policy) != names:
    sys.exit(f"layer declarations differ from workspace: {set(policy) ^ names}")
errors = []
for p in workspace:
    rule = policy[p["name"]]
    normal_allowlist = rule.get("normal-dependencies")
    if normal_allowlist is not None:
        actual = {d["name"] for d in p["dependencies"] if d["kind"] != "dev"}
        unexpected = actual - set(normal_allowlist)
        if unexpected:
            errors.append(f'{p["name"]}: unlisted normal/build dependencies {unexpected}')
    for d in p["dependencies"]:  # Includes optional, target-specific, build and dev edges.
        if d["name"] in names:
            if d["name"] not in rule["dependencies"] or policy[d["name"]]["layer"] > rule["layer"]:
                errors.append(f'{p["name"]} -> {d["name"]}: unlisted or upward edge')
    if set(rule["dependencies"]) - names:
        errors.append(f'{p["name"]}: unknown allowed dependencies {set(rule["dependencies"]) - names}')

nodes = {n["id"]: n for n in m["resolve"]["nodes"]}
heavy = {"anyhow", "tokio", "clap", "rayon", "reqwest", "hyper", "image", "rusqlite", "age", "ssh-key", "ed25519-dalek", "rustls", "openssl", "zstd-safe"}
for p in workspace:
    if not policy[p["name"]].get("leaf"):
        continue
    seen, todo = set(), [p["id"]]
    while todo:
        node = todo.pop()
        if node in seen:
            continue
        seen.add(node)
        if packages[node]["name"] in heavy - set(policy[p["name"]].get("allow-heavy", [])):
            errors.append(f'{p["name"]}: forbidden transitive dependency {packages[node]["name"]}')
        todo.extend(dep["pkg"] for dep in nodes[node]["deps"] if any(k["kind"] != "dev" for k in dep["dep_kinds"]))
# Filesystem implementations are composed once. Shared algorithms must not import a
# concrete driver or restore the old cfg-selected module alias.
root = Path(m["workspace_root"])
main = (root / "src/main.rs").read_text()
if re.search(r"\bmod\s+storage\b|#\[path\s*=\s*\"(?:apfs|btrfs|overlayfs)\.rs\"", main):
    errors.append("src/main.rs: filesystem facade alias is forbidden")
for name in ["repo.rs", "lineops.rs", "overlay.rs", "cache_overlay.rs", "changes.rs", "objstore.rs", "tree.rs", "fsutil.rs", "merge.rs", "write_source.rs", "recv.rs", "project_copy.rs", "project_move.rs", "gitbridge.rs", "cmd/admin.rs", "cmd/du.rs", "cmd/work.rs", "cmd/mergecmd.rs", "cmd/park.rs"]:
    source = (root / "src" / name).read_text()
    if re.search(r"crate::(?:apfs|btrfs|btrfs_driver|overlayfs|storage)::|(?<![:\w])storage::", source):
        errors.append(f"src/{name}: shared algorithms must use filesystem contracts")
if errors:
    print('\n'.join(errors), file=sys.stderr)
    sys.exit(1)
print('Workspace layers OK')
