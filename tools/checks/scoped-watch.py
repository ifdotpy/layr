#!/usr/bin/env python3
"""Scoped check watcher of a laptop runner (docs/laptop-runner.md).

`layr checks watch` takes every pending version of every open change: 3,905 of project layr on
2026-11-15, more than a laptop runs in a week. This loop takes only unprivileged checks named on
its command line, of states in the queue into main first, then of states younger than MAX_AGE,
newest first. It runs one check at a time.

It runs as root with the project's grant (LAYR_GATEWAY_TOKEN_FILE), like the AWS check job: the
daemon of the check user has no grant, so root brings each state here. The check itself runs as
the check user through the runner daemon, because layr refuses a root executor.

LAYR_CHECK_SEED_JOB names the check job script of Layr Cloud (infra/aws/ci_job.py of project
layr-cloud). When it is set, the loop adds the Rust toolchain a state pins and the crates of its
Cargo.lock to the read-only check seed before the check, with the code of the AWS job. Without
them each state that pinned a newer Rust failed its unit check in seconds (2026-11-15).

No timer: when no check is pending, the loop waits until the project changes in Layr Cloud (`layr wait`,
layr 0.52 and later), then reads the new heads. After each check it pushes the result at once. A layr
without `layr wait` sleeps IDLE seconds instead.

Usage: scoped-watch.py <project> <check>[,<check>...]
"""
import importlib.util
import json
import os
import platform
import subprocess
import sys
import time

LAYR = '/usr/local/bin/layr'
ROOT = os.environ.get('LAYR_ROOT', '/var/lib/layr')
SOCKET = '/run/layr-runner/layr.sock'
RUNUSER = '/usr/sbin/runuser'
CHECK_USER = 'layr-checks'
PATH = '/usr/local/bin:/usr/bin:/bin'
MAX_AGE = 24 * 3600 * 1000
# A state that this store cannot bring here waits this long before the next try. Without the wait, one
# queued state whose view named a collected state stopped every other check (2026-11-15). A state that
# came before its objects (the push still runs) is here on a later try.
RETRY = 600
# The build cache of this platform comes as root at most this often. The check user has no grant, so a check
# copy could not fetch it and every build was cold ("gateway bootstrap: HTTP 404", 2026-11-15).
CACHE_EVERY = 600
IDLE = 60
# The cursor files of `layr wait`, one for each project.
WAIT_DIR = os.path.join(ROOT, 'wait')


def log(**fields):
    print(json.dumps(fields, sort_keys=True), flush=True)


def stamp(state):
    """A state id is a UUIDv7: its first 48 bits are its time in milliseconds."""
    try:
        return int(state.replace('-', '')[:12], 16)
    except ValueError:
        return 0


def choose(pending, queued, checks, now, skip=frozenset()):
    """The next check to run: queued states first, then states younger than MAX_AGE, newest first."""
    take = [p for p in pending if p['name'] in checks and not p.get('privileged') and p['state'] not in skip
            and (p['state'] in queued or now - stamp(p['state']) < MAX_AGE)]
    take.sort(key=lambda p: (p['state'] not in queued, -stamp(p['state'])))
    return take[0] if take else None


