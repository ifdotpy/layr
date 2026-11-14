#!/usr/bin/env python3
"""Enforce workspace edges and transitive leaf dependencies over all manifest targets/features."""
import json
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
    for d in p["dependencies"]:  # Includes optional, target-specific, build and dev edges.
        if d["name"] in names:
            if d["name"] not in rule["dependencies"] or policy[d["name"]]["layer"] > rule["layer"]:
                errors.append(f'{p["name"]} -> {d["name"]}: unlisted or upward edge')
    if set(rule["dependencies"]) - names:
        errors.append(f'{p["name"]}: unknown allowed dependencies {set(rule["dependencies"]) - names}')

nodes = {n["id"]: n for n in m["resolve"]["nodes"]}
heavy = {"anyhow", "tokio", "clap", "rayon", "reqwest", "hyper", "image", "rusqlite"}
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
if errors:
    print('\n'.join(errors), file=sys.stderr)
    sys.exit(1)
print('Workspace layers OK')
