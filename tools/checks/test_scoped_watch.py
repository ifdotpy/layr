#!/usr/bin/env python3
"""The choice of the scoped check watcher (scoped-watch.py)."""
import importlib.util
import tempfile
from pathlib import Path
import unittest

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
    """No timer: an idle runner waits for a change of the project, and each result goes out at once."""

    def runner(self, wait_ok=True, can_wait=True):
        runner = watch.Runner.__new__(watch.Runner)
        runner.project, runner.calls, runner.can_wait = 'p', [], None
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        watch.WAIT_DIR = folder.name

        class Done:
            def __init__(self, code):
                self.returncode, self.stderr, self.stdout = code, '', ''

        def layr(*args, timeout=1800):
            runner.calls.append((args, timeout))
            if args == ('wait', '--help'):
                return Done(0 if can_wait else 1)
            if args[0] == 'wait':
                return Done(0 if wait_ok else 1)
            return Done(0)
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

    def test_a_result_is_pushed_at_once(self):
        runner = self.runner()
        runner.push()
        self.assertEqual(runner.calls[0][0], ('push', 'origin', '--no-cache-sweep'))


if __name__ == '__main__':
    unittest.main()
