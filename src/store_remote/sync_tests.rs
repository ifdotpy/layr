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
    trusted: bool,
    reject: bool,
    selected_heads: bool,
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
            missing: self.states.iter().filter(|state| available.contains(&state.id) && self.installed == 0).cloned().collect(),
            left: Vec::new(),
            omitted: Vec::new(),
            clone_lines: clone.then(Vec::new),
            restricted: opts.heads,
        })
    }
    fn push_candidates(
        &self,
        _: &[String],
        _: &Options,
        present: &BTreeSet<String>,
        _: &mut Vec<String>,
    ) -> Result<Vec<StateRec>> {
        Ok(self.states.iter().filter(|s| !present.contains(&s.id)).cloned().collect())
    }
    fn omitted_cache_refs(&self, _: &Options) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn tips(&self) -> Result<BTreeSet<String>> {
        Ok(self.states.iter().map(|s| s.id.clone()).collect())
    }
    fn ancestry(&self, id: &str) -> Result<Vec<String>> {
        Ok(vec![id.into()])
    }
    fn pin_state(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn check_secrets(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn has_state(&self, _: &str) -> bool {
        self.installed > 0
    }
    fn is_overlay(&self) -> bool {
        false
    }
    fn open_state(&self, _: &str) -> Result<Box<dyn StateRead + '_>> {
        Ok(Box::new(FakeRead { root: self.root.clone() }))
    }
    fn restore_base(&self, _: &StateRec, _: &BTreeMap<String, String>) -> Option<String> {
        None
    }
    fn stage_state(&mut self, _: Option<&str>) -> Result<PathBuf> {
        let stage = self.root.join("stage");
        std::fs::create_dir_all(&stage)?;
        self.stage = Some(stage.clone());
        Ok(stage)
    }
    fn install_state(&mut self, _: &StateRec, _: u64, files: usize) -> Result<usize> {
        self.installed += 1;
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
fn fixture(root: &Path, machine_id: &str, key: &ed25519_dalek::SigningKey) -> Result<FakeLocal> {
    let state: StateRec = serde_json::from_value(serde_json::json!({
        "id":"01a12290-0000-7000-8000-000000000003", "kind":"commit", "parents":[],
        "subvol":"01a12290-0000-7000-8000-000000000004", "author":{"name":"Fixture","email":"fixture@example.test"},
        "time":1, "message":"port fixture"
    }))?;
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join("notes.txt"), b"portable bytes\n")?;
    let mut machine = crate::model::Record::untrusted_preview("machine");
    machine.data = serde_json::json!({"name":"fixture","public_key":hex::encode(key.verifying_key().to_bytes())});
    let mut project = crate::model::Record::untrusted_preview("project");
    project.data = serde_json::json!({"id":"01a12290-0000-7000-8000-000000000002","name":"fixture","config":{}});
    let mut commit = crate::model::Record::untrusted_preview("commit");
    commit.states.push(state.clone());
    let mut records = Vec::new();
    for (seq, mut record) in [machine, project, commit].into_iter().enumerate() {
        record.id = crate::ids::new_id();
        record.project = "01a12290-0000-7000-8000-000000000002".into();
        record.machine = machine_id.into();
        record.seq = u64::try_from(seq)? + 1;
        records.push(crate::oplog::sign_line(&record, key)?);
    }
    Ok(FakeLocal { records, states: vec![state], root: root.to_owned(), ..Default::default() })
}

#[test]
fn fake_local_round_trip_preserves_signed_bytes_selection_and_publication() -> Result<()> {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir(&root)?;
    let cleanup = crate::repo::Temp::plain(root.clone());
    let ctx = super::writer::tests::context(&root);
    let mut store = super::writer::tests::remote(&root, &ctx);
    super::writer::initialize_plain(&store, super::format::Mode::Private)?;
    let mut sender = fixture(&root.join("source"), &store.machine, &store.key)?;
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
    let records = fixture(&root.join("fixture-source"), &ctx.store.machine.id, &ctx.store.machine.key)?.records;
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
