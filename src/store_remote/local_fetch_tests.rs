//! Local fetch work is bounded by machines and batches, rather than history length.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::model::{DraftRecord, LineChange, LineState, LineTransition};
use layr_store::sync::LocalReplica;

fn fixture<'a>(ctx: &'a Ctx, project: &str, count: usize) -> Repo<'a> {
    let dir = ctx.store.project_dir("p").unwrap();
    std::fs::create_dir_all(dir.join("meta/db")).unwrap();
    std::fs::create_dir_all(dir.join("states")).unwrap();
    let p = crate::store::Project {
        lease: Default::default(),
        store: &ctx.store,
        name: "p".into(),
        dir: dir.clone(),
        id: project.into(),
        db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
    };
    let mut init = DraftRecord::new("project");
    init.data = serde_json::json!({"id":project,"name":"p","config":{"objects":"off"}});
    p.lock_maintenance().unwrap().append(init, &ctx.actor()).unwrap();
    let mut drafts = Vec::new();
    let mut previous = None;
    for _ in 0..count {
        let id = crate::ids::new_id();
        std::fs::create_dir(p.state_path(&id)).unwrap();
        let mut record = DraftRecord::new("state");
        record.states.push(
            serde_json::from_value(serde_json::json!({
                "id":id,"kind":"commit","parents":previous.iter().collect::<Vec<_>>(),
                "subvol":"","author":{"name":"Test","email":"test@example.test"},"time":1700000000000i64
            }))
            .unwrap(),
        );
        drafts.push(record);
        previous = Some(id);
    }
    if let Some(head) = previous {
        let mut record = DraftRecord::new("line.create");
        record.lines.insert(
            crate::ids::new_id(),
            LineChange {
                transition: LineTransition::Create {
                    after: LineState { name: "main".into(), head, owner: ctx.caller.uid, machine: ctx.store.machine.id.clone() },
                },
                working_before: None,
                working_after: None,
            },
        );
        drafts.push(record);
    }
    p.lock_maintenance().unwrap().append_batch(&mut drafts, &ctx.actor()).unwrap();
    Repo { restore_from_remote: true, p, ctx: None }
}

#[test]
fn local_fetch_import_queries_are_bounded_and_forks_still_fail() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let repo = fixture(&ctx, "01a12290-0000-7000-8000-000000000002", 256);
    let lines = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
    let before = crate::db::RECORD_READS.with(std::cell::Cell::get);
    assert_eq!(repo.p.lock_maintenance().unwrap().import_lines(&lines, crate::store::Trust::Known).unwrap(), 0);
    let reads = crate::db::RECORD_READS.with(std::cell::Cell::get) - before;
    assert!(reads <= 2, "{reads} record lookups for {} existing records", lines.len());
    let mut fork = lines.clone();
    let signed = oplog::parse_line(fork.last().unwrap()).unwrap();
    let mut changed = signed.record;
    changed.time += 1;
    *fork.last_mut().unwrap() = oplog::sign_line(&changed, &ctx.store.machine.key).unwrap();
    assert!(repo
        .p
        .lock_maintenance()
        .unwrap()
        .import_lines(&fork, crate::store::Trust::Known)
        .unwrap_err()
        .to_string()
        .contains("forked history"));
}

#[test]
fn local_fetch_presence_is_one_lock_and_no_redundant_updates() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let mut repo = fixture(&ctx, "01a12290-0000-7000-8000-000000000002", 256);
    let lines = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
    let locks = crate::store::PROJECT_LOCK_COUNT.with(std::cell::Cell::get);
    let reads = crate::db::STATE_READS.with(std::cell::Cell::get);
    let changes = repo.p.db.test_connection().total_changes();
    let plan = crate::sync_local::Local::write(&ctx, &mut repo, None)
        .fetch_preflight(&lines, &Options::default(), false)
        .unwrap()
        .unwrap();
    assert!(plan.missing.is_empty());
    assert_eq!(crate::store::PROJECT_LOCK_COUNT.with(std::cell::Cell::get) - locks, 1);
    assert!(crate::db::STATE_READS.with(std::cell::Cell::get) - reads <= 1, "selection must bulk-read its states");
    assert_eq!(repo.p.db.test_connection().total_changes(), changes);
}

