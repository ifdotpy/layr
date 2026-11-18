#!/usr/bin/env python3
"""Negative probes for new callers in legacy/local files and new transport workers."""
import importlib.util
from pathlib import Path
import shutil
import sys
import tempfile

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('boundary', Path(__file__).with_name('check-sync-boundary.py'))
boundary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(boundary)
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    for name in ['src/store_remote/reader.rs', 'src/store_remote/writer.rs', 'src/cmd/branch.rs',
                 'src/sync_local/push.rs', 'src/sync_local/fetch.rs', 'crates/layr-store/src/sync.rs']:
        target = root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(boundary.ROOT / name, target)
    assert not boundary.check(root)
    probes = [
        ('src/new_caller.rs', 'use crate::{store_remote::Store};'),
        ('src/cmd/branch.rs', 'fn new_caller() { store.fetch(ctx, repo, opts); }'),
        ('src/sync_local/push.rs', 'fn new_caller() { store.fetch(ctx, repo, opts); }'),
        ('src/store_remote/fetch_plan.rs', 'use crate::repo::Repo; fn plan(local: &Repo) { local.p.db.lines(); }'),
        ('src/store_remote/reader.rs', 'fn worker(local: &Repo) { local.p.db.lines(); }'),
        ('src/store_remote/writer.rs', 'fn worker(local: &crate::repo::Repo) { local.p.db.lines(); }'),
        ('crates/layr-store/src/sync.rs', 'use reqwest::Client;'),
        ('src/store_remote/fetch_plan.rs', "fn plan(local: &crate::store::Project<'_>) { local.export_jsonl(); }"),
        ('src/store_remote/reader.rs', 'struct NewWorker; impl NewWorker { fn fetch(local: &Repo) { local.p.export_jsonl(); } }'),
    ]
    for name, injection in probes:
        target = root / name
        original = target.read_text() if target.exists() else ''
        target.write_text(original + '\n' + injection + '\n')
        assert boundary.check(root), name
        if original:
            target.write_text(original)
        else:
            target.unlink()
    # A path-qualified database call inside an existing engine must be rejected too.
    path = root / 'src/store_remote/reader.rs'
    original = path.read_text()
    start = original.index('{', original.index('fn fetch_with_local(')) + 1
    path.write_text(original[:start] + ' crate::db::open(); ' + original[start:])
    assert boundary.check(root)
    path.write_text(original)
    # A new call inside an existing allowlisted function is not grandfathered either.
    path = root / 'src/sync_local/push.rs'
    original = path.read_text()
    path.write_text(original.replace('let repo = self.repo();', 'let repo = self.repo(); store.fetch(ctx, repo, opts);', 1))
    assert boundary.check(root)
    path.write_text(original)
    # Grouped imports cannot silently introduce a new Backend alias into an existing adapter.
    path = root / 'src/sync_local/fetch.rs'
    path.write_text(path.read_text() + '\nuse crate::store_remote::{Options, backend::Backend};\n')
    assert boundary.check(root)
print('Sync boundary regression probes OK (12 negative cases)')
