//! Move a private APFS index into a state without copying it. The project lock is required.
//! The durable journal protects the only staged copy until the record transaction commits.
use crate::{fsutil, ids, model, storage, store::Project};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    project: String,
    line: String,
    state: String,
    subvol: String,
    device: u64,
    inode: u64,
    mode: u32,
    /// The index is a thin layer: the state goes to `deltas/<id>`.
    #[serde(default)]
    thin: bool,
}

fn journal_path(p: &Project) -> PathBuf {
    p.meta_dir().join("apfs-index-commit.json")
}

/// True when a save has a journal to recover: a pending folder, or a journal of the old commit.
pub fn pending(p: &Project) -> Result<bool> {
    if exists(&journal_path(p))? {
        return Ok(true);
    }
    Ok(fs::read_dir(pending_root(p)).map(|mut rd| rd.next().is_some()).unwrap_or(false))
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn read(p: &Project) -> Result<Option<Journal>> {
    if !exists(&journal_path(p))? {
        return Ok(None);
    }
    let file = crate::tree::Tree::open(&p.meta_dir())?.open_read("apfs-index-commit.json")?;
    let j: Journal = serde_json::from_reader(file.take(65536)).context("read APFS index state journal")?;
    if j.version != 1
        || j.project != p.id
        || !model::is_uuid(&j.line)
        || !model::is_uuid(&j.state)
        || !model::is_uuid(&j.subvol)
        || j.mode & !0o7777 != 0
    {
        bail!("invalid APFS index state journal");
    }
    Ok(Some(j))
}

/// A failed record must not delete the only staged copy. Recovery owns this state.
pub fn preserves(p: &Project, state: &str) -> Result<bool> {
    if read(p)?.is_some_and(|j| j.state == state) {
        return Ok(true);
    }
    Ok(pending_journals(p)?.iter().any(|j| j.state == state))
}

fn target(p: &Project, j: &Journal) -> PathBuf {
    if j.thin {
        p.delta_path(&j.state)
    } else {
        p.state_path(&j.state)
    }
}

fn verify_tree(path: &Path, j: &Journal) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.dev() != j.device || m.ino() != j.inode || storage::subvol_info(path)?.uuid != j.subvol {
        bail!("APFS index state tree changed: {}", path.display());
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}

fn move_tree(src: &Path, dst: &Path, mode: u32) -> Result<()> {
    // An immutable root cannot be renamed. Children can remain immutable.
    crate::sys::apfs_immutable(src, false)?;
    fs::set_permissions(src, fs::Permissions::from_mode(mode | 0o200))?;
    crate::sys::apfs_publish(src, dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
    sync_parent(src)?;
    sync_parent(dst)?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Reconcile the saves in progress with the database, with the project lock held: the pending
/// folders of `save -m`, and the journal of the commit before them.
pub fn recover(p: &Project) -> Result<()> {
    recover_legacy(p)?;
    for (dir, journal) in pending_entries(p)? {
        match journal {
            // A save of a live process (this one too) is not recovered: it is in progress.
            Some(j) if crate::free::holder_alive(&j.holder) => {}
            Some(j) => recover_pending(p, &dir, &j)?,
            // No readable journal: the folder cannot be a save in progress.
            None => p.trash(&dir)?,
        }
    }
    Ok(())
}

/// The journal of the commit before the pending folders. A missing record restores the index; a
/// committed record retains the immutable state.
fn recover_legacy(p: &Project) -> Result<()> {
    let Some(j) = read(p)? else {
        return Ok(());
    };
    let stage = p.stage_path(&j.line);
    let state = target(p, &j);
    if let Some(rec) = p.db.state(&j.state)? {
        if rec.subvol != j.subvol || rec.line.as_deref() != Some(&j.line) || exists(&stage)? {
            bail!("APFS index state record does not match its journal");
        }
        verify_tree(&state, &j)?;
        if !storage::subvol_info(&state)?.readonly {
            bail!("saved APFS index is not immutable");
        }
        remove_if_present(&stage.with_extension("base"))?;
        sync_parent(&stage)?;
    } else {
        match (exists(&stage)?, exists(&state)?) {
            (true, false) => verify_tree(&stage, &j)?,
            (false, true) => {
                verify_tree(&state, &j)?;
                storage::set_writable(&state)?;
                move_tree(&state, &stage, j.mode)?;
            }
            _ => bail!("cannot recover APFS index state: expected exactly one copy of the index"),
        }
        // This also handles interruption while sealing or restoring the index.
        storage::set_writable(&stage)?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(j.mode))?;
        File::open(&stage)?.sync_all()?;
        sync_parent(&stage)?;
    }
    remove_if_present(&journal_path(p))?;
    sync_parent(&journal_path(p))?;
    Ok(())
}

/// The journal of a pending save, in its folder `meta/pending/<state>/journal.json`. It names the
/// clone (the state's tree) and the stage that the clone was made from. Both are identities
/// (subvolume UUID, device, inode), checked before a move and by recovery.
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct PendingJournal {
    version: u32,
    project: String,
    line: String,
    state: String,
    /// The process that makes the save, as `pid:start time` (`free::self_holder`). Recovery leaves
    /// the folder of a live holder alone; a reused pid does not match the start time.
    holder: String,
    mode: u32,
    stage_subvol: String,
    stage_device: u64,
    stage_inode: u64,
    tree_subvol: String,
    tree_device: u64,
    tree_inode: u64,
}

const PENDING_JOURNAL: &str = "journal.json";
const PENDING_TREE: &str = "tree";

fn pending_root(p: &Project) -> PathBuf {
    p.meta_dir().join("pending")
}

fn pending_dir(p: &Project, state: &str) -> PathBuf {
    pending_root(p).join(state)
}

/// The pending folders, each with its journal (None: the journal is missing or unreadable).
fn pending_entries(p: &Project) -> Result<Vec<(PathBuf, Option<PendingJournal>)>> {
    let rd = match fs::read_dir(pending_root(p)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for e in rd {
        let dir = e?.path();
        let journal = read_pending_journal(p, &dir).ok();
        out.push((dir, journal));
    }
    Ok(out)
}

/// The pending folders whose journal can be read.
fn pending_journals(p: &Project) -> Result<Vec<PendingJournal>> {
    Ok(pending_entries(p)?.into_iter().filter_map(|(_, j)| j).collect())
}

fn read_pending_journal(p: &Project, dir: &Path) -> Result<PendingJournal> {
    let file = fs::File::open(dir.join(PENDING_JOURNAL))?;
    let j: PendingJournal = serde_json::from_reader(file.take(65536)).context("read pending save journal")?;
    // The tree identity is empty until the clone is made (the journal is written before the clone).
    let tree_known = !j.tree_subvol.is_empty();
    if j.version != 1
        || j.project != p.id
        || !model::is_uuid(&j.line)
        || !model::is_uuid(&j.state)
        || !model::is_uuid(&j.stage_subvol)
        || !j.holder.contains(':')
        || (tree_known && !model::is_uuid(&j.tree_subvol))
        || j.mode & !0o7777 != 0
    {
        bail!("invalid pending save journal");
    }
    Ok(j)
}


/// True when `path` is the subvolume with this UUID and identity; false when nothing is there.
fn is_tree(path: &Path, subvol: &str, device: u64, inode: u64) -> Result<bool> {
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(m.is_dir() && m.dev() == device && m.ino() == inode && storage::subvol_info(path)?.uuid == subvol)
}

/// A named save's index, cloned before the project lock (`save -m`). The clone waits in
/// `meta/pending/<state>/tree` with its journal beside it, and the stage stays where it is. Under the
/// lock, `publish` renames the clone into the state. The caller then records the save and drops the
/// stage, and `finish` removes the journal last. Recovery finishes a committed save, and removes a
/// clone whose record never committed. The stage is never moved, so nothing is lost. The line lock
/// keeps the stage unchanged from the clone to the record.
pub struct PendingCommit<'p, 's> {
    project: &'p Project<'s>,
    /// The state that the save records.
    pub id: String,
    /// The clone, read-only, until `publish`.
    pub tree: PathBuf,
    /// The subvolume UUID of the clone: the `subvol` of the state record.
    pub subvol: String,
    journal: PendingJournal,
    /// The clone is still in the pending folder (not published).
    in_pending: bool,
    active: bool,
}

impl<'p, 's> PendingCommit<'p, 's> {
    /// Clone the stage of `line` with `clone`, which writes the complete tree to the path it is given
    /// (the view of the stage, so a thin stage is complete too). Call with the line lock held and
    /// without the project lock.
    pub fn prepare(project: &'p Project<'s>, line: &str, clone: impl FnOnce(&Path) -> Result<()>) -> Result<Self> {
        let _t = crate::trace::span("APFS clone index");
        if !model::is_uuid(line) {
            bail!("APFS index state requires a valid line");
        }
        let stage = project.stage_path(line);
        let sm = fs::symlink_metadata(&stage)?;
        let sinfo = storage::subvol_info(&stage)?;
        if !sm.is_dir() || sinfo.readonly {
            bail!("APFS index must be a writable tree");
        }
        let id = ids::new_id();
        let root = pending_root(project);
        fs::create_dir_all(&root)?;
        // The journal is complete before the folder appears under its state name: a folder without a
        // journal is never a save in progress.
        let tmp = root.join(format!(".{id}"));
        fs::create_dir(&tmp)?;
        let mut journal = PendingJournal {
            version: 1,
            project: project.id.clone(),
            line: line.into(),
            state: id.clone(),
            holder: crate::free::self_holder(),
            mode: sm.mode() & 0o7777,
            stage_subvol: sinfo.uuid,
            stage_device: sm.dev(),
            stage_inode: sm.ino(),
            tree_subvol: String::new(),
            tree_device: 0,
            tree_inode: 0,
        };
        fsutil::atomic_write(&tmp.join(PENDING_JOURNAL), &serde_json::to_vec(&journal)?, 0o600)?;
        let dir = pending_dir(project, &id);
        fs::rename(&tmp, &dir)?;
        sync_parent(&dir)?;
        let tree = dir.join(PENDING_TREE);
        let mut prepared = Self { project, id: id.clone(), tree: tree.clone(), subvol: String::new(), journal: journal.clone(), in_pending: true, active: true };
        // A failure from here on removes the folder (Drop).
        clone(&tree)?;
        let tm = fs::symlink_metadata(&tree)?;
        let tinfo = storage::subvol_info(&tree)?;
        journal.tree_subvol = tinfo.uuid.clone();
        journal.tree_device = tm.dev();
        journal.tree_inode = tm.ino();
        fsutil::atomic_write(&dir.join(PENDING_JOURNAL), &serde_json::to_vec(&journal)?, 0o600)?;
        prepared.subvol = tinfo.uuid;
        prepared.journal = journal;
        Ok(prepared)
    }

    /// Under the project lock: check that the stage is still the index that was cloned, then rename
    /// the clone into the state. Returns false, and changes nothing, when the stage changed.
    pub fn publish(&mut self) -> Result<bool> {
        let j = &self.journal;
        let stage = self.project.stage_path(&j.line);
        if !is_tree(&stage, &j.stage_subvol, j.stage_device, j.stage_inode)? {
            return Ok(false);
        }
        if !is_tree(&self.tree, &j.tree_subvol, j.tree_device, j.tree_inode)? {
            bail!("APFS index clone changed: {}", self.tree.display());
        }
        let state = self.project.state_path(&self.id);
        if exists(&state)? {
            bail!("state {} exists before its save", crate::ids::short(&self.id));
        }
        crate::thin::ensure_parent(&state)?;
        storage::set_writable(&self.tree)?;
        move_tree(&self.tree, &state, j.mode)?;
        storage::set_readonly(&state)?;
        File::open(&state)?.sync_all()?;
        sync_parent(&state)?;
        self.in_pending = false;
        Ok(true)
    }

    /// After the record and the drop of the stage, under the project lock: remove the journal, and
    /// then the pending folder. Requires the record.
    pub fn finish(mut self) -> Result<()> {
        if self.project.db.state(&self.id)?.is_none() {
            bail!("pending save state has no saved record");
        }
        let dir = pending_dir(self.project, &self.id);
        remove_if_present(&dir.join(PENDING_JOURNAL))?;
        sync_parent(&dir.join(PENDING_JOURNAL))?;
        self.active = false;
        match fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync_parent(&dir)?;
        Ok(())
    }
}

impl Drop for PendingCommit<'_, '_> {
    fn drop(&mut self) {
        // Not published: the clone goes and the stage stays. Published: the record decides at the
        // next lock (recovery), so the journal and the state stay.
        if self.active && self.in_pending {
            let dir = pending_dir(self.project, &self.id);
            if let Err(e) = self.project.trash(&dir) {
                eprintln!("warning: pending save folder stays until the next command: {e:#}");
            }
        }
    }
}

/// Recovery of one pending save, with the project lock held.
fn recover_pending(p: &Project, dir: &Path, j: &PendingJournal) -> Result<()> {
    let tree = dir.join(PENDING_TREE);
    let state = p.state_path(&j.state);
    if p.db.state(&j.state)?.is_some() {
        // Committed: the clone becomes the state, and the stage it was cloned from is dropped.
        if exists(&tree)? {
            crate::thin::ensure_parent(&state)?;
            storage::set_writable(&tree)?;
            move_tree(&tree, &state, j.mode)?;
            storage::set_readonly(&state)?;
            sync_parent(&state)?;
        }
        if !is_tree(&state, &j.tree_subvol, j.tree_device, j.tree_inode)? {
            bail!("committed save state is not its clone: {}", state.display());
        }
        let stage = p.stage_path(&j.line);
        if is_tree(&stage, &j.stage_subvol, j.stage_device, j.stage_inode)? {
            storage::delete_tree(&stage)?;
        }
        remove_if_present(&stage.with_extension("base"))?;
    } else {
        // Not committed: the clone goes; the stage was never moved. A tree at the state path that
        // is not the clone is refused, not removed.
        if exists(&state)? && !is_tree(&state, &j.tree_subvol, j.tree_device, j.tree_inode)? {
            bail!("state path holds a tree that is not the clone of its save: {}", state.display());
        }
        if exists(&state)? {
            p.trash(&state)?;
        }
    }
    remove_if_present(&dir.join(PENDING_JOURNAL))?;
    sync_parent(&dir.join(PENDING_JOURNAL))?;
    p.trash(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, Author, Record, StateKind, StateRec};
    use crate::store::Store;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let root = PathBuf::from(std::env::var_os("LAYR_APFS_TEST_ROOT").expect("set LAYR_APFS_TEST_ROOT"));
            let p = root.join(format!("pending-save-{}", ids::new_id()));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = storage::delete_tree(&self.0);
        }
    }
    fn actor() -> Actor {
        Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None }
    }
    fn stage(p: &Project, line: &str) -> Result<u64> {
        let path = p.stage_path(line);
        storage::create_subvolume(&path)?;
        fs::write(path.join("file"), b"staged bytes")?;
        fs::hard_link(path.join("file"), path.join("alias"))?;
        std::os::unix::fs::symlink("file", path.join("link"))?;
        crate::sys::apfs_attribute(&path.join("file"), c"com.layr.keep", Some(b"metadata"))?;
        fs::write(path.with_extension("base"), "original base")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o555))?;
        Ok(fs::metadata(path)?.ino())
    }
    /// Make the save in `dir` look left by a process that died: its journal names a holder whose
    /// process is gone.
    fn die(dir: &Path) {
        let path = dir.join(PENDING_JOURNAL);
        if !path.exists() {
            return;
        }
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let mut j: PendingJournal = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        j.holder = format!("{}:", child.id());
        fsutil::atomic_write(&path, &serde_json::to_vec(&j).unwrap(), 0o600).unwrap();
    }
    /// The record of a save, as `commit_state` writes it (the record of `move_head` is not needed).
    fn recorded(p: &Project, pending: &PendingCommit, line: &str) -> Result<()> {
        let mut r = Record::new("commit");
        r.states.push(StateRec {
            id: pending.id.clone(),
            kind: StateKind::Commit,
            change_id: None,
            derived_from: None,
            context: None,
            parents: vec![],
            line: Some(line.into()),
            subvol: pending.subvol.clone(),
            author: Author::default(),
            message: "test".into(),
            time: ids::now_ms(),
            running: false,
            conflicts: vec![],
            caches: Default::default(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
            cache_of: None,
        });
        p.append(r, &actor())?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn clone_keeps_the_stage_and_dropping_removes_the_clone() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
        let _lock = p.lock()?;
        let line = ids::new_id();
        let inode = stage(&p, &line)?;
        let old_info = storage::subvol_info(&p.stage_path(&line))?;
        let pc = PendingCommit::prepare(&p, &line, |dst| storage::snapshot(&p.stage_path(&line), dst, true))?;
        // The stage is untouched by the clone.
        assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), inode);
        assert_eq!(storage::subvol_info(&p.stage_path(&line))?.uuid, old_info.uuid);
        assert_eq!(fs::read(p.stage_path(&line).join("file"))?, b"staged bytes");
        // The clone is a read-only tree with the same content and its own identity.
        assert_ne!(fs::metadata(&pc.tree)?.ino(), inode);
        assert!(storage::subvol_info(&pc.tree)?.readonly);
        assert_eq!(fs::read(pc.tree.join("file"))?, b"staged bytes");
        assert_eq!(fs::metadata(pc.tree.join("file"))?.ino(), fs::metadata(pc.tree.join("alias"))?.ino());
        assert!(pc.tree.join("file").exists());
        let dir = pending_dir(&p, &pc.id);
        drop(pc);
        assert!(!dir.exists());
        assert!(!pending_root(&p).join(&line).exists());
        assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), inode);
        assert!(!pending(&p)?);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn publish_moves_the_clone_and_recovery_removes_an_uncommitted_one() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let line = ids::new_id();
        let id;
        let stage_inode;
        {
            let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
            let _lock = p.lock()?;
            stage_inode = stage(&p, &line)?;
            let mut pc = PendingCommit::prepare(&p, &line, |dst| storage::snapshot(&p.stage_path(&line), dst, true))?;
            id = pc.id.clone();
            let clone_inode = fs::metadata(&pc.tree)?.ino();
            assert!(pc.publish()?);
            let state = p.state_path(&id);
            assert_eq!(fs::metadata(&state)?.ino(), clone_inode);
            assert!(storage::subvol_info(&state)?.readonly);
            assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), stage_inode);
            die(&pending_dir(&p, &id));
            std::mem::forget(pc);
        }
        // No record: the reopened project removes the published clone and keeps the stage.
        let p = Project::open(&store, "p")?;
        assert!(!p.state_path(&id).exists());
        assert!(!pending(&p)?);
        assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), stage_inode);
        assert_eq!(fs::read(p.stage_path(&line).join("file"))?, b"staged bytes");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn reopen_recovers_each_uncommitted_boundary() -> Result<()> {
        // 0: the clone and its journal; 1: the clone published; 2: a temporary folder before its rename.
        for phase in 0..3 {
            let s = Sandbox::new();
            let store = Store::open(&s.0.join("store"))?;
            let line = ids::new_id();
            let stage_inode;
            {
                let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
                let _lock = p.lock()?;
                stage_inode = stage(&p, &line)?;
                let mut pc = PendingCommit::prepare(&p, &line, |dst| storage::snapshot(&p.stage_path(&line), dst, true))?;
                if phase >= 1 {
                    assert!(pc.publish()?);
                }
                let dir = pending_dir(&p, &pc.id);
                if phase == 2 {
                    // Left before the folder got its state name: a temporary folder with its journal.
                    fs::rename(&dir, pending_root(&p).join(format!(".{}", pc.id)))?;
                    die(&pending_root(&p).join(format!(".{}", pc.id)));
                } else {
                    die(&dir);
                }
                std::mem::forget(pc);
            }
            let p = Project::open(&store, "p")?;
            assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), stage_inode);
            assert_eq!(fs::read(p.stage_path(&line).join("file"))?, b"staged bytes");
            assert!(!storage::subvol_info(&p.stage_path(&line))?.readonly);
            assert!(!pending(&p)?);
            assert_eq!(fs::read_dir(p.states_dir())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn committed_save_survives_reopen_and_finishes() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let line = ids::new_id();
        let id;
        let clone_inode;
        {
            let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
            let _lock = p.lock()?;
            stage(&p, &line)?;
            let mut pc = PendingCommit::prepare(&p, &line, |dst| storage::snapshot(&p.stage_path(&line), dst, true))?;
            id = pc.id.clone();
            clone_inode = fs::metadata(&pc.tree)?.ino();
            assert!(pc.publish()?);
            recorded(&p, &pc, &line)?;
            // The stage is dropped after the record; the crash comes before the journal is removed.
            storage::delete_tree(&p.stage_path(&line))?;
            die(&pending_dir(&p, &pc.id));
            std::mem::forget(pc);
        }
        let p = Project::open(&store, "p")?;
        assert!(p.db.state(&id)?.is_some());
        assert!(!p.stage_path(&line).exists());
        assert!(!p.stage_path(&line).with_extension("base").exists());
        assert_eq!(fs::metadata(p.state_path(&id))?.ino(), clone_inode);
        assert!(storage::subvol_info(&p.state_path(&id))?.readonly);
        assert_eq!(fs::read(p.state_path(&id).join("file"))?, b"staged bytes");
        assert!(!pending(&p)?);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn recovery_refuses_a_substitute_tree() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
        let _lock = p.lock()?;
        let line = ids::new_id();
        stage(&p, &line)?;
        let mut pc = PendingCommit::prepare(&p, &line, |dst| storage::snapshot(&p.stage_path(&line), dst, true))?;
        assert!(pc.publish()?);
        let state = p.state_path(&pc.id);
        die(&pending_dir(&p, &pc.id));
        std::mem::forget(pc);
        let keep = p.work_dir().join("keep");
        move_tree(&state, &keep, 0o555)?;
        storage::create_subvolume(&state)?;
        fs::write(state.join("other"), b"keep this")?;
        assert!(recover(&p).is_err());
        assert_eq!(fs::read(state.join("other"))?, b"keep this");
        assert_eq!(fs::read(keep.join("file"))?, b"staged bytes");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn recovery_takes_over_a_reused_pid() -> Result<()> {
        // The journal names this live pid with another start time: the process that made the
        // save is gone, and recovery removes its clone.
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let line = ids::new_id();
        let stage_inode;
        {
            let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
            let _lock = p.lock()?;
            stage_inode = stage(&p, &line)?;
            let pc = PendingCommit::prepare(&p, &line, |dst| storage::snapshot(&p.stage_path(&line), dst, true))?;
            let path = pending_dir(&p, &pc.id).join(PENDING_JOURNAL);
            let mut j: PendingJournal = serde_json::from_slice(&fs::read(&path)?)?;
            j.holder = format!("{}:Mon Jan  1 00:00:00 1990", std::process::id());
            fsutil::atomic_write(&path, &serde_json::to_vec(&j)?, 0o600)?;
            std::mem::forget(pc);
        }
        let p = Project::open(&store, "p")?;
        assert!(!pending(&p)?);
        assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), stage_inode);
        Ok(())
    }
}