#[test]
fn local_fetch_reconciles_flags_and_rechecks_collected_trees() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let mut repo = fixture(&ctx, "01a12290-0000-7000-8000-000000000002", 4);
    let lines = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
    let ids = repo.p.db.state_auxiliary().unwrap().into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    repo.p
        .lock_maintenance()
        .unwrap()
        .write(|tx| {
            tx.set_present(&ids[0], false)?;
            tx.set_present(&ids[1], false)
        })
        .unwrap();
    let changes = repo.p.db.test_connection().total_changes();
    let locks = crate::store::PROJECT_LOCK_COUNT.with(std::cell::Cell::get);
    let mut local = crate::sync_local::Local::write(&ctx, &mut repo, None);
    assert!(local.fetch_preflight(&lines, &Options::default(), false).unwrap().is_some());
    assert_eq!(crate::store::PROJECT_LOCK_COUNT.with(std::cell::Cell::get) - locks, 1);
    assert_eq!(repo.p.db.test_connection().total_changes() - changes, 2, "only stale flags change");
    assert_eq!(repo.p.db.state_flags(&ids[0]).unwrap(), Some((true, false)));
    // Deterministically collect a tree after the planner saw it and before it obtains its lock.
    let path = repo.p.state_path(&ids[0]);
    let other_path = repo.p.state_path(&ids[1]);
    crate::sync_local::fetch::BEFORE_PRESENCE_RECHECK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || std::fs::remove_dir(path).unwrap()));
    });
    let mut local = crate::sync_local::Local::write(&ctx, &mut repo, None);
    assert!(local.fetch_preflight(&lines, &Options::default(), false).unwrap().is_none());
    // Inventory selection must restore a tree that vanished after planning as well.
    crate::sync_local::fetch::BEFORE_PRESENCE_RECHECK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || std::fs::remove_dir(other_path).unwrap()));
    });
    let plan = local.fetch_selection(&lines, &Options::default(), &ids, false).unwrap();
    let ordered = super::super::writer::parents_first(plan.missing).unwrap();
    assert_eq!(ordered[0].id, ids[0], "the collected ancestor must restore first");
    let missing = ordered.into_iter().map(|state| state.id).collect::<BTreeSet<_>>();
    assert_eq!(missing, BTreeSet::from([ids[0].clone(), ids[1].clone()]));
}

#[test]
fn local_fetch_selection_uses_one_catalog_snapshot() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let mut repo = fixture(&ctx, "01a12290-0000-7000-8000-000000000002", 1);
    let old = repo.p.db.line_by_name("main").unwrap().unwrap();
    let lines = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
    let state = crate::ids::new_id();
    let imported = state.clone();
    let writer_root = root.clone();
    crate::sync_local::fetch::AFTER_FETCH_CATALOG.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let writer_ctx = super::super::writer::tests::context(&writer_root);
            let dir = writer_ctx.store.project_dir("p").unwrap();
            let p = crate::store::Project {
                lease: Default::default(),
                store: &writer_ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: "01a12290-0000-7000-8000-000000000002".into(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            };
            let before = LineState { name: old.name, head: old.head.clone(), owner: old.owner, machine: old.machine };
            let after = LineState { head: imported.clone(), ..before.clone() };
            let mut record = DraftRecord::new("state");
            record.states.push(
                serde_json::from_value(serde_json::json!({
                    "id":imported,"kind":"commit","parents":[old.head],"subvol":"",
                    "author":{"name":"Test","email":"test@example.test"},"time":1700000000000i64
                }))
                .unwrap(),
            );
            record.lines.insert(
                old.id,
                LineChange {
                    transition: LineTransition::from_endpoints(Some(before), Some(after)),
                    working_before: None,
                    working_after: None,
                },
            );
            let mut reference = DraftRecord::new("ref.set");
            reference.data = serde_json::json!({"ref_change":{"name":"pins/imported","before":null,
                "after":{"name":"pins/imported","target":imported}}});
            p.lock_maintenance().unwrap().append_batch(&mut [record, reference], &writer_ctx.actor()).unwrap();
        }));
    });
    let mut local = crate::sync_local::Local::write(&ctx, &mut repo, None);
    let plan = local.fetch_preflight(&lines, &Options::default(), false).unwrap();
    assert!(plan.is_some(), "the plan must see the catalog's old refs and heads too");
    assert_eq!(repo.p.db.line_by_name("main").unwrap().unwrap().head, state);
    assert!(repo.p.db.test_connection().is_autocommit(), "selection must release its read snapshot");
    // A new selection sees the completed concurrent import and must request the missing tree.
    let mut local = crate::sync_local::Local::write(&ctx, &mut repo, None);
    assert!(local.fetch_preflight(&lines, &Options::default(), false).unwrap().is_none());
}

