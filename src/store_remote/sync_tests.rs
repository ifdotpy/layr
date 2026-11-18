use super::Options;
use crate::{
    model::{Line, StateRec},
    repo::Repo,
    sync_local::Local,
};
use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_store::sync::LocalReplica;
use layr_store::sync::{FetchSelection, Imported, StateRead};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Default)]
struct FakeLocal {
    records: Vec<String>,
    states: Vec<StateRec>,
    root: PathBuf,
    stage: Option<PathBuf>,
    installed: usize,
    present: BTreeSet<String>,
    trusted: bool,
    reject: bool,
    selected_heads: bool,
    /// States that the walks of the ancestry visited.
    ancestry_walks: std::cell::Cell<usize>,
    /// States of the history that a push does not send.
    withheld: BTreeSet<String>,
    /// Stages that fail with a full disk before one succeeds.
    full_stages: usize,
    _coordinator_only: std::rc::Rc<()>,
}
impl LocalReplica for FakeLocal {
    type State = StateRec;
    type Line = Line;
    type Selection = Options;
    fn visit_records(&self, machine: Option<(&str, u64)>, visit: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
        for line in &self.records {
            let record = crate::oplog::parse_line(line)?.record;
            if machine.is_none_or(|(id, after)| record.machine == id && record.seq > after) {
                visit(line)?;
            }
        }
        Ok(())
    }
    fn record_machines(&self) -> Result<Vec<String>> {
        Ok(self
            .records
            .iter()
            .map(|line| crate::oplog::parse_line(line).map(|r| r.record.machine))
            .collect::<Result<BTreeSet<_>>>()?
            .into_iter()
            .collect())
    }
    fn record_line(&self, machine: &str, seq: u64) -> Result<Option<String>> {
        for line in &self.records {
            let record = crate::oplog::parse_line(line)?.record;
            if record.machine == machine && record.seq == seq {
                return Ok(Some(line.clone()));
            }
        }
        Ok(None)
    }
    fn verify_records(&self) -> Result<()> {
        Ok(())
    }
    fn import_records(&mut self, lines: &[String], _: &BTreeMap<String, VerifyingKey>) -> Result<Imported> {
        ensure!(!self.reject, "untrusted writer");
        let bootstrap = self.records.is_empty();
        self.records = lines.to_vec();
        Ok(Imported { count: lines.len(), bootstrap, project: "01a12290-0000-7000-8000-000000000002".into() })
    }
    fn finish_import(&mut self, _: &BTreeMap<String, VerifyingKey>) -> Result<()> {
        self.trusted = true;
        Ok(())
    }
    fn fetch_selection(
        &mut self,
        _: &[String],
        opts: &Options,
        available: &[String],
        clone: bool,
    ) -> Result<FetchSelection<StateRec, Line>> {
        self.selected_heads = opts.heads;
        Ok(FetchSelection {
            missing: self
                .states
                .iter()
                .filter(|state| available.contains(&state.id) && !self.has_state(&state.id))
                .cloned()
                .collect(),
            left: Vec::new(),
            omitted: Vec::new(),
            clone_lines: clone.then(Vec::new),
            restricted: opts.heads,
        })
    }
    fn fetch_preflight(
        &mut self,
        records: &[String],
        opts: &Options,
        clone: bool,
    ) -> Result<Option<FetchSelection<StateRec, Line>>> {
        ensure!(records == self.records, "fetch must import signed records before preflight");
        let complete = self.states.iter().all(|state| self.present.contains(&state.id));
        let available = self.present.iter().cloned().collect::<Vec<_>>();
        let plan = self.fetch_selection(records, opts, &available, clone)?;
        Ok(complete.then_some(plan))
    }
    fn fetch_temp_dir(&self) -> Option<PathBuf> {
        std::fs::create_dir_all(&self.root).ok()?;
        Some(self.root.join("material"))
    }
    fn push_candidates(
        &self,
        _: &[String],
        _: &Options,
        present: &BTreeSet<String>,
        _: &mut Vec<String>,
    ) -> Result<Vec<StateRec>> {
        Ok(self.states.iter().filter(|s| !present.contains(&s.id) && !self.withheld.contains(&s.id)).cloned().collect())
    }
    fn omitted_cache_refs(&self, _: &Options) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn tips(&self) -> Result<BTreeSet<String>> {
        Ok(self.states.iter().map(|s| s.id.clone()).collect())
    }
    fn nearest_ancestor(&self, id: &str, wanted: &mut dyn FnMut(&str) -> bool) -> Result<Option<String>> {
        let parents = |id: &str| self.states.iter().find(|s| s.id == id).map(|s| s.parents.clone()).unwrap_or_default();
        let (mut queue, mut seen) = (std::collections::VecDeque::from(parents(id)), BTreeSet::new());
        while let Some(at) = queue.pop_front() {
            if !seen.insert(at.clone()) {
                continue;
            }
            self.ancestry_walks.set(self.ancestry_walks.get() + 1);
            if wanted(&at) {
                return Ok(Some(at));
            }
            queue.extend(parents(&at));
        }
        Ok(None)
    }
    fn pin_state(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn check_secrets(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn has_state(&self, id: &str) -> bool {
        self.present.contains(id)
    }
    fn is_overlay(&self) -> bool {
        false
    }
    fn open_state(&self, id: &str) -> Result<Box<dyn StateRead + '_>> {
        let root = self.root.join(id);
        Ok(Box::new(FakeRead { root: if root.is_dir() { root } else { self.root.clone() } }))
    }
    fn restore_base(&self, state: &StateRec, available: &BTreeMap<String, String>) -> Option<String> {
        state.parents.iter().chain(&state.derived_from).find(|id| self.has_state(id) && available.contains_key(*id)).cloned()
    }
    fn stage_state(&mut self, _: Option<&str>) -> Result<PathBuf> {
        if self.full_stages > 0 {
            self.full_stages -= 1;
            return Err(std::io::Error::from(std::io::ErrorKind::StorageFull).into());
        }
        let stage = self.root.join("stage");
        std::fs::create_dir_all(&stage)?;
        self.stage = Some(stage.clone());
        Ok(stage)
    }
    fn install_state(&mut self, state: &StateRec, _: u64, files: usize) -> Result<usize> {
        self.installed += 1;
        self.present.insert(state.id.clone());
        Ok(files)
    }
    fn abort_state(&mut self, error: anyhow::Error) -> anyhow::Error {
        error
    }
    fn git_has_store(&self) -> bool {
        false
    }
    fn git_init(&self) -> Result<()> {
        Ok(())
    }
    fn git_imported(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn git_has_commits(&self, commits: &[String]) -> Result<Vec<bool>> {
        Ok(vec![false; commits.len()])
    }
    fn git_outside(&self, _: &[String], _: &[String]) -> Result<Vec<String>> {
        bail!("no Git history")
    }
    fn git_pack(&self, _: &[String], _: &[String]) -> Result<Vec<u8>> {
        bail!("no Git history")
    }
    fn git_install(&self, _: &[u8]) -> Result<()> {
        bail!("no Git history")
    }
}
struct FakeRead {
    root: PathBuf,
}
impl StateRead for FakeRead {
    fn legacy_root(&self) -> &Path {
        &self.root
    }
}
fn fixture(root: &Path, machine_id: &str, key: &ed25519_dalek::SigningKey, count: usize) -> Result<FakeLocal> {
    let state: StateRec = serde_json::from_value(serde_json::json!({
        "id":"01a12290-0000-7000-8000-000000000003", "kind":"commit", "parents":[],
        "subvol":"01a12290-0000-7000-8000-000000000004", "author":{"name":"Fixture","email":"fixture@example.test"},
        "time":1, "message":"port fixture"
    }))?;
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join("notes.txt"), b"portable bytes\n")?;
    let states = (0..count)
        .map(|n| {
            if n == 0 {
                state.clone()
            } else {
                StateRec { id: format!("01a12290-0000-7000-8000-{:012x}", n + 10), ..state.clone() }
            }
        })
        .collect::<Vec<_>>();
    if count > 1 {
        for (n, state) in states.iter().enumerate() {
            let path = root.join(&state.id);
            std::fs::create_dir(&path)?;
            std::fs::write(path.join("notes.txt"), format!("source {n}\n"))?;
        }
    }
    let mut machine = crate::model::Record::untrusted_preview("machine");
    machine.data = serde_json::json!({"name":"fixture","public_key":hex::encode(key.verifying_key().to_bytes())});
    let mut project = crate::model::Record::untrusted_preview("project");
    project.data = serde_json::json!({"id":"01a12290-0000-7000-8000-000000000002","name":"fixture","config":{}});
    let mut commit = crate::model::Record::untrusted_preview("commit");
    commit.states = states.clone();
    let mut records = Vec::new();
    for (seq, mut record) in [machine, project, commit].into_iter().enumerate() {
        record.id = crate::ids::new_id();
        record.project = "01a12290-0000-7000-8000-000000000002".into();
        record.machine = machine_id.into();
        record.seq = u64::try_from(seq)? + 1;
        records.push(crate::oplog::sign_line(&record, key)?);
    }
    Ok(FakeLocal { records, states, root: root.to_owned(), ..Default::default() })
}

#[test]
fn fake_local_round_trip_preserves_signed_bytes_selection_and_publication() -> Result<()> {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir(&root)?;
    let cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::writer::tests::context(&root);
    let mut store = super::writer::tests::remote(&root, &ctx);
    super::writer::initialize_plain(&store, super::format::Mode::Private)?;
    let mut sender = fixture(&root.join("source"), &store.machine, &store.key, 1)?;
    let opts = Options { heads: true, ..Default::default() };
    let pushed = super::writer::push_with_local(&store, &mut sender, &opts)?;
    assert_eq!((pushed.records, pushed.states, pushed.files), (3, 1, 1));
    let mut receiver = FakeLocal { root: root.join("destination"), states: sender.states.clone(), ..Default::default() };
    let fetched = super::reader::fetch_with_local(&mut store, &mut receiver, &opts)?;
    assert_eq!((fetched.records, fetched.states, fetched.files), (3, 1, 1));
    assert_eq!(receiver.records, sender.records);
    assert!(receiver.trusted && receiver.selected_heads);
    assert_eq!(receiver.installed, 1);
    assert_eq!(std::fs::read(receiver.stage.as_ref().context("missing stage")?.join("notes.txt"))?, b"portable bytes\n");
    let again = super::reader::fetch_with_local(&mut store, &mut receiver, &opts)?;
    assert_eq!(again.states, 0);
    let mut rejected = FakeLocal { reject: true, ..Default::default() };
    assert!(super::reader::fetch_with_local(&mut store, &mut rejected, &opts).is_err());
    assert_eq!(rejected.installed, 0);
    cleanup.discard()
}

#[test]
fn record_import_releases_the_project_lock() -> Result<()> {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir(&root)?;
    let cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::writer::tests::context(&root);
    let project = crate::store::Project::create_empty(&ctx.store, "fixture")?;
    let lock = project.meta_dir().join("lock");
    let mut repo = Repo { restore_from_remote: true, p: project, ctx: Some(&ctx) };
    let records = fixture(&root.join("fixture-source"), &ctx.store.machine.id, &ctx.store.machine.key, 1)?.records;
    let keys = BTreeMap::from([(ctx.store.machine.id.clone(), ctx.store.machine.key.verifying_key())]);
    let mut local = Local::write(&ctx, &mut repo, None);
    assert!(local.finish_import(&BTreeMap::new()).is_err());
    local.import_records(&records, &keys)?;
    local.visit_records(None, &mut |_| Ok(()))?;
    // A transport wait after import cannot retain the local project lock.
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || -> Result<()> {
        let file = std::fs::OpenOptions::new().write(true).open(lock)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        send.send(()).map_err(|error| anyhow::anyhow!(error))?;
        Ok(())
    });
    let result = receive.recv().map_err(|error| anyhow::anyhow!(error));
    worker.join().map_err(|_| anyhow::anyhow!("lock worker panicked"))??;
    result?;
    // The overlayfs build keeps a project mount under the fixture until the process ends, so a folder
    // that cannot be removed yet is not a failure of this test.
    let _ = cleanup.discard();
    Ok(())
}

