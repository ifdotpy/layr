#!/usr/bin/env python3
"""Compare workspace Clippy diagnostics with the checked-in per-package ceilings."""
import collections
import json
import subprocess
import sys
import tomllib

baseline = json.load(open('tools/clippy-baseline.json'))
with open('Cargo.toml', 'rb') as manifest:
    lints = tomllib.load(manifest)['workspace']['lints']
tracked = set(lints['rust']) | {f'clippy::{lint}' for lint in lints['clippy']}
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--no-deps', '--format-version=1']))
packages = {p['id']: p['name'] for p in metadata['packages']}
for package in metadata['packages']:
    with open(package['manifest_path'], 'rb') as manifest:
        if tomllib.load(manifest).get('lints', {}).get('workspace') is not True:
            print(f"{package['name']}: missing [lints] workspace = true", file=sys.stderr)
            sys.exit(1)
counts = collections.Counter()
diagnostics = collections.defaultdict(list)
cmd = ['cargo', 'clippy', '--locked', '--workspace', '--all-targets', '--all-features', '--message-format=json', '--', '--cap-lints', 'warn']
proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True)
for line in proc.stdout:
    item = json.loads(line)
    if item.get('reason') != 'compiler-message':
        continue
    message = item['message']
    if message['level'] == 'error' and message.get('rendered'):
        print(message['rendered'], file=sys.stderr)
    code = (message.get('code') or {}).get('code')
    package = packages.get(item['package_id'])
    if package and code and code in tracked:
        counts[package, code] += 1
        diagnostics[package, code].append(message.get('rendered') or message['message'])
if proc.wait():
    sys.exit(proc.returncode)
failed = False
for package, code in sorted(counts):
    count, limit = counts[package, code], baseline.get(package, {}).get(code, 0)
    if count > limit:
        print(f'{package}: {code}: {count} > {limit}', file=sys.stderr)
        for diagnostic in diagnostics[package, code]:
            print(diagnostic, file=sys.stderr)
        failed = True
if failed:
    sys.exit(1)
print('Clippy ratchet OK')