#[test]
fn local_fetch_import_rechecks_history_extended_since_discovery() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let repo = fixture(&ctx, "01a12290-0000-7000-8000-000000000002", 1);
    let mut discovered = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
    let next = repo.p.lock_maintenance().unwrap().append(DraftRecord::new("config"), &ctx.actor()).unwrap();
    assert_eq!(repo.p.lock_maintenance().unwrap().import_lines(&discovered, crate::store::Trust::Known).unwrap(), 0);
    let mut fork = next;
    fork.time += 1;
    discovered.push(oplog::sign_line(&fork, &ctx.store.machine.key).unwrap());
    assert!(repo
        .p
        .lock_maintenance()
        .unwrap()
        .import_lines(&discovered, crate::store::Trust::Known)
        .unwrap_err()
        .to_string()
        .contains("forked history"));
}

#[test]
fn local_fetch_bulk_ancestry_keeps_derived_from_edges() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let mut repo = fixture(&ctx, "01a12290-0000-7000-8000-000000000002", 1);
    let auto = crate::ids::new_id();
    let derived = crate::ids::new_id();
    let mut record = DraftRecord::new("fixture");
    for (id, kind, source) in [(&auto, "auto", None), (&derived, "derived", Some(&auto))] {
        std::fs::create_dir(repo.p.state_path(id)).unwrap();
        record.states.push(
            serde_json::from_value(serde_json::json!({
                "id":id,"kind":kind,"derived_from":source,"context":"test-v1",
                "subvol":"","author":{"name":"Test","email":"test@example.test"},"time":1700000000000i64
            }))
            .unwrap(),
        );
    }
    repo.p.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
    let mut record = DraftRecord::new("ref.set");
    record.data = serde_json::json!({"ref_change":{"name":"derived/test","before":null,
        "after":{"name":"derived/test","target":derived}}});
    repo.p.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
    std::fs::remove_dir(repo.p.state_path(&auto)).unwrap();
    let lines = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect::<Vec<_>>();
    let ids = repo.p.db.state_auxiliary().unwrap().into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    let plan =
        crate::sync_local::Local::write(&ctx, &mut repo, None).fetch_selection(&lines, &Options::default(), &ids, false).unwrap();
    assert_eq!(plan.missing.into_iter().map(|state| state.id).collect::<Vec<_>>(), vec![auto]);
}