#[cfg(test)]
mod fetch_regressions {
    use super::*;
    use crate::store_remote::{format, gateway, reader, writer, Store};

    fn native_fetch_fixture(count: usize, merge: bool) -> Result<(Store, FakeLocal, crate::repo::Temp)> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root)?;
        let cleanup = crate::repo::Temp::plain(root.clone());
        let ctx = writer::tests::context(&root);
        let store = writer::tests::remote(&root, &ctx);
        writer::initialize_plain(&store, format::Mode::Private)?;
        let mut sender = fixture(&root.join("source"), &store.machine, &store.key, count)?;
        if merge {
            sender.states[2].parents = sender.states[..2].iter().map(|s| s.id.clone()).collect();
        }
        let mut commit = crate::oplog::parse_line(&sender.records[2])?.record;
        commit.states = sender.states.clone();
        sender.records[2] = crate::oplog::sign_line(&commit, &store.key)?;
        writer::push_with_local(&store, &mut sender, &Options::default())?;
        Ok((store, sender, cleanup))
    }

    #[test]
    fn native_fetch_batches_252_states_and_noop_uses_no_state_inventory() -> Result<()> {
        let (mut store, sender, cleanup) = native_fetch_fixture(252, false)?;
        let (backend, peer) = gateway::tests::file_transport(store.backend.clone());
        store.backend = backend;
        let mut receiver =
            FakeLocal { states: sender.states.clone(), root: cleanup.path.join("destination"), ..Default::default() };
        let before = store.backend.transfer_stats();
        let cold = reader::fetch_with_local(&mut store, &mut receiver, &Options::default())?;
        let after = store.backend.transfer_stats();
        assert_eq!((cold.states, cold.files, receiver.installed), (252, 252, 252));
        assert_eq!(cold.bytes, after.downloaded - before.downloaded);
        assert!(cold.bytes > 0);
        let warm = reader::fetch_with_local(&mut store, &mut receiver, &Options::default())?;
        assert_eq!((warm.states, warm.files), (0, 0));
        let log = peer.finish();
        let (cold_log, warm_log) = log.split_at(usize::try_from(after.requests - before.requests)?);
        // The listing gives the sizes of the parts: 252 small parts fit one window of half the operation cache.
        assert_eq!(cold_log.len(), 7, "{cold_log:?}");
        let batches = cold_log.iter().filter(|(r, _)| r.ends_with("/batch")).count();
        assert_eq!(batches, 3, "records, trees and parts batch once each");
        assert_eq!(warm_log.len(), 3, "descriptor, records listing, git listing: {warm_log:?}");
        assert!(!warm_log.iter().any(|(r, _)| r.contains("states") || r.ends_with("/batch")));
        eprintln!("fetch_with_local 252 states: {} cold requests ({batches} payload batches), 3 warm requests", cold_log.len());
        cleanup.discard()
    }

    /// A disk that the operation cache filled: the fetch drops the cache and stages the state once more without it,
    /// whether the stage, a folder or a file of it found the disk full.
    #[test]
    fn a_full_disk_while_staging_drops_the_operation_cache_and_retries() -> Result<()> {
        for point in ["stage", "mkdir", "create"] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir(&root)?;
            let cleanup = crate::repo::Temp::plain(root.clone());
            let ctx = writer::tests::context(&root);
            let mut store = writer::tests::remote(&root, &ctx);
            writer::initialize_plain(&store, format::Mode::Private)?;
            let mut sender = fixture(&root.join("source"), &store.machine, &store.key, 1)?;
            std::fs::create_dir(root.join("source/folder"))?;
            std::fs::write(root.join("source/folder/nested.txt"), b"nested bytes\n")?;
            writer::push_with_local(&store, &mut sender, &Options::default())?;
            let mut receiver = FakeLocal {
                root: root.join("destination"),
                states: sender.states.clone(),
                full_stages: usize::from(point == "stage"),
                ..Default::default()
            };
            crate::tree::NO_SPACE.with(|next| next.set(Some(point).filter(|point| *point != "stage")));
            let fetched = reader::fetch_with_local(&mut store, &mut receiver, &Options::default());
            crate::tree::NO_SPACE.with(|next| next.set(None));
            assert_eq!(fetched.with_context(|| format!("full disk at {point}"))?.states, 1);
            let stage = receiver.stage.as_ref().context("missing stage")?;
            assert_eq!(std::fs::read(stage.join("notes.txt"))?, b"portable bytes\n");
            assert_eq!(std::fs::read(stage.join("folder/nested.txt"))?, b"nested bytes\n");
            cleanup.discard()?;
        }
        Ok(())
    }

    /// A chain of new states on a pushed one: each base is the parent that the push sends just before, so no state
    /// walks its ancestry (before, the cold-base pass walked it once per descendant).
    #[test]
    fn a_pushed_chain_walks_no_ancestry() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root)?;
        let cleanup = crate::repo::Temp::plain(root.clone());
        let ctx = writer::tests::context(&root);
        let store = writer::tests::remote(&root, &ctx);
        writer::initialize_plain(&store, format::Mode::Private)?;
        let mut sender = fixture(&root.join("source"), &store.machine, &store.key, 5)?;
        for n in 1..sender.states.len() {
            sender.states[n].parents = vec![sender.states[n - 1].id.clone()];
        }
        let mut commit = crate::oplog::parse_line(&sender.records[2])?.record;
        commit.states = sender.states.clone();
        sender.records[2] = crate::oplog::sign_line(&commit, &store.key)?;
        let all = std::mem::take(&mut sender.states);
        sender.states = all[..1].to_vec();
        writer::push_with_local(&store, &mut sender, &Options::default())?;
        sender.states = all;
        assert_eq!(writer::push_with_local(&store, &mut sender, &Options::default())?.states, 4);
        assert_eq!(sender.ancestry_walks.get(), 0);
        cleanup.discard()
    }

    /// A history whose every other state a push does not send: each base lookup walks to the nearest state that the
    /// push sends or the remote holds, not the whole history (before, each state walked all of it, twice).
    #[test]
    fn a_sparse_history_walks_each_ancestor_a_bounded_number_of_times() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root)?;
        let cleanup = crate::repo::Temp::plain(root.clone());
        let ctx = writer::tests::context(&root);
        let store = writer::tests::remote(&root, &ctx);
        writer::initialize_plain(&store, format::Mode::Private)?;
        let mut sender = fixture(&root.join("source"), &store.machine, &store.key, 81)?;
        for n in 1..sender.states.len() {
            sender.states[n].parents = vec![sender.states[n - 1].id.clone()];
        }
        let mut commit = crate::oplog::parse_line(&sender.records[2])?.record;
        commit.states = sender.states.clone();
        sender.records[2] = crate::oplog::sign_line(&commit, &store.key)?;
        // The remote holds the root; the states after it alternate between sent and withheld.
        sender.withheld = sender.states[1..].iter().map(|s| s.id.clone()).collect();
        writer::push_with_local(&store, &mut sender, &Options::default())?;
        sender.withheld = sender.states.iter().skip(1).step_by(2).map(|s| s.id.clone()).collect();
        assert_eq!(writer::push_with_local(&store, &mut sender, &Options::default())?.states, 40);
        let walked = sender.ancestry_walks.get();
        eprintln!("40 states on withheld parents: {walked} ancestors visited");
        assert!(walked <= 4 * 40, "{walked} ancestors visited");
        cleanup.discard()
    }

    #[test]
    fn native_fetch_ignores_a_corrupt_unused_merge_parent() -> Result<()> {
        let (mut store, sender, cleanup) = native_fetch_fixture(3, true)?;
        let second = &sender.states[1].id;
        let attempts = reader::attempts(&store.backend)?;
        store.backend.delete(&format::tree_name(second, &attempts[second]))?;
        store.backend.put_new(&format::tree_name(second, &attempts[second]), b"bad unused tree".to_vec())?;
        let (backend, peer) = gateway::tests::file_transport(store.backend.clone());
        store.backend = backend;
        let mut receiver = FakeLocal {
            records: sender.records.clone(),
            states: sender.states.clone(),
            root: cleanup.path.join("destination"),
            present: sender.states[..2].iter().map(|s| s.id.clone()).collect(),
            ..Default::default()
        };
        let report = reader::fetch_with_local(&mut store, &mut receiver, &Options::default())?;
        assert_eq!(report.states, 1);
        assert!(receiver.present.contains(&sender.states[2].id));
        assert!(!peer
            .finish()
            .iter()
            .filter(|(r, _)| r.ends_with("/batch"))
            .flat_map(|(_, v)| v["keys"].as_array().into_iter().flatten())
            .any(|key| key.as_str().is_some_and(|key| key.contains(second))));
        cleanup.discard()
    }
}
