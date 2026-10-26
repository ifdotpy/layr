#!/usr/bin/env python3
"""Run a push-cache benchmark in OrbStack with a Mac host disk guard.

Example (no fixture copies):
  python3 benchmarks/push-cache-guard.py --peak-gib 2 -- \\
    python3 /root/pc/benchmark.py --run-id adaptive --after-run-id timing \\
    --fixture studio after

The peak estimate covers one encrypted store, bounded cache, temporary frames,
logs and filesystem overhead. Run only after recording a justified estimate.
"""
import argparse
import datetime
import json
import os
import pathlib
import signal
import subprocess
import sys
import uuid

GIB = 1024 ** 3
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--machine', default='layr-pc')
parser.add_argument('--peak-gib', type=float, required=True)
parser.add_argument('--log', type=pathlib.Path, default=pathlib.Path('/tmp/pc-disk-guard.jsonl'))
parser.add_argument('command', nargs=argparse.REMAINDER)
args = parser.parse_args()
command = args.command[1:] if args.command[:1] == ['--'] else args.command
assert command and args.peak_gib > 0
pid_file = '/root/pc/guard-' + uuid.uuid4().hex + '.pid'
process = None


def observe():
    output = subprocess.check_output(['df', '-k', '/System/Volumes/Data'], text=True)
    free = int(output.splitlines()[-1].split()[3]) * 1024
    with args.log.open('a') as log:
        log.write(json.dumps({'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                              'available_bytes': free, 'peak_estimate_bytes': args.peak_gib * GIB,
                              'command': command, 'pid_file': pid_file}) + '\n')
    return free


def stop_remote():
    # The benchmark owns this unique PID file and terminates its active layr child
    # before exiting. No unobserved remote upload remains after a disk abort.
    script = ('import os,pathlib,signal; p=pathlib.Path(' + repr(pid_file) + '); '
              'os.kill(int(p.read_text()),signal.SIGTERM) if p.exists() else None')
    subprocess.run(['orb', '-m', args.machine, '-u', 'root', 'python3', '-c', script], timeout=20)
    if process is not None:
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)


def interrupted(signum, frame):
    raise InterruptedError(signum)


signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
free = observe()
required = max(20 * GIB, 3 * args.peak_gib * GIB)
if free < required:
    print('aborted: disk (start requires %.1f GiB, available %.1f GiB)' % (required / GIB, free / GIB), flush=True)
    raise SystemExit(75)
print('disk guard: %.1f GiB available; peak estimate %.1f GiB; required %.1f GiB' %
      (free / GIB, args.peak_gib, required / GIB), flush=True)
process = subprocess.Popen(['orb', '-m', args.machine, '-u', 'root', 'env',
                            'LAYR_PUSH_GUARD_PID_FILE=' + pid_file, *command])
try:
    while True:
        try:
            status = process.wait(timeout=30)
            break
        except subprocess.TimeoutExpired:
            if observe() < 10 * GIB:
                stop_remote()
                print('aborted: disk (Mac host below 10 GiB)', flush=True)
                raise SystemExit(75)
    observe()
    raise SystemExit(status)
except InterruptedError as error:
    stop_remote()
    raise SystemExit(128 + int(error.args[0]))