#[test]
fn local_fetch_reader_decodes_linearly_and_preserves_order() {
    let (backend, identity, key, root) = super::tests::fixture();
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let lines = (1..=256)
        .map(|seq| {
            let mut record = oplog::parse_line(&super::tests::line(seq, &key)).unwrap().record;
            record.time = i64::try_from((seq * 73) % 257).unwrap();
            oplog::sign_line(&record, &key).unwrap()
        })
        .collect::<Vec<_>>();
    super::tests::segment(&backend, &identity, 1, 256, &lines);
    let seen = root.join("seen-records");
    records_with_local(&backend, &identity, super::tests::PROJECT, &[], Some((&seen, &key))).unwrap();
    let mut expected = lines.clone();
    expected.sort_by_cached_key(|line| oplog::parse_line(line).unwrap().record.time);
    let before = oplog::DECODE_COUNT.with(std::cell::Cell::get);
    let records = records_with_local(&backend, &identity, super::tests::PROJECT, &lines, Some((&seen, &key))).unwrap();
    let decodes = oplog::DECODE_COUNT.with(std::cell::Cell::get) - before;
    assert_eq!(records.lines, expected);
    assert!(decodes <= 2 * records.lines.len() + 2, "{decodes} decodes for {} records", records.lines.len());
}

/// An isolated local remote reproduction, also runnable under strace. No task line is published.
#[test]
fn local_fetch_noop_file_remote() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::super::writer::tests::context(&root);
    let mut remote = super::super::writer::tests::remote(&root, &ctx);
    let mut repo = fixture(&ctx, &remote.project, 256);
    remote
        .configure(&ctx, &crate::store_remote::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
        .unwrap();
    // Create only the signed segments: the no-op preflight needs no state objects.
    let descriptor = remote.descriptor().unwrap();
    let identity = remote.identity(&descriptor, None).unwrap();
    let lines = repo.p.export_jsonl().unwrap();
    let count = lines.lines().count();
    let key = format::segment_name(&ctx.store.machine.id, 1, u64::try_from(count).unwrap());
    remote.backend.put_new(&key, keys::encrypt(&keys::recipient_of(&identity).unwrap(), lines.as_bytes()).unwrap()).unwrap();
    for _ in 0..2 {
        let started = std::time::Instant::now();
        let report = fetch(&mut remote, &ctx, &mut repo, &Options::default()).unwrap();
        assert_eq!((report.records, report.states), (0, 0));
        eprintln!("local file remote, {count} records, 256 states: {:?}", started.elapsed());
    }
}

/// The signature checks and decodes of `run`.
fn counted<T>(run: impl FnOnce() -> T) -> (T, usize, usize) {
    let counters = || (oplog::VERIFY_COUNT.with(std::cell::Cell::get), oplog::DECODE_COUNT.with(std::cell::Cell::get));
    let before = counters();
    let value = run();
    let after = counters();
    (value, after.0 - before.0, after.1 - before.1)
}

/// A no-op push or fetch verifies no signature that an earlier command verified, and decodes each record at most once
/// per pass that needs it (push: the selection; fetch: the local index, the import and the selection). One new record
/// is the one verified. The same on a Secret store and on a Private one (no vault identity).
#[test]
fn noop_push_and_fetch_verify_only_records_that_are_new() {
    for (count, plain) in [(8, false), (64, false), (8, true)] {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let ctx = super::super::writer::tests::context(&root);
        let mut remote = super::super::writer::tests::remote(&root, &ctx);
        let mut repo = fixture(&ctx, &remote.project, count);
        if plain {
            super::super::writer::initialize_plain(&remote, format::Mode::Private).unwrap();
        } else {
            let options = crate::store_remote::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() };
            remote.configure(&ctx, &options).unwrap();
        }
        let records = repo.p.export_jsonl().unwrap().lines().count();
        let (_, verifies, _) = counted(|| super::super::writer::push(&remote, &ctx, &repo, &Options::default()).unwrap());
        assert_eq!(verifies, records, "the first push verifies the history once");
        let (_, verifies, _) = counted(|| fetch(&mut remote, &ctx, &mut repo, &Options::default()).unwrap());
        assert_eq!(verifies, records, "the first fetch verifies the history once");
        let (report, verifies, decodes) =
            counted(|| super::super::writer::push(&remote, &ctx, &repo, &Options::default()).unwrap());
        assert_eq!((report.records, report.states, verifies), (0, 0, 0));
        assert!(decodes <= records + 1, "push: {decodes} decodes for {records} records");
        let (report, verifies, decodes) = counted(|| fetch(&mut remote, &ctx, &mut repo, &Options::default()).unwrap());
        assert_eq!((report.records, report.states, verifies), (0, 0, 0));
        assert!(decodes <= 3 * records + 3, "fetch: {decodes} decodes for {records} records");
        let mut record = DraftRecord::new("config");
        record.data = serde_json::json!({"config": {"note": "one more"}});
        repo.p.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
        let (report, verifies, _) = counted(|| super::super::writer::push(&remote, &ctx, &repo, &Options::default()).unwrap());
        assert_eq!((report.records, verifies), (1, 1));
        let (_, verifies, _) = counted(|| fetch(&mut remote, &ctx, &mut repo, &Options::default()).unwrap());
        assert_eq!(verifies, 1);
        crate::free::finish(&ctx);
    }
}

