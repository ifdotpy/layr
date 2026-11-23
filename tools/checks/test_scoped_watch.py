#!/usr/bin/env python3
"""The choice of the scoped check watcher (scoped-watch.py)."""
import importlib.util
import json
import tempfile
from pathlib import Path
from subprocess import CompletedProcess, TimeoutExpired
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('scoped_watch', Path(__file__).with_name('scoped-watch.py'))
watch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(watch)

NOW = 0x01a1dc87_0000
HOUR = 3600 * 1000


def state(ms):
    return f'{ms:012x}'[:8] + '-' + f'{ms:012x}'[8:] + '-7000-8000-000000000000'


def item(ms, name='unit', privileged=False):
    return {'state': state(ms), 'name': name, 'privileged': privileged}


class Choose(unittest.TestCase):
    def test_a_state_id_gives_its_time(self):
        self.assertEqual(watch.stamp(state(NOW)), NOW)
        self.assertEqual(watch.stamp('not-a-state'), 0)

    def test_newest_recent_state_comes_first(self):
        old, new = item(NOW - 2 * HOUR), item(NOW - HOUR)
        self.assertEqual(watch.choose([old, new], set(), {'unit'}, NOW), new)

    def test_a_queued_state_comes_before_a_newer_one(self):
        queued, new = item(NOW - 30 * 24 * HOUR), item(NOW - HOUR)
        self.assertEqual(watch.choose([new, queued], {queued['state']}, {'unit'}, NOW), queued)

    def test_old_unqueued_other_and_privileged_checks_stay(self):
        pending = [item(NOW - 25 * HOUR), item(NOW, name='apfs'), item(NOW, privileged=True)]
        self.assertIsNone(watch.choose(pending, set(), {'unit'}, NOW))

    def test_a_state_this_store_cannot_bring_waits(self):
        stuck, other = item(NOW - 30 * 24 * HOUR), item(NOW - HOUR)
        self.assertEqual(watch.choose([stuck, other], {stuck['state']}, {'unit'}, NOW, {stuck['state']}), other)


