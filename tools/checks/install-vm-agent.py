#!/usr/bin/env python3
"""Start the isolated runner VM from user launchd, outside the installing shell."""
import argparse
import json
import os
import pathlib
import plistlib
import shutil
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.parse_args()
if sys.platform != "darwin" or os.getuid() == 0:
    parser.error("run as the Mac user")
binary = shutil.which("limactl")
if not binary:
    parser.error("limactl is required")
vm = "layr-runner-vz"
info = json.loads(subprocess.check_output([binary, "list", "--json", vm], text=True))
if info["status"] != "Stopped":
    parser.error("stop layr-runner-vz before installing its foreground LaunchAgent")
if info["vmType"] != "vz" or info.get("config", {}).get("mounts", []) or info.get("config", {}).get("ssh", {}).get("forwardAgent"):
    parser.error("runner must use VZ with no host mounts or SSH agent forwarding")
home = pathlib.Path.home()
logs = home / "Library/Logs/Layr"
agents = home / "Library/LaunchAgents"
logs.mkdir(parents=True, exist_ok=True)
agents.mkdir(parents=True, exist_ok=True)
label = "ws.layr.runner-vm"
path = agents / f"{label}.plist"
config = {
    "Label": label,
    "ProgramArguments": [binary, "start", "--foreground", "--yes", vm],
    "RunAtLoad": True,
    "KeepAlive": False,
    "WorkingDirectory": str(home),
    "EnvironmentVariables": {"PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"},
    "StandardOutPath": str(logs / "runner-vm.log"),
    "StandardErrorPath": str(logs / "runner-vm.err"),
}
subprocess.run(["launchctl", "bootout", f"gui/{os.getuid()}/{label}"], capture_output=True)
path.write_bytes(plistlib.dumps(config))
subprocess.run(["launchctl", "bootstrap", f"gui/{os.getuid()}", str(path)], check=True)
print(path)