/// One derived state with a `derived/` ref and one automatic state per commit of `repo`. The automatic states are
/// here only as objects (`layr migrate` retired their copies): their tree and times are in the object store.
fn add_refs_and_object_only_autos(ctx: &Ctx, repo: &Repo) {
    let commits: Vec<String> = repo.p.db.state_rows().unwrap().into_iter().map(|(state, _)| state.id).collect();
    let objects = crate::objstore::open(&repo.p).unwrap();
    let tree = objects.put_tree(&Default::default()).unwrap().hex();
    let times = objects.put_times(&layr_objects::times::Times { own: 0, records: Vec::new() }).unwrap().hex();
    let author = serde_json::json!({"name":"Test","email":"test@example.test"});
    let mut drafts = Vec::new();
    let mut autos = Vec::new();
    for commit in &commits {
        let (derived, auto) = (crate::ids::new_id(), crate::ids::new_id());
        std::fs::create_dir(repo.p.state_path(&derived)).unwrap();
        let mut record = DraftRecord::new("attest");
        record.states.push(
            serde_json::from_value(serde_json::json!({"id":derived,"kind":"derived","derived_from":commit,
                "context":"check-v1","subvol":"","author":author,"time":1700000000001i64}))
            .unwrap(),
        );
        let name = format!("derived/check-v1/{derived}");
        record.data = serde_json::json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":derived}}});
        drafts.push(record);
        let mut record = DraftRecord::new("save");
        record.states.push(
            serde_json::from_value(serde_json::json!({"id":auto,"kind":"auto","parents":[commit],"subvol":"",
                "author":author,"time":1700000000002i64,"tree":tree,"times":times}))
            .unwrap(),
        );
        drafts.push(record);
        autos.push(auto);
    }
    repo.p.lock_maintenance().unwrap().append_batch(&mut drafts, &ctx.actor()).unwrap();
    for auto in autos {
        repo.p.db.test_connection().execute("UPDATE states SET present=0, deleted=0 WHERE id=?1", [&auto]).unwrap();
    }
}

