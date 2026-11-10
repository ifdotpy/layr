#!/usr/bin/env python3
"""Verify planned-only checks on a disposable case-sensitive APFS store."""
import json
import os
import subprocess
import tempfile
from pathlib import Path


def main():
    if os.getuid() == 0:
        raise RuntimeError('run as a non-root user')
    binary = os.environ['LAYR']
    with tempfile.TemporaryDirectory(prefix='layr-planned-', dir=os.environ['LAYR_APFS_TEST_ROOT']) as directory:
        root = Path(directory)
        source = root / 'source'
        source.mkdir()
        (source / 'file.txt').write_text('initial\n')
        env = {'PATH': os.environ['PATH'], 'HOME': str(root), 'TMPDIR': str(root),
               'LAYR_ROOT': str(root / 'store'), 'LAYR_AUTHOR_NAME': 'Tester',
               'LAYR_AUTHOR_EMAIL': 'tester@example.com', 'GIT_CONFIG_NOSYSTEM': '1',
               'GIT_CONFIG_GLOBAL': '/dev/null'}

        def run(*args, check=True, cwd=None):
            result = subprocess.run([binary, *args], cwd=cwd or root, env=env, text=True, capture_output=True)
            if check and result.returncode:
                raise RuntimeError(f'{args}: {result.stdout}\n{result.stderr}')
            return result

        run('init', 'p', '--from', str(source), '-q')
        main_line = root / 'store/projects/p/lines/main'
        run('config', 'check.line', 'true', cwd=main_line)
        run('config', 'check.change', 'true', cwd=main_line)
        run('config', 'checks.changes', '["change"]', cwd=main_line)
        run('config', 'checks.lines', '{"main":["line"]}', cwd=main_line)
        run('config', '--local', 'checks.runner', 'true', cwd=main_line)
        result = json.loads(run('checks', 'run', '--state', 'main', '--planned-only', '--json', cwd=main_line).stdout)
        assert {item['name'] for item in result['checks']} == {'line'}, result
        run('line', 'new', 'topic', 'main', cwd=main_line)
        topic = root / 'store/projects/p/lines/topic'
        (topic / 'file.txt').write_text('topic\n')
        run('add', 'file.txt', cwd=topic)
        run('save', '-qm', 'topic', cwd=topic)
        result = json.loads(run('checks', 'run', '--state', 'topic', '--planned-only', '--json', cwd=main_line).stdout)
        assert result['count'] == 0, result
        run('approve', 'topic', cwd=main_line)
        result = json.loads(run('checks', 'run', '--state', 'topic', '--json', cwd=main_line).stdout)
        assert {item['name'] for item in result['checks']} == {'change'}, result
        assert run('checks', 'watch', '--once', '--planned-only', cwd=main_line, check=False).returncode != 0
        assert run('checks', 'run', '--state', 'main', '--planned-only', '--required-checks', 'line',
                   cwd=main_line, check=False).returncode != 0
        print('PASS: planned-only preserves watched checks and excludes unplanned change checks')


if __name__ == '__main__':
    main()
