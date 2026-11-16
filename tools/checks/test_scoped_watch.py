#!/usr/bin/env python3
"""The choice of the scoped check watcher (scoped-watch.py)."""
import importlib.util
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


if __name__ == '__main__':
    unittest.main()