class Runner:
    def __init__(self, project, checks):
        self.project, self.checks = project, checks
        self.platform = f"linux-{platform.machine()}"
        self.env = {'PATH': PATH, 'LAYR_ROOT': ROOT,
                    'LAYR_GATEWAY_TOKEN_FILE': os.environ['LAYR_GATEWAY_TOKEN_FILE']}
        self.seed = self.seeder(os.environ.get('LAYR_CHECK_SEED_JOB'))
        self.failed = {}
        self.cache_at = 0
        self.can_wait = None

    def layr(self, *args, timeout=1800):
        return subprocess.run([LAYR, '--project', self.project, *args], env=self.env, text=True,
                              capture_output=True, cwd='/', timeout=timeout)

    def seeder(self, job):
        if not job:
            return lambda state: None
        spec = importlib.util.spec_from_file_location('ci_job', job)
        ci_job = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(ci_job)
        ci_job.log = log

        def job_layr(*args, check=True):
            result = self.layr(*args)
            if check and result.returncode != 0:
                raise RuntimeError(f"layr {' '.join(args[:3])} exit {result.returncode}: {result.stderr[-500:]}")
            return result
        seeded = set()
        return lambda state: ci_job.seed_changes(job_layr, [state], seeded)

    def next_check(self):
        pending = self.layr('checks', 'pending', '--platform', self.platform, '--json')
        if pending.returncode != 0:
            log(step='pending', exit=pending.returncode, detail=pending.stderr.strip()[-300:])
            return None
        queue = self.layr('queue', 'list', '--into', 'main', '--json')
        queued = set()
        if queue.returncode == 0:
            queued = {e['state'] for e in json.loads(queue.stdout) if e.get('status') == 'pending'}
        now = time.time()
        self.failed = {state: at for state, at in self.failed.items() if now - at < RETRY}
        return choose(json.loads(pending.stdout)['pending'], queued, self.checks, int(now * 1000), set(self.failed))

    def wait_for_change(self):
        """Wait until another machine changes the project (a new state, a queue entry, an approval), then
        fetch the heads. The results of this runner do not count (`--others`)."""
        if self.can_wait is None:
            self.can_wait = self.layr('wait', '--help').returncode == 0
        if self.can_wait:
            os.makedirs(WAIT_DIR, exist_ok=True)
            cursor = os.path.join(WAIT_DIR, f'{self.project}-scoped.cursor')
            result = self.layr('wait', 'origin', '--cursor-file', cursor, '--others', timeout=None)
            if result.returncode != 0:
                log(step='wait', exit=result.returncode, detail=result.stderr.strip()[-300:])
                time.sleep(IDLE)
        else:
            time.sleep(IDLE)
        started = time.time()
        result = self.layr('fetch', 'origin', '--heads')
        if result.returncode not in (0, 2):
            log(step='fetch', exit=result.returncode, seconds=round(time.time() - started),
                detail=result.stderr.strip()[-300:])

    def push(self):
        """The signed result goes to Layr Cloud right after the check: the merge job waits for it."""
        started = time.time()
        result = self.layr('push', 'origin', '--no-cache-sweep')
        log(step='push', exit=result.returncode, seconds=round(time.time() - started),
            detail=result.stderr.strip()[-300:] if result.returncode else '')

    def fetch_cache(self):
        if time.time() - self.cache_at < CACHE_EVERY:
            return
        self.cache_at = time.time()
        overlay = f'target.{self.platform}'
        # Every state of the overlay (history=last keeps few): a check copy takes the cache of its own line,
        # which `--heads` leaves on the remote.
        started = time.time()
        result = self.layr('fetch', 'origin', '--overlay', overlay)
        log(step='fetch-cache', overlay=overlay, exit=result.returncode, seconds=round(time.time() - started),
            detail=result.stderr.strip()[-300:] if result.returncode not in (0, 2) else '')

    def bring(self, state):
        """Bring the tree of the state here; fetch the heads once when the store does not hold it."""
        here = self.layr('try', state, '--', 'true')
        if here.returncode != 0:
            self.layr('fetch', 'origin', '--heads')
            here = self.layr('try', state, '--', 'true')
        if here.returncode != 0:
            log(step='fetch-state', state=state, exit=here.returncode, detail=here.stderr.strip()[-300:])
            self.failed[state] = time.time()
        return here.returncode == 0

    def run(self, item):
        started = time.time()
        result = subprocess.run(
            [RUNUSER, '-u', CHECK_USER, '--', 'env', '-i', f'PATH={PATH}', f'LAYR_ROOT={ROOT}',
             f'LAYR_SOCKET={SOCKET}', LAYR, '--project', self.project, 'checks', 'run',
             '--state', item['state'], '--only', item['name'], '--json'],
            text=True, capture_output=True, cwd='/', timeout=7200)
        log(step='check', state=item['state'], name=item['name'], exit=result.returncode,
            seconds=round(time.time() - started), detail=result.stderr.strip()[-300:])
        return result.returncode

    def loop(self):
        while True:
            item = self.next_check()
            if not item:
                self.wait_for_change()
                continue
            if not self.bring(item['state']):
                continue
            self.fetch_cache()
            try:
                self.seed(item['state'])
            except Exception as error:
                log(step='seed', state=item['state'], error=str(error)[-300:])
            # Exit 1 is a failed check, a result like a pass. Other codes mean the run did not finish.
            if self.run(item) in (0, 1):
                self.push()
            else:
                time.sleep(IDLE)


if __name__ == '__main__':
    Runner(sys.argv[1], set(sys.argv[2].split(','))).loop()