/// A no-op push selects its candidates from one read of the state catalog and one open of the object store, whatever
/// the number of states, refs and object-only states: before, it read each ref target, each state of the history and
/// each candidate again, and opened the object store once per object-only state.
#[test]
fn noop_push_selection_reads_the_state_catalog_once() {
    for count in [8, 64] {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let ctx = super::super::writer::tests::context(&root);
        let mut remote = super::super::writer::tests::remote(&root, &ctx);
        let repo = fixture(&ctx, &remote.project, count);
        add_refs_and_object_only_autos(&ctx, &repo);
        // One more automatic state names a times table that this machine never had: it is no candidate.
        let tree = crate::objstore::open(&repo.p).unwrap().put_tree(&Default::default()).unwrap().hex();
        let elsewhere = crate::ids::new_id();
        let mut record = DraftRecord::new("save");
        record.states.push(
            serde_json::from_value(serde_json::json!({"id":elsewhere,"kind":"auto","parents":[],"subvol":"",
                "author":{"name":"Test","email":"test@example.test"},"time":1700000000003i64,
                "tree":tree,"times":"12".repeat(32)}))
            .unwrap(),
        );
        repo.p.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
        repo.p.db.test_connection().execute("UPDATE states SET present=0 WHERE id=?1", [&elsewhere]).unwrap();
        let options = crate::store_remote::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() };
        remote.configure(&ctx, &options).unwrap();
        // The options of `layr push <remote> --overlay main`.
        let opts = Options { caches: true, overlays: Some(["main".to_string()].into()), ..Default::default() };
        let first = super::super::writer::push(&remote, &ctx, &repo, &opts).unwrap();
        assert_eq!(first.states, 2 * count, "the commits and the derived states go up");
        let counters = || {
            (
                crate::db::STATE_READS.with(std::cell::Cell::get),
                crate::db::CATALOG_READS.with(std::cell::Cell::get),
                crate::objstore::OPENS.with(std::cell::Cell::get),
            )
        };
        let before = counters();
        let report = super::super::writer::push(&remote, &ctx, &repo, &opts).unwrap();
        let after = counters();
        let (reads, catalogs, opens) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
        assert_eq!((report.records, report.states), (0, 0));
        let autos: Vec<String> = repo
            .p
            .db
            .object_only_states()
            .unwrap()
            .into_iter()
            .filter(|id| *id != elsewhere)
            .map(|id| format!("{id} \"auto\""))
            .collect();
        assert_eq!((autos.len(), &report.omitted, &first.omitted), (count, &autos, &autos), "omitted in catalog order");
        assert!(reads <= 2, "{reads} state reads for {} states and {count} refs", 3 * count);
        assert_eq!(catalogs, 1, "one read of the whole state catalog");
        assert!(opens <= 1, "{opens} object store opens for {count} object-only states");
        // Every state on the remote: no object-only state is left to look up, and the object store stays closed.
        let lines: Vec<String> = repo.p.export_jsonl().unwrap().lines().map(str::to_owned).collect();
        let every = repo.p.db.state_rows().unwrap().into_iter().map(|(state, _)| state.id).collect();
        let opens = crate::objstore::OPENS.with(std::cell::Cell::get);
        let local = crate::sync_local::Local::read(&ctx, &repo);
        assert!(local.push_candidates(&lines, &opts, &every, &mut Vec::new()).unwrap().is_empty());
        assert_eq!(crate::objstore::OPENS.with(std::cell::Cell::get), opens);
        // An object-only state counts only with both its tree and its times table here.
        let held = repo.p.db.object_only_states().unwrap().into_iter().find(|id| *id != elsewhere).unwrap();
        assert!(crate::objstore::verified(&repo.p, &held).unwrap().is_some());
        assert!(crate::objstore::verified(&repo.p, &elsewhere).unwrap().is_none());
        let objects = crate::objstore::open(&repo.p).unwrap();
        let record = |id: &str| repo.p.db.state(id).unwrap().unwrap();
        assert!(crate::objstore::recorded(&objects, &record(&held)).unwrap().is_some());
        assert!(crate::objstore::recorded(&objects, &record(&elsewhere)).unwrap().is_none());
        // `--line main` sends the history of main only: the commits, not the derived states that the refs keep.
        let line = Options { lines: vec!["main".into()], ..opts.clone() };
        let commits: std::collections::BTreeSet<String> = repo
            .p
            .db
            .state_rows()
            .unwrap()
            .into_iter()
            .filter(|(state, _)| state.kind == crate::model::StateKind::Commit)
            .map(|(state, _)| state.id)
            .collect();
        let selected = local.push_candidates(&lines, &line, &Default::default(), &mut Vec::new()).unwrap();
        assert_eq!(selected.into_iter().map(|state| state.id).collect::<std::collections::BTreeSet<_>>(), commits);
        crate::free::finish(&ctx);
    }
}
