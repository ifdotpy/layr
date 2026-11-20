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
    records_with_local(&backend, &identity, super::tests::PROJECT, &[], Some(&seen)).unwrap();
    let mut expected = lines.clone();
    expected.sort_by_cached_key(|line| oplog::parse_line(line).unwrap().record.time);
    let before = oplog::DECODE_COUNT.with(std::cell::Cell::get);
    let records = records_with_local(&backend, &identity, super::tests::PROJECT, &lines, Some(&seen)).unwrap();
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
