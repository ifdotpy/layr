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
    for name in ['src/store_remote/reader.rs', 'src/store_remote/writer.rs', 'src/store_remote/chunked.rs', 'src/cmd/branch.rs',
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
    # A legacy transport function keeps only its frozen local-storage accesses: a new Repo access fails.
    path = root / 'src/store_remote/reader.rs'
    original = path.read_text()
    start = original.index('{', original.index('fn restore_over(')) + 1
    path.write_text(original[:start] + ' let _ = repo.p.db.lines(); ' + original[start:])
    assert boundary.check(root)
    path.write_text(original)
    # A `;` in an array type of a signature (`key: [u8; 32]`) does not hide the function's body.
    path = root / 'src/store_remote/chunked.rs'
    original = path.read_text()
    start = original.index('{', original.index('key: [u8; 32],\n        by_sha')) + 1
    path.write_text(original[:start] + ' let _ = crate::db::Db::open(path); ' + original[start:])
    assert boundary.check(root)
    path.write_text(original)
    # A `{` of a const generic argument in the return type (`-> T<{ 1 }>`) does not hide the function's body.
    path = root / 'src/store_remote/writer.rs'
    original = path.read_text()
    path.write_text(original + '\nstruct Probe<const N: usize>;\nfn probe() -> Probe<{ 1 }> { let _ = crate::db::Db::open(path); Probe }\n')
    assert boundary.check(root)
    path.write_text(original)
    # A new call inside an existing allowlisted function is not grandfathered either.
    path = root / 'src/sync_local/push.rs'
    original = path.read_text()
    path.write_text(original.replace('let repo = self.repo();', 'let repo = self.repo(); store.fetch(ctx, repo, opts);', 1))
    assert boundary.check(root)
    path.write_text(original)
    # A file that only a `#[cfg(test)] mod` declaration reaches is a test fixture; the same file in production fails.
    path = root / 'src/store_remote/reader.rs'
    original = path.read_text()
    probe = root / 'src/store_remote/probe_tests.rs'
    probe.write_text('fn fixture(local: &Repo) { local.p.db.lines(); }\n')
    path.write_text(original + '\n#[cfg(test)]\n#[path = "probe_tests.rs"]\nmod probe_tests;\n')
    assert not boundary.check(root)
    # A production declaration of the same file makes it production, also when it sits in an inline module whose
    # directory the test alias does not share (the file is `src/store_remote/worker.rs` for both).
    path.write_text(original + '\n#[cfg(test)]\n#[path = "probe_tests.rs"]\nmod probe_tests;\n#[path = "probe_tests.rs"]\nmod production_probe;\n')
    assert boundary.check(root)
    path.write_text(original)
    probe.unlink()
    # A test alias never exempts a production module, whatever the layout or spelling of the declarations; the test
    # `src/main.rs` is not a real crate root, so no declaration resolves inside the inline module.
    for file, declarations in [
        ('worker.rs', 'mod store_remote { pub mod worker; }\n#[cfg(test)]\n#[path = "store_remote/worker.rs"]\nmod test_worker;'),
        ('worker/mod.rs', 'mod store_remote { pub mod worker; }\n#[cfg(test)]\n#[path = "store_remote/worker/mod.rs"]\nmod test_worker;'),
        ('worker.rs', '#[cfg_attr(target_os = "macos", cfg(test))]\n#[path = "store_remote/worker.rs"]\nmod worker;'),
        ('worker.rs', '#[path = r"store_remote/worker.rs"]\nmod prod;\n#[cfg(test)]\n#[path = "store_remote/worker.rs"]\nmod test_worker;'),
    ]:
        worker = root / 'src/store_remote' / file
        worker.parent.mkdir(exist_ok=True)
        worker.write_text('fn fixture(local: &Repo) { local.p.db.lines(); }\n')
        (root / 'src/main.rs').write_text(declarations + '\n')
        assert boundary.check(root), declarations
        (root / 'src/main.rs').unlink()
        shutil.rmtree(worker.parent) if file == 'worker/mod.rs' else worker.unlink()
    # Ordinary test layouts stay test: a child of a test file, and a test file whose stem a production module shares.
    fixture = 'fn fixture(local: &Repo) { local.p.db.lines(); }\n'
    (root / 'src/main.rs').write_text('#[cfg(test)]\n#[path = "store_remote/probe_tests.rs"]\nmod probe_tests;\n')
    (root / 'src/store_remote/probe_tests.rs').write_text('mod helpers;\n')
    (root / 'src/store_remote/helpers.rs').write_text(fixture)
    assert not boundary.check(root)
    (root / 'src/store_remote/probe_tests.rs').unlink()
    (root / 'src/store_remote/helpers.rs').unlink()
    (root / 'src/main.rs').write_text('mod reader;\n#[cfg(test)]\n#[path = "store_remote/fixtures/reader.rs"]\nmod reader_tests;\n')
    (root / 'src/reader.rs').write_text('')
    (root / 'src/store_remote/fixtures').mkdir()
    (root / 'src/store_remote/fixtures/reader.rs').write_text(fixture)
    assert not boundary.check(root)
    shutil.rmtree(root / 'src/store_remote/fixtures')
    (root / 'src/main.rs').unlink()
    (root / 'src/reader.rs').unlink()
    # A production module that a test alias also loads keeps its children production too.
    (root / 'src/store_remote/portable').mkdir()
    (root / 'src/store_remote/portable/mod.rs').write_text('mod reader;\n')
    (root / 'src/store_remote/portable/reader.rs').write_text(fixture)
    (root / 'src/main.rs').write_text('mod store_remote { pub mod portable; }\n#[cfg(test)]\n#[path = "store_remote/portable/mod.rs"]\nmod reference;\n')
    assert boundary.check(root)
    (root / 'src/main.rs').unlink()
    shutil.rmtree(root / 'src/store_remote/portable')
    nested = root / 'nested.rs'
    # A `cfg(all(test, ..))` gate is a test gate, `any(test, ..)` and `not(test)` are not, and a `path` inside `cfg_attr` counts.
    for gate, exempt in [('all(test, target_os = "linux")', True), ('any(test, target_os = "linux")', False), ('all(not(test), unix)', False)]:
        nested.write_text(f'#[cfg({gate})]\nmod tests {{ fn f() {{ crate::apfs::run(); }} }}\nfn g() {{}}\n')
        assert ('crate::apfs' not in boundary.source(nested)) == exempt, gate
    nested.write_text('#[cfg(target_os = "linux")]\n#[cfg(test)]\nmod tests { fn f() { crate::apfs::run(); } }\n')
    assert 'crate::apfs' not in boundary.source(nested)
    (root / 'src/store_remote/portable.rs').write_text(fixture)
    (root / 'src/main.rs').write_text('mod store_remote {\n #[cfg_attr(feature = "portable", path = "portable.rs")]\n mod driver;\n}\n'
                                      '#[cfg(test)]\n#[path = "store_remote/portable.rs"]\nmod portable;\n')
    assert boundary.check(root)
    (root / 'src/main.rs').unlink()
    (root / 'src/store_remote/portable.rs').unlink()
    # Both crate roots count: a library declaration keeps the module production although the binary only tests it.
    (root / 'src/lib.rs').write_text('mod probe;\n')
    (root / 'src/main.rs').write_text('#[cfg(test)]\nmod probe;\n')
    (root / 'src/probe.rs').write_text('')
    assert 'src/probe.rs' not in boundary.test_files(root)
    (root / 'src/bin').mkdir()
    (root / 'src/bin/audit.rs').write_text('mod probe;\n')
    (root / 'src/bin/probe.rs').write_text('')
    (root / 'src/main.rs').write_text('#[cfg(test)]\n#[path = "bin/probe.rs"]\nmod probe;\n')
    assert 'src/bin/probe.rs' not in boundary.test_files(root)
    shutil.rmtree(root / 'src/bin')
    for leftover in ['lib.rs', 'main.rs', 'probe.rs']:
        (root / 'src' / leftover).unlink()
    # Block comments nest, and raw strings may take any number of hashes: the call after them stays visible.
    nested.write_text('/* a /* b */ " */ fn f() { crate::apfs::run(); let _ = "after"; }\n')
    assert 'crate::apfs::run' in boundary.source(nested)
    nested.write_text('fn f() { let _ = r#########" " /* "#########; crate::apfs::run(); }\n')
    assert 'crate::apfs::run' in boundary.source(nested)
    nested.write_text('#[cfg(test)]\n#[allow(dead_code)]\npub mod tests { fn f() { crate::apfs::run(); } }\nfn g() {}\n')
    assert 'crate::apfs' not in boundary.source(nested)
    # Grouped imports cannot silently introduce a new Backend alias into an existing adapter.
    path = root / 'src/sync_local/fetch.rs'
    path.write_text(path.read_text() + '\nuse crate::store_remote::{Options, backend::Backend};\n')
    assert boundary.check(root)
print('Sync boundary regression probes OK (15 negative cases, test-only files and nested comments)')
