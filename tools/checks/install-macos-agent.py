#!/usr/bin/env python3
"""Install user LaunchAgents for one provisioned project; no credentials in plists."""
import argparse
import os
import pathlib
import plistlib
import re
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("project")
parser.add_argument("--binary", type=pathlib.Path, required=True)
parser.add_argument("--root", type=pathlib.Path, required=True)
args = parser.parse_args()
if not re.fullmatch(r"[a-zA-Z0-9_-]+", args.project):
    parser.error("project must contain letters, digits, underscores or hyphens")
binary = args.binary.resolve(strict=True)
root = args.root.resolve(strict=True)
home = pathlib.Path.home()
logs = home / "Library/Logs/Layr"
agents = home / "Library/LaunchAgents"
logs.mkdir(parents=True, exist_ok=True)
agents.mkdir(parents=True, exist_ok=True)
for kind, command in (("checks", ["checks", "watch", "--json"]), ("fetch", ["fetch", "origin"]), ("push", ["push", "origin", "--index"])):
    label = f"ws.layr.{kind}.{args.project}"
    path = agents / f"{label}.plist"
    config = {
        "Label": label,
        "ProgramArguments": [str(binary), "--project", args.project, *command],
        "EnvironmentVariables": {"LAYR_ROOT": str(root)},
        "WorkingDirectory": "/",
        "RunAtLoad": True,
        "ThrottleInterval": 5,
        "StandardOutPath": str(logs / f"{kind}-{args.project}.log"),
        "StandardErrorPath": str(logs / f"{kind}-{args.project}.err"),
    }
    if kind == "checks":
        config["KeepAlive"] = True
    else:
        config["StartInterval"] = 15
    path.write_bytes(plistlib.dumps(config))
    # Replacing this project's own agent is safe and makes installation repeatable.
    subprocess.run(["launchctl", "bootout", f"gui/{os.getuid()}/{label}"], capture_output=True)
    subprocess.run(["launchctl", "bootstrap", f"gui/{os.getuid()}", str(path)], check=True)
    print(path)