class Signal(unittest.TestCase):
    """An idle runner wakes on project changes or local retries, and pushes results at once."""

    def runner(self, wait_ok=True, can_wait=True):
        runner = watch.Runner.__new__(watch.Runner)
        runner.project, runner.calls, runner.can_wait = 'p', [], None
        runner.platform, runner.checks, runner.failed = 'linux-x86_64', {'unit'}, {}
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        watch.WAIT_DIR = folder.name

        def layr(*args, timeout=1800):
            runner.calls.append((args, timeout))
            if args == ('wait', '--help'):
                return CompletedProcess(args, 0 if can_wait else 1, '', '')
            if args[0] == 'wait':
                return CompletedProcess(args, 0 if wait_ok else 1, '', '')
            return CompletedProcess(args, 0, '', '')
        runner.layr = layr
        return runner

    def test_an_idle_runner_waits_for_others_then_fetches_the_heads(self):
        runner = self.runner()
        sleeps = []
        watch.time.sleep, sleep = sleeps.append, watch.time.sleep
        try:
            runner.wait_for_change()
        finally:
            watch.time.sleep = sleep
        names = [args for args, _ in runner.calls]
        self.assertEqual(names[1], ('wait', 'origin', '--cursor-file', f'{watch.WAIT_DIR}/p-scoped.cursor', '--others'))
        self.assertIsNone(runner.calls[1][1], 'the wait has no time limit')
        self.assertEqual(names[2], ('fetch', 'origin', '--heads'))
        self.assertEqual(sleeps, [])

    def test_a_layr_without_wait_sleeps_the_old_interval(self):
        runner = self.runner(can_wait=False)
        sleeps = []
        watch.time.sleep, sleep = sleeps.append, watch.time.sleep
        try:
            runner.wait_for_change()
        finally:
            watch.time.sleep = sleep
        self.assertEqual(sleeps, [watch.IDLE])
        self.assertEqual([a for a, _ in runner.calls], [('wait', '--help'), ('fetch', 'origin', '--heads')])

    def test_a_quiet_project_wakes_at_the_earliest_local_retry(self):
        runner = self.runner()
        older, newer = item(NOW - HOUR), item(NOW)
        now = NOW / 1000
        runner.failed = {newer['state']: now, older['state']: now - watch.RETRY + 0.25}
        original = runner.layr

        def layr(*args, timeout=1800):
            nonlocal now
            if args[:2] == ('checks', 'pending'):
                return CompletedProcess(args, 0, json.dumps({'pending': [newer, older]}), '')
            if args[:2] == ('queue', 'list'):
                return CompletedProcess(args, 0, '[]', '')
            result = original(*args, timeout=timeout)
            if args[:2] == ('wait', 'origin'):
                self.assertEqual(args[-2:], ('--timeout', '1'))
                self.assertEqual(timeout, 1 + watch.WAIT_GRACE)
                now += int(args[-1])
            return result

        runner.layr = layr
        with patch.object(watch.time, 'time', lambda: now):
            self.assertIsNone(runner.next_check())
            runner.wait_for_change()
            self.assertEqual(runner.next_check(), older)

    def test_a_wait_that_ignores_its_timeout_returns_to_the_scan(self):
        runner = self.runner()
        pending = item(NOW)
        now = NOW / 1000
        runner.failed = {pending['state']: now - watch.RETRY + 0.25}
        calls = []

        def layr(*args, timeout=1800):
            nonlocal now
            calls.append(args[:2])
            if args == ('wait', '--help'):
                return CompletedProcess(args, 0, '', '')
            if args[:2] == ('checks', 'pending'):
                return CompletedProcess(args, 0, json.dumps({'pending': [pending]}), '')
            if args[:2] == ('queue', 'list'):
                return CompletedProcess(args, 0, '[]', '')
            if args == ('fetch', 'origin', '--heads'):
                return CompletedProcess(args, 0, '', '')
            self.assertEqual(args[:2], ('wait', 'origin'))
            self.assertEqual(timeout, 1 + watch.WAIT_GRACE)
            now += timeout
            raise TimeoutExpired(args, timeout)

        runner.layr = layr
        with patch.object(watch.time, 'time', lambda: now):
            self.assertIsNone(runner.next_check())
            runner.wait_for_change()
            self.assertEqual(runner.next_check(), pending)
        self.assertEqual(calls[-3:], [('fetch', 'origin'), ('checks', 'pending'), ('queue', 'list')])

    def test_a_failed_pending_list_fetches_heads_before_rescanning(self):
        runner = self.runner()
        pending = item(NOW)
        calls = []

        def layr(*args):
            command = args[:2]
            calls.append(command)
            if command == ('checks', 'pending'):
                if ('fetch', 'origin') not in calls:
                    self.assertEqual(calls, [('checks', 'pending')])
                    return CompletedProcess(args, 1, '', 'unavailable')
                return CompletedProcess(args, 0, json.dumps({'pending': [pending]}), '')
            if command == ('fetch', 'origin'):
                self.assertEqual(args[2:], ('--heads',))
                return CompletedProcess(args, 0, '', '')
            self.assertEqual(command, ('queue', 'list'))
            return CompletedProcess(args, 0, '[]', '')

        runner.layr = layr
        with patch.object(watch.time, 'time', return_value=NOW / 1000), \
                patch.object(watch.time, 'sleep', side_effect=calls.append), patch.object(watch, 'log'):
            self.assertEqual(runner.next_check(), pending)
        self.assertEqual(calls, [
            ('checks', 'pending'), watch.IDLE, ('fetch', 'origin'), ('checks', 'pending'), ('queue', 'list'),
        ])

    def test_a_result_is_pushed_at_once(self):
        runner = self.runner()
        runner.push()
        self.assertEqual(runner.calls[0][0], ('push', 'origin', '--no-cache-sweep'))


class Prepare(unittest.TestCase):
    """The seed command runs once for each head of main that the merge machine moves."""

    def runner(self, command='bash tools/checks/prepare-linux.sh', machine='m1'):
        runner = watch.Runner.__new__(watch.Runner)
        runner.project, runner.calls = 'p', []
        runner.prepare_command, runner.merge_machine = watch.shlex.split(command), machine
        runner.head = 'h1'
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        watch.WAIT_DIR = folder.name

        class Done:
            def __init__(self, stdout=''):
                self.returncode, self.stderr, self.stdout = 0, '', stdout

        def layr(*args, timeout=1800):
            runner.calls.append(args)
            return Done(runner.head + '\n' if 'rev-parse' in args else '')
        runner.layr = layr
        return runner

    def tries(self, runner):
        return [args for args in runner.calls if 'try' in args]

    def test_a_new_head_runs_the_command_once_in_that_head(self):
        runner = self.runner()
        runner.prepare()
        runner.prepare()
        self.assertIn(('--line', 'main', 'rev-parse', 'main@m1'), runner.calls)
        self.assertEqual(self.tries(runner), [('--line', 'main', 'try', 'h1', '--', 'bash', 'tools/checks/prepare-linux.sh')])
        runner.head = 'h2'
        runner.prepare()
        self.assertEqual(len(self.tries(runner)), 2)

    def test_no_command_runs_nothing(self):
        runner = self.runner(command='')
        runner.prepare()
        self.assertEqual(runner.calls, [])


if __name__ == '__main__':
    unittest.main()
