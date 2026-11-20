//! The owned prepare/publish protocol for automatic and final-deletion saves.
use super::{recover_cache_replacements, skip_path, snap, Repo, Snap};
#[cfg(target_os = "macos")]
use crate::apfs::NestedRoots;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use crate::btrfs::NestedRoots;
use crate::ctx::Ctx;
use crate::fs as storage;
use crate::fsutil::Src;
use crate::model::{Line, StateKind, StateRec};
use crate::objstore::{self, PreparationLease};
#[cfg(feature = "overlayfs")]
use crate::overlayfs::NestedRoots;
use crate::store::{LineLeaseSet, LockedProject, Project};
use anyhow::{bail, ensure, Result};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub enum SaveMode {
    Automatic { message: String, running: bool, force: bool },
    FinalDeletion,
}

// The public outcome returns the record by value, matching the save API.
#[allow(clippy::large_enum_variant)]
pub enum PublishOutcome {
    Saved(StateRec),
    Unchanged(String),
    Stale,
}

/// Only prepare_save constructs this capability. The creator-tracked workspace, input pins
/// and object lease live through publication, including an ambiguous database commit.
pub struct PreparedSave {
    project: String,
    meta: PathBuf,
    line: Line,
    base: String,
    location: PathBuf,
    location_id: Option<(u64, u64)>,
    capture_root: PathBuf,
    capture_root_id: Option<(u64, u64)>,
    config: Vec<(String, String)>,
    view_config: Option<Vec<u8>>,
    validation: Validation,
    unsettled: bool,
    capture: Capture,
    private: Option<PreparedPrivate>,
    final_view: Option<(String, PreparedTree)>,
    caches: Vec<PreparedCache>,
    staged: Option<(Box<StateRec>, PreparedTree)>,
    final_index: Option<FinalIndex>,
    _pin: crate::free::Pin,
    _lease: PreparationLease,
    _inputs: Vec<super::Temp>,
}

impl PreparedSave {
    pub(crate) fn staged_id(&self) -> Option<String> {
        self.staged.as_ref().map(|(state, _)| state.id.clone())
    }

    pub(crate) fn accept_unsettled(&mut self) {
        if self.unsettled {
            // Only the content fence relaxes. Publication still validates every owner,
            // input and destination. Final deletion never sets this capability.
            self.validation = Validation::Captured;
        }
    }
}

struct FinalIndex {
    path: Option<PathBuf>,
    entries: Entries,
}
impl FinalIndex {
    fn read(repo: &Repo, line: &Line) -> Result<Self> {
        let path = repo.current_stage(line)?;
        let entries = match &path {
            Some(path) => entries(path, Boundary::Final)?,
            None => Entries::new(),
        };
        Ok(Self { path, entries })
    }
}

enum Validation {
    Captured,
    Changed,
    Stable(Entries, Boundary),
}

enum Capture {
    Unchanged(String),
    State { state: Box<StateRec>, tree: PreparedTree },
}

/// Discarding a stale or failed preparation is a rename even while a project lock is held.
/// A failed rename leaves a creator-tracked workspace for the dead-temporary sweeper.
struct Workspace {
    path: PathBuf,
    trash: PathBuf,
}
impl Workspace {
    fn new(p: &Project) -> Result<Self> {
        std::fs::create_dir_all(p.work_dir())?;
        let path = p.work_dir().join(crate::store::temp_name());
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700).create(&path)?;
        Ok(Self { path, trash: p.trash_dir()? })
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        #[cfg(feature = "overlayfs")]
        let _backend = match crate::overlayfs::try_publication(&self.path) {
            Ok(Some(backend)) => backend,
            _ => return, // The creator-tracked workspace remains for the sweeper.
        };
        let _ = std::fs::create_dir_all(&self.trash);
        let _ = crate::fsutil::rename_tree(&self.path, self.trash.join(crate::store::temp_name()));
    }
}

/// An empty overlay root is captured and sealed before acquiring the parent project lock.
pub(crate) struct PreparedEmptyTree(Workspace);
impl PreparedEmptyTree {
    fn new(p: &Project) -> Result<Self> {
        let root = Self(Workspace::new(p)?);
        let empty = super::Temp::empty(root.0.path.join("empty"))?;
        storage::freeze_path_owned(&empty.path, &root.0.path.join("tree"), storage::CapturePurpose::State)?.release();
        Ok(root)
    }
    pub(crate) fn publish(&self, dst: &Path) -> Result<()> {
        publish_tree(&self.0.path.join("tree"), dst)
    }
}

struct PreparedTree {
    workspace: Workspace,
    layout: Layout,
}
enum Layout {
    Complete,
    #[cfg(target_os = "macos")]
    Layer {
        base: String,
    },
}
impl PreparedTree {
    fn path(&self) -> PathBuf {
        self.workspace.path.join(match self.layout {
            Layout::Complete => "tree",
            #[cfg(target_os = "macos")]
            Layout::Layer { .. } => "delta",
        })
    }
    fn source(&self, repo: &Repo) -> Result<Snap> {
        #[cfg(not(target_os = "macos"))]
        let _ = repo;
        match &self.layout {
            Layout::Complete => Ok(snap(&self.path())),
            #[cfg(target_os = "macos")]
            Layout::Layer { base } => {
                let mut source = repo.state_src(base)?;
                source = crate::thin::Layered::over(&self.path(), source.into_layered())?.into();
                Ok(source)
            }
        }
    }
    /// Final deletion owns a complete tree before publication, including APFS thin captures.
    fn complete_final(&mut self, repo: &Repo) -> Result<()> {
        #[cfg(target_os = "macos")]
        if matches!(self.layout, Layout::Layer { .. }) {
            let info = crate::apfs::native_info(&self.path())?;
            let path = self.workspace.path.join("tree");
            crate::fs::freeze_source(&self.source(repo)?, &path)?.release();
            // The filesystem capture seals its root; update this private companion
            // with the original state identity before sealing it again.
            crate::sys::apfs_immutable(&path, false)?;
            crate::apfs::write_info(&path, &crate::apfs::SubvolInfo { base: None, ..info })?;
            crate::apfs::seal(&path, true, false)?;
            self.layout = Layout::Complete;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = repo;
        Ok(())
    }

    fn publish(&self, repo: &Repo, state: &StateRec) -> Result<()> {
        let dst = match &self.layout {
            Layout::Complete => repo.p.state_path(&state.id),
            #[cfg(target_os = "macos")]
            Layout::Layer { base } => {
                std::fs::create_dir_all(repo.p.deltas_dir())?;
                repo.p.lock_or_borrow()?.write(|tx| tx.record_base(&state.id, base))?;
                repo.p.delta_path(&state.id)
            }
        };
        if let Err(error) = publish_tree(&self.path(), &dst) {
            repo.discard_states(std::slice::from_ref(state));
            return Err(error);
        }
        if let Err(error) = (|| -> Result<()> {
            ensure!(std::env::var_os("LAYR_TEST_SAVE_INSTALL_FAIL").is_none(), "injected prepared installation failure");
            objstore::install_prepared(&repo.p, state)
        })() {
            repo.discard_states(std::slice::from_ref(state));
            return Err(error);
        }
        Ok(())
    }
}

/// Only this publication primitive varies by backend. btrfs refuses renaming a read-only
/// subvolume for an unprivileged creator, so open it for the rename and restore its seal.
fn publish_tree(from: &Path, to: &Path) -> Result<()> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    if storage::host().namespace.managed(from) && storage::host().namespace.info(from)?.readonly {
        crate::btrfs::unseal_tree(from)?;
        let moved = storage::move_tree(from, to);
        let sealed = crate::btrfs::seal_tree(if moved.is_ok() { to } else { from });
        moved?;
        return sealed;
    }
    storage::move_tree(from, to)
}

struct PreparedPrivate {
    name: String,
    project: String,
    config: Vec<(String, String)>,
    view: Vec<ViewInput>,
    state: StateRec,
    tree: PreparedTree,
    _lease: PreparationLease,
}

impl PreparedPrivate {
    fn publish(&self, ctx: &Ctx, repo: &Repo, lock: &std::rc::Rc<crate::store::Lock>, shared: &str) -> Result<()> {
        self.tree.publish(repo, &self.state)?;
        let mut rec = crate::model::DraftRecord::new("overlay.use");
        rec.data = serde_json::json!({"config": {crate::overlay::auto_key(shared): self.state.id}});
        rec.states.push(self.state.clone());
        repo.commit_record(&repo.p.borrow_reserved(lock), ctx, rec)?;
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
struct ViewInput {
    name: String,
    project: String,
    mark: (i64, i64),
}
impl ViewInput {
    fn read(p: &Project) -> Result<Self> {
        Ok(Self { name: p.name.clone(), project: p.id.clone(), mark: p.db.records_mark()? })
    }
}

struct PreparedCache {
    name: String,
    project: String,
    config: Vec<(String, String)>,
    previous: Option<Line>,
    state: StateRec,
    tree: PreparedTree,
    _pin: crate::free::Pin,
    _lease: PreparationLease,
}

fn folder_id(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

// No file data, content comparison or hashing in publication. Nanosecond
// ctime catches same-size writes even when a writer restores mtime. Atime is deliberately absent.
#[derive(PartialEq, Eq)]
struct Entry {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    // Nested storage roots contribute their identity and permissions, but their contents
    // and changing directory metadata belong to a separately managed tree.
    content: Option<ContentStamp>,
}
#[derive(PartialEq, Eq)]
struct ContentStamp {
    size: u64,
    links: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Source,
    Final,
}

#[derive(Debug)]
pub(crate) struct StalePreparation;
impl std::fmt::Display for StalePreparation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("working folder changed during save validation")
    }
}
impl std::error::Error for StalePreparation {}

fn scan_io<T>(result: std::result::Result<T, impl Into<anyhow::Error>>) -> Result<T> {
    result.map_err(|error| {
        let error = error.into();
        match error.downcast_ref::<std::io::Error>().and_then(std::io::Error::raw_os_error) {
            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => StalePreparation.into(),
            _ => error,
        }
    })
}

type Entries = HashMap<PathBuf, Entry>;
fn entries(root: &Path, boundary: Boundary) -> Result<Entries> {
    scan_entries(root, boundary, |_| Ok(()))
}

// Test barriers run only in the two preparation scans, never during publication.
fn preparation_entries(root: &Path, boundary: Boundary, phase: &str) -> Result<Validation> {
    let pause = std::env::var_os("LAYR_TEST_SAVE_SCAN_ENTRY").map(PathBuf::from);
    validation_scan(
        scan_entries(root, boundary, |rel| {
            if pause.as_deref() == Some(rel) {
                barrier(phase)?;
            }
            Ok(())
        }),
        boundary,
    )
}

fn validation_scan(scan: Result<Entries>, boundary: Boundary) -> Result<Validation> {
    match scan {
        Ok(entries) => Ok(Validation::Stable(entries, boundary)),
        // A scan race invalidates the content fence, but must not discard an unsettled
        // capture before the last foreground attempt can accept it. Other errors fail.
        Err(error) if error.is::<StalePreparation>() => Ok(Validation::Changed),
        Err(error) => Err(error),
    }
}

fn scan_entries(root: &Path, boundary: Boundary, mut before_entry: impl FnMut(&Path) -> Result<()>) -> Result<Entries> {
    let mut out = HashMap::new();
    let mut roots = None;
    let mut stack = vec![(PathBuf::new(), None)];
    while let Some((rel, expected_type)) = stack.pop() {
        before_entry(&rel)?;
        let path = root.join(&rel);
        let m = scan_io(std::fs::symlink_metadata(&path))?;
        if expected_type.is_some_and(|kind| kind != m.file_type()) || (rel.as_os_str().is_empty() && !m.is_dir()) {
            return Err(StalePreparation.into());
        }
        let nested = if boundary == Boundary::Source && !rel.as_os_str().is_empty() && m.is_dir() {
            if roots.is_none() {
                roots = Some(scan_io(NestedRoots::read(root))?);
            }
            scan_io(roots.as_ref().expect("scan boundaries").classify(&path, &m))? == crate::fsutil::StorageRoot::Managed
        } else {
            false
        };
        out.insert(
            rel.clone(),
            Entry {
                dev: m.dev(),
                ino: m.ino(),
                mode: m.mode(),
                uid: m.uid(),
                gid: m.gid(),
                content: (!nested).then_some(ContentStamp {
                    size: m.size(),
                    links: m.nlink(),
                    mtime: (m.mtime(), m.mtime_nsec()),
                    ctime: (m.ctime(), m.ctime_nsec()),
                }),
            },
        );
        if m.is_dir() && !nested {
            for child in scan_io(std::fs::read_dir(&path))? {
                let child = scan_io(child)?;
                let child_rel = rel.join(child.file_name());
                if !skip_path(&child_rel.to_string_lossy()) {
                    stack.push((child_rel, Some(scan_io(child.file_type())?)));
                }
            }
            // A directory replaced with a link or another tree while read_dir ran is stale.
            let after = scan_io(std::fs::symlink_metadata(&path))?;
            if (after.dev(), after.ino(), after.mode()) != (m.dev(), m.ino(), m.mode()) {
                return Err(StalePreparation.into());
            }
        }
    }
    Ok(out)
}

/// Test barriers are outside the project lock, and cost one env lookup when unset.
pub(crate) fn barrier(phase: &str) -> Result<()> {
    let Some(dir) = std::env::var_os(format!("LAYR_TEST_SAVE_{}_BARRIER", phase.to_ascii_uppercase())) else { return Ok(()) };
    let mut dir = PathBuf::from(dir);
    if phase == "copy" {
        // Copy validation repeats within and across captures. Give each check its own
        // gate so tests can change the real source in every round without a timing race.
        thread_local! {
            static CHECK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        let check = CHECK.replace(CHECK.get() + 1);
        dir.push(check.to_string());
        std::fs::create_dir_all(&dir)?;
    }
    std::fs::write(dir.join("ready"), b"")?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !dir.join("release").exists() {
        ensure!(std::time::Instant::now() < until, "save {phase} barrier timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

fn cache_warning(line: &Line, error: &anyhow::Error) {
    eprintln!("warning: cache overlay of line {} not saved: {error:#}", line.name);
}

fn private_warning(line: &Line, error: &anyhow::Error) {
    eprintln!("warning: the working changes of line {} have no automatic state: {error:#}", line.name);
}

fn test_cache_failure(phase: &str) -> Result<()> {
    ensure!(std::env::var_os(format!("LAYR_TEST_SAVE_CACHE_{phase}_FAIL")).is_none(), "injected optional cache failure");
    Ok(())
}

impl Repo<'_> {
    /// Cache-overlay creation records only empty trees. Do this separate setup before preparing
    /// objects, so prepare_save itself never takes a project lock. Existing overlays need no lock.
    /// A periodic caller requests non-blocking setup and retries on its next tick.
    pub fn setup_save(&self, ctx: &Ctx, line: &Line, wait: bool) -> Result<bool> {
        if !self.working(line).exists() {
            return Ok(true);
        }
        // View validation reserves every local overlay. Recover them before preparation,
        // outside the parent lock; publication only reserves clean overlays or retries.
        if self.private_view(line)? {
            for name in self.overlay_candidates()? {
                let name = format!("{}%{name}", self.p.name);
                if self.p.store.project_dir(&name)?.exists() && Project::open_maintenance(self.p.store, &name)?.is_none() {
                    return Ok(false);
                }
            }
        }
        #[cfg(target_os = "macos")]
        if crate::views::of(self, line).is_some() {
            return Ok(true);
        }
        let paths = {
            let _phase = crate::store::SavePreparation::new(&self.p);
            recover_cache_replacements(&self.working(line))?;
            self.restore_caches(line)?;
            match self.unsaved_cache_layers(line) {
                Ok(paths) => paths,
                Err(error) => {
                    cache_warning(line, &error);
                    return Ok(true);
                }
            }
        };
        for path in paths {
            let setup = (|| -> Result<bool> {
                if self.folder_overlay_of(&path)?.is_some() {
                    return Ok(true);
                }
                let empty = PreparedEmptyTree::new(&self.p)?;
                barrier("setup")?;
                let lock = if self.p.holds_lock() {
                    None
                } else if wait {
                    Some(self.p.lock_maintenance()?)
                } else {
                    let crate::store::LockAttempt::Acquired(lock) = self.p.try_lock()? else { return Ok(false) };
                    Some(lock)
                };
                if self.folder_overlay_of(&path)?.is_none() {
                    self.ensure_cache_overlay_prepared(&self.p.lock_or_borrow()?, ctx, &path, Some(&empty))?;
                }
                drop(lock);
                Ok(true)
            })();
            match setup {
                Ok(false) => return Ok(false),
                Err(error) if error.is::<crate::cmd::overlaycmd::OverlayRetirementBusy>() => return Ok(false),
                Err(error) => cache_warning(line, &error),
                Ok(true) => {}
            }
        }
        Ok(true)
    }

    pub fn prepare_save(&self, ctx: &Ctx, line: &Line, guard: &LineLeaseSet, mode: SaveMode) -> Result<PreparedSave> {
        ensure!(guard.require_line(&self.p, &line.id).is_ok(), "save preparation requires this project's real line lock");
        ensure!(!self.p.holds_lock(), "save preparation must run outside the project lock");
        let _phase = crate::store::SavePreparation::new(&self.p);
        let _trace = crate::trace::span("prepare save");
        ensure!(!self.p.db.line_deleted(&line.id)?, "line {} was deleted before save preparation", line.name);
        let fresh = self.line(&line.id)?;
        ensure!(fresh == *line, "line {} changed before save preparation", line.name);
        let base = self.save_base(line)?;
        let pin = self.pin(&[&line.head, &base])?;
        let lease = objstore::preparation_lease(&self.p)?;
        let config = self.p.db.config_all()?;
        let view_config = self.save_view_config(line)?;
        let location = self.working(line);
        let location_id = folder_id(&location);
        let (message, running, force, final_delete) = match mode {
            SaveMode::Automatic { message, running, force } => (message, running, force, false),
            SaveMode::FinalDeletion => ("before line delete".into(), false, false, true),
        };
        let final_index = final_delete.then(|| FinalIndex::read(self, line)).transpose()?;
        let root = self.save_validation_root(line);
        let capture_root_id = folder_id(&root);
        // A foreground save includes edits made while it waits for publication. Periodic
        // saves may retain a running capture and try newer content on the next tick.
        let boundary = if final_delete { Boundary::Final } else { Boundary::Source };
        let before = if (final_delete || !running) && root.exists() {
            preparation_entries(&root, boundary, "scan_before")?
        } else {
            Validation::Captured
        };
        barrier("snapshot")?;
        // APFS can avoid writing an empty thin layer. Native Linux comparisons require
        // immutable snapshots, so those backends compare after capture below.
        let unchanged = cfg!(target_os = "macos")
            && root.exists()
            && root == location
            && !force
            && self.tree_available(&base)
            && !self.private_view(line)?
            && self.private_paths(line)?.is_empty()
            && !crate::changes::any_file_change(&self.state_src(&base)?, &snap(&root), skip_path)?;
        let mut unsettled = false;
        let mut private = None;
        let capture = if !root.exists() || unchanged {
            Capture::Unchanged(base.clone())
        } else {
            #[cfg(target_os = "macos")]
            let capture_base =
                if crate::thin::chain_len(&self.p, &base) >= self.p.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize {
                    &line.head
                } else {
                    &base
                };
            #[cfg(not(target_os = "macos"))]
            let capture_base = &base;
            // A cache copy from setup must not qualify the source capture for fallback.
            let prior_unsettled = crate::fsutil::take_unsettled();
            let (mut state, mut tree) = self.prepare_working_tree(ctx, line, vec![line.head.clone()], &message, capture_base)?;
            unsettled = crate::fsutil::take_unsettled();
            state.running = running || prior_unsettled || unsettled;
            private = self.prepare_private_working(ctx, line, &message);
            unsettled |= crate::fsutil::take_unsettled();
            state.running |= unsettled;
            barrier("comparison")?;
            let mut compared = None;
            let changed = if force || !self.tree_available(&base) {
                true
            } else {
                let (old, old_full) = self.working_source_with(line, &base, |repo, id| repo.state_src(id))?;
                if old_full || private.is_some() {
                    match &private {
                        Some(part) if old_full => crate::changes::any_file_change(&old, &snap(&part.tree.path()), skip_path)?,
                        _ => true,
                    }
                } else {
                    let source = tree.source(self)?;
                    // Object encoding needs a complete stored delta, including times,
                    // directory attributes and hard links. Keep that evidence with the
                    // immutable capture instead of comparing it again in prepare_state.
                    if old.objects().is_some() && source.folder().is_some() && objstore::enabled(&self.p)? {
                        let comparison = objstore::compare(&old, source, None)?;
                        let changed = comparison.files.iter().any(|change| !skip_path(change.path()));
                        compared = Some(comparison);
                        changed
                    } else {
                        crate::changes::any_file_change(&old, &source, skip_path)?
                    }
                }
            };
            if !changed {
                private = None;
                Capture::Unchanged(base.clone())
            } else {
                if final_delete {
                    tree.complete_final(self)?;
                    barrier("final_complete")?;
                }
                barrier("objects")?;
                if let Some(compared) = compared {
                    compared.prepare(&self.p, &mut state)?;
                } else {
                    objstore::prepare_state(self, &mut state, &tree.source(self)?)?;
                }
                Capture::State { state: Box::new(state), tree }
            }
        };
        barrier("cache")?;
        let of = match &capture {
            Capture::State { state, .. } => &state.id,
            Capture::Unchanged(id) => id,
        };
        let final_view =
            if final_delete && matches!(capture, Capture::Unchanged(_)) { self.prepare_final_view(&base)? } else { None };
        let caches = self.prepare_save_cache_parts(ctx, line, of);
        barrier("cache_parts")?;
        let staged = if final_delete { self.prepare_final_stage(ctx, line)? } else { None };
        let validation = match before {
            Validation::Stable(before, boundary) => {
                // A writer during capture invalidates a foreground preparation even if
                // it stops before publication. Retry outside the project lock.
                match preparation_entries(&root, boundary, "scan_after")? {
                    Validation::Stable(after, _) if before == after => Validation::Stable(after, boundary),
                    _ => Validation::Changed,
                }
            }
            validation => validation,
        };
        super::clear_regenerable_memo();
        Ok(PreparedSave {
            project: self.p.id.clone(),
            meta: self.p.meta_dir(),
            line: line.clone(),
            base,
            location,
            location_id,
            capture_root: root,
            capture_root_id,
            config,
            view_config,
            validation,
            unsettled: unsettled && !final_delete && matches!(capture, Capture::State { .. }),
            capture,
            private,
            final_view,
            caches,
            staged,
            final_index,
            _pin: pin,
            _lease: lease,
            _inputs: crate::store::take_save_inputs(&self.p),
        })
    }

    /// Save before a command with the same prepare/publish protocol as an explicit save.
    /// The caller can retain the returned project lock for its following records.
    pub fn save_working<'p>(
        &'p self,
        ctx: &Ctx,
        line: &Line,
        guard: &LineLeaseSet,
        message: &str,
    ) -> Result<(LockedProject<'p>, Option<String>)> {
        let (lock, _, outcome, _) = self.save_prepared(
            ctx,
            &line.id,
            guard,
            SaveMode::Automatic { message: message.into(), running: false, force: false },
        )?;
        let state = match outcome {
            PublishOutcome::Saved(state) => Some(state.id),
            PublishOutcome::Unchanged(id) => self.working(line).exists().then_some(id),
            PublishOutcome::Stale => unreachable!("foreground save retries stale preparation"),
        };
        Ok((lock, state))
    }

    /// Bounded foreground retries always drop the project lock before preparing again.
    pub fn save_prepared<'p>(
        &'p self,
        ctx: &Ctx,
        id: &str,
        guard: &LineLeaseSet,
        mode: SaveMode,
    ) -> Result<(LockedProject<'p>, Line, PublishOutcome, Option<String>)> {
        let _warnings = crate::fsutil::copy_retry_warnings();
        for attempt in 0..3 {
            let line = self.line(id)?;
            if !self.setup_save(ctx, &line, true)? {
                barrier("retry")?;
                continue;
            }
            let line = self.line(id)?;
            let mut prepared = match self.prepare_save(ctx, &line, guard, mode.clone()) {
                Err(error) if error.is::<StalePreparation>() => {
                    barrier("retry")?;
                    continue;
                }
                result => result?,
            };
            if attempt == 2 {
                // A copy backend may never settle while a process writes. Keep its last
                // running capture after the bounded attempts, as the copy contract requires.
                prepared.accept_unsettled();
            }
            if let Some(ms) = std::env::var("LAYR_TEST_SAVE_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            barrier("publish")?;
            let staged = prepared.staged_id();
            let lock = guard.lock(&self.p)?;
            match self.publish_save(ctx, &lock, prepared)? {
                PublishOutcome::Stale => {
                    drop(lock);
                    barrier("retry")?;
                }
                outcome => return Ok((lock, line, outcome, staged)),
            }
        }
        bail!("line changed during save publication; retry the command")
    }

    fn prepare_final_view(&self, id: &str) -> Result<Option<(String, PreparedTree)>> {
        #[cfg(target_os = "macos")]
        if self.p.delta_path(id).is_dir() && !self.p.state_path(id).exists() {
            let workspace = Workspace::new(&self.p)?;
            let path = workspace.path.join("tree");
            // Use the exact existing state: equal live content may have different metadata.
            crate::fs::freeze_source(&self.state_src(id)?, &path)?.release();
            let info = crate::apfs::native_info(&self.p.delta_path(id))?;
            // The filesystem capture seals its root; update this private companion
            // with the original state identity before sealing it again.
            crate::sys::apfs_immutable(&path, false)?;
            crate::apfs::write_info(&path, &crate::apfs::SubvolInfo { base: None, ..info })?;
            crate::apfs::seal(&path, true, false)?;
            let tree = PreparedTree { workspace, layout: Layout::Complete };
            barrier("final_complete")?;
            return Ok(Some((id.into(), tree)));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = id;
        Ok(None)
    }

    fn save_view_config(&self, line: &Line) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.p.meta_dir().join("vlines").join(&line.id).join("view.json")) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save_validation_root(&self, line: &Line) -> PathBuf {
        #[cfg(target_os = "macos")]
        if crate::views::of(self, line).is_some() {
            return crate::views::upper(&self.p, &line.id);
        }
        self.working(line)
    }

    pub fn publish_save(&self, ctx: &Ctx, lock: &LockedProject<'_>, prepared: PreparedSave) -> Result<PublishOutcome> {
        ensure!(lock.check(&self.p).is_ok(), "save publication requires this project's real lock");
        #[cfg(feature = "overlayfs")]
        let Some(_backend) = crate::overlayfs::try_publication(&self.p.dir)?
        else {
            return Ok(PublishOutcome::Stale);
        };
        if self.p.db.line_deleted(&prepared.line.id)? {
            return Ok(PublishOutcome::Stale);
        }
        let Some(line) = self.p.db.line_by_id(&prepared.line.id)? else { return Ok(PublishOutcome::Stale) };
        let line = self.localize(line)?;
        if prepared.project != self.p.id
            || prepared.meta != self.p.meta_dir()
            || line != prepared.line
            || self.save_base(&line)? != prepared.base
            || self.working(&line) != prepared.location
            || folder_id(&prepared.location) != prepared.location_id
            || self.save_validation_root(&line) != prepared.capture_root
            || folder_id(&prepared.capture_root) != prepared.capture_root_id
            || self.p.db.config_all()? != prepared.config
            || self.save_view_config(&line)? != prepared.view_config
        {
            return Ok(PublishOutcome::Stale);
        }
        if matches!(prepared.validation, Validation::Changed) {
            return Ok(PublishOutcome::Stale);
        }
        if let Validation::Stable(expected, boundary) = &prepared.validation {
            match entries(&prepared.capture_root, *boundary) {
                Ok(current) if current == *expected => {}
                Ok(_) => return Ok(PublishOutcome::Stale),
                Err(error) if error.is::<StalePreparation>() => return Ok(PublishOutcome::Stale),
                Err(error) => return Err(error),
            }
        }
        if let Some(expected) = &prepared.final_index {
            match FinalIndex::read(self, &line) {
                Ok(current) if current.path == expected.path && current.entries == expected.entries => {}
                Ok(_) => return Ok(PublishOutcome::Stale),
                Err(error) if error.is::<StalePreparation>() => return Ok(PublishOutcome::Stale),
                Err(error) => return Err(error),
            }
        }
        let _trace = crate::trace::span("publish prepared save");
        // Acquire every overlay lock non-blocking before publishing anything. No blocking overlay
        // open or lock wait under the parent: pending APFS recovery also returns Stale.
        let mut overlays = Vec::new();
        for cache in &prepared.caches {
            let checked = (|| -> Result<Option<_>> {
                let p = Project::open_records(self.p.store, &format!("{}%{}", self.p.name, cache.name))?;
                let Some(lock) = p.try_reserve()? else { return Ok(None) };
                let orepo = Repo { restore_from_remote: self.restore_from_remote, p, ctx: self.ctx };
                let info = self.reserved_overlay_info(&cache.name, &orepo.p)?;
                if orepo.p.id != cache.project
                    || orepo.p.db.config_all()? != cache.config
                    || crate::overlay::find_overlay_line(&orepo.p.db, &info, &line)? != cache.previous
                {
                    return Ok(None);
                }
                Ok(Some((orepo, std::rc::Rc::new(lock), info)))
            })();
            match checked {
                Ok(None) => return Ok(PublishOutcome::Stale),
                Ok(Some((orepo, lock, info))) => overlays.push((cache, orepo, lock, info)),
                Err(error) => cache_warning(&line, &error),
            }
        }
        let mut view_locks = Vec::new();
        let held = overlays.iter().map(|(_, _, lock, _)| lock.as_ref()).collect::<Vec<_>>();
        let private = if let Some(part) = &prepared.private {
            let Some(reserved) = self.reserve_private_working(part, &held, &mut view_locks)? else {
                return Ok(PublishOutcome::Stale);
            };
            Some(reserved)
        } else {
            None
        };
        if let Some((id, view)) = &prepared.final_view {
            let dst = self.p.state_path(id);
            if !dst.exists() {
                if let Err(error) = publish_tree(&view.path(), &dst) {
                    let _ = self.p.trash(&dst);
                    return Err(error);
                }
            }
        }
        let mut rec = crate::model::DraftRecord::new("save");
        let before = self.recorded_working(&line)?;
        if let Some((state, tree)) = &prepared.staged {
            tree.publish(self, state)?;
            rec.states.push(state.as_ref().clone());
        }
        let outcome = match &prepared.capture {
            Capture::Unchanged(id) => PublishOutcome::Unchanged(id.clone()),
            Capture::State { state, tree } => {
                if let Err(error) = tree.publish(self, state) {
                    self.discard_states(&rec.states);
                    return Err(error);
                }
                rec.states.push(state.as_ref().clone());
                PublishOutcome::Saved(state.as_ref().clone())
            }
        };
        let after = match &outcome {
            PublishOutcome::Saved(state) => &state.id,
            PublishOutcome::Unchanged(id) => id,
            PublishOutcome::Stale => unreachable!(),
        };
        rec.data = serde_json::json!({"line": line.id, "head": line.head, "working_before": before, "working_after": after});
        if !rec.states.is_empty() {
            self.commit_record(lock, ctx, rec)?;
        }
        if let (Some(part), Some((orepo, _lock))) = (&prepared.private, &private) {
            if let Err(error) = part.publish(ctx, orepo, _lock, after) {
                private_warning(&line, &error);
            }
        }
        for (cache, orepo, _lock, info) in &overlays {
            let saved = (|| -> Result<()> {
                test_cache_failure("PUBLISH")?;
                let ol =
                    self.ensure_build_line(&orepo.p.borrow_reserved(_lock), ctx, orepo, &cache.name, info, &line, &info.root)?;
                let mut state = cache.state.clone();
                state.line = Some(ol.id.clone());
                state.parents = vec![ol.head.clone()];
                cache.tree.publish(orepo, &state)?;
                let id = state.id.clone();
                Ok(crate::overlay::move_overlay_line(
                    &orepo.p.borrow_reserved(_lock),
                    orepo,
                    ctx,
                    "commit",
                    &ol,
                    &id,
                    vec![state],
                )?)
            })();
            if let Err(error) = saved {
                cache_warning(&line, &error);
            }
        }
        Ok(outcome)
    }

    /// `working_snapshot` with its thin layer over `base` (APFS). Both the shared
    /// tree and its private companion are prepared before either is published.
    pub fn working_snapshot_over(
        &self,
        ctx: &Ctx,
        line: &Line,
        parents: Vec<String>,
        message: &str,
        base: &str,
    ) -> Result<StateRec> {
        let _lease = objstore::preparation_lease(&self.p)?;
        let (mut state, tree) = self.prepare_working_tree(ctx, line, parents, message, base)?;
        let private = self.prepare_private_working(ctx, line, message);
        objstore::prepare_state(self, &mut state, &tree.source(self)?)?;
        let mut view_locks = Vec::new();
        let reserved = match &private {
            Some(part) => self.reserve_private_working(part, &[], &mut view_locks)?,
            None => None,
        };
        ensure!(private.is_none() || reserved.is_some(), "overlay view changed while capturing line {}", line.name);
        tree.publish(self, &state)?;
        if let (Some(part), Some((orepo, lock))) = (&private, &reserved) {
            if let Err(error) = part.publish(ctx, orepo, lock, &state.id) {
                private_warning(line, &error);
            }
        }
        Ok(state)
    }

    fn prepare_working_tree(
        &self,
        ctx: &Ctx,
        line: &Line,
        parents: Vec<String>,
        message: &str,
        base: &str,
    ) -> Result<(StateRec, PreparedTree)> {
        let workspace = Workspace::new(&self.p)?;
        let state = self.capture_working(
            ctx,
            line,
            parents,
            message,
            base,
            crate::ids::new_id(),
            &workspace.path.join("tree"),
            &workspace.path.join("delta"),
        )?;
        #[cfg(target_os = "macos")]
        let layout = if workspace.path.join("tree").exists() {
            Layout::Complete
        } else {
            Layout::Layer {
                base: crate::apfs::native_info(&workspace.path.join("delta"))?
                    .base
                    .ok_or_else(|| anyhow::anyhow!("prepared layer has no base"))?,
            }
        };
        #[cfg(not(target_os = "macos"))]
        let layout = Layout::Complete;
        #[allow(unused_mut)]
        let mut tree = PreparedTree { workspace, layout };
        // A recovered Git base exists only in this operation. Do not publish a thin
        // dependency on its missing native tree, including virtual-view captures.
        #[cfg(target_os = "macos")]
        if let Layout::Layer { base } = &tree.layout {
            if !self.p.has_state(base) && objstore::verified(&self.p, base)?.is_none() {
                tree.complete_final(self)?;
            }
        }
        Ok((state, tree))
    }

    fn reserve_private_working(
        &self,
        part: &PreparedPrivate,
        held: &[&crate::store::Lock],
        view_locks: &mut Vec<crate::store::Lock>,
    ) -> Result<Option<(Repo<'_>, std::rc::Rc<crate::store::Lock>)>> {
        let p = Project::open_records(self.p.store, &format!("{}%{}", self.p.name, part.name))?;
        let Some(lock) = p.try_reserve()? else { return Ok(None) };
        if p.id != part.project || p.db.config_all()? != part.config {
            return Ok(None);
        }
        // The top was resolved before capture. Reserve every local overlay that could
        // affect that resolution, then check its records without APFS recovery opens.
        // A second busy overlay must release the parent lock for a fresh preparation.
        let mut view = Vec::new();
        for op in self.save_view_projects()? {
            if !lock.protects(&op) && !held.iter().any(|lock| lock.protects(&op)) {
                let Some(lock) = op.try_reserve()? else { return Ok(None) };
                view_locks.push(lock);
            }
            view.push(ViewInput::read(&op)?);
        }
        if view != part.view {
            return Ok(None);
        }
        Ok(Some((Repo { restore_from_remote: self.restore_from_remote, p, ctx: self.ctx }, std::rc::Rc::new(lock))))
    }

    fn save_view_projects(&self) -> Result<Vec<Project<'_>>> {
        let mut projects = Vec::new();
        for name in self.overlay_candidates()? {
            let name = format!("{}%{name}", self.p.name);
            if self.p.store.project_dir(&name)?.exists() {
                projects.push(Project::open_records(self.p.store, &name)?);
            }
        }
        Ok(projects)
    }

    // Private content stays in the overlay's workspace and object store. Preparation never
    // publishes either capture; a failed private capture still permits the safe shared state.
    fn prepare_private_working(&self, ctx: &Ctx, line: &Line, message: &str) -> Option<PreparedPrivate> {
        let made = (|| -> Result<Option<PreparedPrivate>> {
            if self.p.is_overlay() {
                return Ok(None);
            }
            let view = self.save_view_projects()?.iter().map(ViewInput::read).collect::<Result<Vec<_>>>()?;
            let Some(name) = self.line_view_top(line)? else { return Ok(None) };
            let orepo = self.overlay_repo(&name)?;
            let lease = objstore::preparation_lease(&orepo.p)?;
            let config = orepo.p.db.config_all()?;
            let tree = PreparedTree { workspace: Workspace::new(&orepo.p)?, layout: Layout::Complete };
            storage::snapshot(&self.working(line), &tree.path(), true)?;
            let mut state = orepo.state_record(
                ctx,
                crate::ids::new_id(),
                StateKind::Auto,
                Vec::new(),
                message,
                None,
                storage::host().namespace.info(&tree.path())?.uuid,
            );
            objstore::prepare_state(&orepo, &mut state, &tree.source(&orepo)?)?;
            Ok(Some(PreparedPrivate { name, project: orepo.p.id.clone(), config, view, state, tree, _lease: lease }))
        })();
        match made {
            Ok(part) => part,
            Err(error) => {
                private_warning(line, &error);
                None
            }
        }
    }

    /// A staged version may exist nowhere else. Prepare it with the final working state,
    /// so deletion publishes both in one record without comparing or copying under its lock.
    fn prepare_final_stage(&self, ctx: &Ctx, line: &Line) -> Result<Option<(Box<StateRec>, PreparedTree)>> {
        if self.current_stage(line)?.is_none() {
            return Ok(None);
        }
        let (index, _temp) = self.index_view(line)?;
        let head = self.head_src(line)?;
        if !crate::changes::any_file_change(&head, &index, skip_path)? {
            return Ok(None);
        }
        let workspace = Workspace::new(&self.p)?;
        let tree = PreparedTree { workspace, layout: Layout::Complete };
        let mut keep = Vec::new();
        let complete = self.complete(index, &mut keep)?;
        storage::snapshot(&complete, &tree.path(), true)?;
        let mut state = self.state_record(
            ctx,
            crate::ids::new_id(),
            StateKind::Auto,
            vec![line.head.clone()],
            "staged before line delete",
            Some(line),
            storage::host().namespace.info(&tree.path())?.uuid,
        );
        objstore::prepare_state(self, &mut state, &tree.source(self)?)?;
        Ok(Some((Box::new(state), tree)))
    }

    fn prepare_save_cache_parts(&self, ctx: &Ctx, line: &Line, of: &str) -> Vec<PreparedCache> {
        let mut out = Vec::new();
        #[cfg(target_os = "macos")]
        if crate::views::of(self, line).is_some() {
            return out;
        }
        let paths = match self.unsaved_cache_layers(line) {
            Ok(paths) => paths,
            Err(error) => {
                cache_warning(line, &error);
                return out;
            }
        };
        for path in paths {
            let made = (|| -> Result<PreparedCache> {
                test_cache_failure("PREPARE")?;
                let name = self.folder_overlay_of(&path)?.ok_or_else(|| anyhow::anyhow!("cache {path} needs save setup"))?;
                let orepo = self.overlay_repo(&name)?;
                let lease = objstore::preparation_lease(&orepo.p)?;
                let info = self.need_overlay(&name)?;
                let previous = crate::overlay::find_overlay_line(&orepo.p.db, &info, line)?;
                let parent = previous.as_ref().map(|l| l.head.as_str()).unwrap_or(&info.root);
                let pin = orepo.pin(&[parent])?;
                let config = orepo.p.db.config_all()?;
                let key = self.cache_key(ctx, &name, &self.working(line), line.owner)?;
                let workspace = Workspace::new(&orepo.p)?;
                let tree = PreparedTree { workspace, layout: Layout::Complete };
                storage::snapshot(&crate::fsutil::safe_join(&self.working(line), &path)?, &tree.path(), true)?;
                let mut state = orepo.state_record(
                    ctx,
                    crate::ids::new_id(),
                    StateKind::Commit,
                    vec![parent.into()],
                    &format!("cache {path} of line {}", line.name),
                    None,
                    storage::host().namespace.info(&tree.path())?.uuid,
                );
                state.cache_by = Some(self.cache_maker(ctx, line));
                state.cache_shared = self.policy().rule_for(&line.name).is_some();
                state.cache_platform = Some(key.key);
                state.cache_key_parts = key.parts;
                state.cache_of = Some(of.into());
                objstore::prepare_state(&orepo, &mut state, &tree.source(&orepo)?)?;
                Ok(PreparedCache { name, project: orepo.p.id.clone(), config, previous, state, tree, _pin: pin, _lease: lease })
            })();
            match made {
                Ok(cache) => out.push(cache),
                Err(error) => cache_warning(line, &error),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Caller;
    use crate::model::LineChange;
    use crate::store::Store;

    fn fixture(test: impl FnOnce(&Ctx, &Repo, &Line) -> Result<()>) -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-save-protocol-{}", crate::ids::new_id()));
        let ctx = Ctx {
            captures: Default::default(),
            store: Store::open(&dir)?,
            caller: Caller::from_uid(rustix::process::getuid().as_raw()),
            cwd: dir.clone(),
            env: Default::default(),
            author: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let path = ctx.store.project_dir("p")?;
        std::fs::create_dir_all(path.join("meta/db"))?;
        std::fs::create_dir_all(path.join("states"))?;
        let p = Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "p".into(),
            dir: path.clone(),
            id: crate::ids::new_id(),
            db: crate::db::Db::open(&crate::store::db_file(&path))?,
        };
        let mut rec = crate::model::DraftRecord::new("project");
        rec.data = serde_json::json!({"id":p.id,"name":"p"});
        p.lock_maintenance()?.append(rec, &ctx.actor())?;
        let repo = Repo { restore_from_remote: true, p, ctx: Some(&ctx) };
        let line = Line {
            id: crate::ids::new_id(),
            name: "main".into(),
            head: crate::ids::new_id(),
            owner: ctx.caller.uid,
            machine: ctx.store.machine.id.clone(),
        };
        let mut rec = crate::model::DraftRecord::new("commit");
        let state = repo.state_record(&ctx, line.head.clone(), StateKind::Commit, vec![], "initial", Some(&line), String::new());
        storage::create(&repo.p.state_path(&state.id))?;
        rec.states.push(state);
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Create { after: repo.line_state(&line, &line.head) },
                working_before: None,
                working_after: None,
            },
        );
        repo.commit_record(&repo.p.lock_or_borrow()?, &ctx, rec)?;
        let result = test(&ctx, &repo, &line);
        drop(repo);
        drop(ctx);
        fn unseal(path: &Path) -> Result<()> {
            if storage::host().namespace.managed(path) {
                storage::set_writable(path)?;
            }
            for child in std::fs::read_dir(path)? {
                let child = child?;
                if child.file_type()?.is_dir() {
                    unseal(&child.path())?;
                }
            }
            Ok(())
        }
        unseal(&dir)?;
        // The store owns its OverlayFS registry. Remove its children through the
        // backend before the plain fixture root takes the registry itself away.
        for child in std::fs::read_dir(&dir)? {
            let child = child?;
            if child.file_name() != ".layr-overlayfs" {
                storage::remove(&child.path())?;
            }
        }
        std::fs::remove_dir_all(&dir)?;
        result
    }
    fn automatic() -> SaveMode {
        SaveMode::Automatic { message: "test".into(), running: false, force: false }
    }

    #[test]
    fn prepared_comparison_reads_private_objects_without_publishing_a_native_tree() -> Result<()> {
        fixture(|ctx, repo, line| {
            let dir = ctx.store.project_dir("p%pro")?;
            std::fs::create_dir_all(dir.join("meta/db"))?;
            std::fs::create_dir_all(dir.join("states"))?;
            let p = Project {
                lease: Default::default(),
                store: &ctx.store,
                name: "p%pro".into(),
                dir: dir.clone(),
                id: crate::ids::new_id(),
                db: crate::db::Db::open(&crate::store::db_file(&dir))?,
            };
            let id = crate::ids::new_id();
            let mut rec = crate::model::DraftRecord::new("project");
            rec.data = serde_json::json!({"id":p.id,"name":"p%pro","config":{
                "overlay":{"project":repo.p.id,"name":"pro"},
                "overlay.pro":{"hidden":true,"owner":line.owner,"mode":"follow","root":id},
                "access":{format!("user:{}",line.owner):"admin"},
                "objects.enabled":true,
                format!("hidden.part.{}",line.head):id,
                crate::overlay::auto_key(&line.head):id
            }});
            p.lock_maintenance()?.append(rec, &ctx.actor())?;
            let orepo = Repo { restore_from_remote: true, p, ctx: Some(ctx) };
            let tree = PreparedTree { workspace: Workspace::new(&orepo.p)?, layout: Layout::Complete };
            std::fs::create_dir(tree.path())?;
            std::fs::write(tree.path().join("file"), b"private bytes")?;
            let mut state = orepo.state_record(ctx, id.clone(), StateKind::Auto, vec![], "private", None, String::new());
            objstore::prepare_state(&orepo, &mut state, &tree.source(&orepo)?)?;
            let mut rec = crate::model::DraftRecord::new("overlay.use");
            rec.states.push(state);
            orepo.commit_record(&orepo.p.lock_or_borrow()?, ctx, rec)?;
            assert!(!orepo.p.state_path(&id).exists());
            let _phase = crate::store::SavePreparation::new(&orepo.p);
            let (source, full) = repo.working_source_with(line, &line.head, |repo, id| repo.state_src(id))?;
            assert!(full, "the comparison must use private content even though the public tree is unchanged");
            assert_eq!(crate::fsutil::read_file(&source, "file")?, b"private bytes");
            assert!(!orepo.p.state_path(&id).exists(), "comparison must not rebuild a published native tree");
            assert!(!crate::changes::any_file_change(&source, &tree.source(&orepo)?, skip_path)?);
            std::fs::write(tree.path().join("file"), b"changed private bytes")?;
            assert!(crate::changes::any_file_change(&source, &tree.source(&orepo)?, skip_path)?);
            drop(_phase);
            Ok(())
        })
    }

    #[test]
    fn preparation_phase_refuses_indirect_project_locks() -> Result<()> {
        fixture(|_, repo, _| {
            let phase = crate::store::SavePreparation::new(&repo.p);
            assert!(repo
                .p
                .lock_maintenance()
                .err()
                .is_some_and(|error| error.to_string().contains("project lock acquired during save preparation")));
            drop(phase);
            let _lock = repo.p.lock_maintenance()?;
            Ok(())
        })
    }

    #[test]
    fn plain_cache_discovery_does_not_rebuild_a_retired_object_head() -> Result<()> {
        fixture(|ctx, repo, line| {
            let mut rec = crate::model::DraftRecord::new("config");
            rec.data = serde_json::json!({"config":{"objects.enabled":true,"regenerable":["target"]}});
            repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
            let id = crate::ids::new_id();
            std::fs::create_dir(repo.p.state_path(&id))?;
            std::fs::write(repo.p.state_path(&id).join("versioned"), b"head bytes")?;
            let mut rec = crate::model::DraftRecord::new("commit");
            rec.states.push(repo.state_record(
                ctx,
                id.clone(),
                StateKind::Commit,
                vec![line.head.clone()],
                "object head",
                Some(line),
                String::new(),
            ));
            rec.lines.insert(
                line.id.clone(),
                LineChange {
                    transition: crate::model::LineTransition::Move {
                        before: repo.line_state(line, &line.head),
                        after: repo.line_state(line, &id),
                    },
                    working_before: None,
                    working_after: None,
                },
            );
            repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
            objstore::retire_native(repo)?;
            assert!(!repo.p.has_state(&id));
            let line = repo.line(&line.id)?;
            let working = repo.working(&line);
            std::fs::create_dir_all(working.join("target"))?;
            std::fs::write(working.join(".gitignore"), b"target/\n")?;
            std::fs::write(working.join("target/output"), b"rebuilt cache")?;
            assert_eq!(repo.plain_caches(&line)?, vec!["target"]);
            assert!(!repo.p.has_state(&id), "cache discovery must read objects without a native checkout");
            Ok(())
        })
    }

    #[test]
    fn preparation_requires_a_real_matching_line_descriptor() -> Result<()> {
        fixture(|ctx, repo, line| {
            let guard = repo.line_lock(line)?;
            let nested = repo.line_lock(line)?;
            assert!(repo.prepare_save(ctx, line, &nested, automatic()).is_ok(), "nested holders retain the same physical lease");
            let other = repo.p.line_lock("another-line", "other")?;
            assert!(repo.prepare_save(ctx, line, &other, automatic()).is_err());
            let mut foreign = Project {
                lease: Default::default(),
                store: repo.p.store,
                name: "foreign".into(),
                dir: repo.p.dir.join("foreign"),
                id: crate::ids::new_id(),
                db: crate::db::Db::open(&repo.p.dir.join("foreign.sqlite"))?,
            };
            std::fs::create_dir_all(foreign.meta_dir())?;
            let foreign_guard = foreign.line_lock(&line.id, &line.name)?;
            assert!(repo.prepare_save(ctx, line, &foreign_guard, automatic()).is_err());
            // Even the right descriptor cannot prepare under a project lock.
            let lock = repo.p.lock_maintenance()?;
            assert!(repo.prepare_save(ctx, line, &guard, automatic()).is_err());
            assert!(lock.check(&foreign).is_err());
            foreign.id = repo.p.id.clone();
            assert!(guard.require_line(&foreign, &line.id).is_err());
            drop(lock);
            let path = repo.p.meta_dir().join("line-locks").join(&line.id);
            std::fs::rename(&path, path.with_extension("replaced"))?;
            std::fs::write(path, b"")?;
            assert!(repo.prepare_save(ctx, line, &guard, automatic()).is_err());
            Ok(())
        })
    }

    #[test]
    fn final_preparation_rejects_a_created_or_edited_stage() -> Result<()> {
        for existing in [false, true] {
            fixture(|ctx, repo, line| {
                let guard = repo.line_lock(line)?;
                let stage = repo.p.stage_path(&line.id);
                std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700).create(repo.p.scan_dir())?;
                std::fs::create_dir_all(repo.p.meta_dir().join("stage"))?;
                if existing {
                    storage::create(&stage)?;
                    repo.mark_stage(line)?;
                    std::fs::write(stage.join("file"), b"before")?;
                }
                let prepared = repo.prepare_save(ctx, line, &guard, SaveMode::FinalDeletion)?;
                drop(guard);
                if !existing {
                    storage::create(&stage)?;
                    repo.mark_stage(line)?;
                }
                std::fs::write(stage.join("file"), b"after")?;
                let guard = repo.line_lock(line)?;
                let lock = guard.lock(&repo.p)?;
                assert!(matches!(repo.publish_save(ctx, &lock, prepared)?, PublishOutcome::Stale));
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn save_base_can_be_stale_without_a_head_move() -> Result<()> {
        fixture(|ctx, repo, line| {
            let guard = repo.line_lock(line)?;
            let prepared = repo.prepare_save(ctx, line, &guard, automatic())?;
            let lock = repo.p.lock_maintenance()?;
            let id = crate::ids::new_id();
            std::fs::create_dir(repo.p.state_path(&id))?;
            let state = repo.state_record(
                ctx,
                id.clone(),
                StateKind::Auto,
                vec![line.head.clone()],
                "other save",
                Some(line),
                String::new(),
            );
            let mut rec = crate::model::DraftRecord::new("save");
            rec.states.push(state);
            rec.data = serde_json::json!({"line":line.id,"head":line.head,"working_after":id});
            repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
            assert_eq!(repo.line(&line.id)?.head, line.head);
            assert!(matches!(repo.publish_save(ctx, &lock, prepared)?, PublishOutcome::Stale));
            Ok(())
        })
    }

    #[test]
    fn unsettled_fallback_relaxes_only_the_content_fence() -> Result<()> {
        for case in ["settled", "unsettled", "config", "delete"] {
            fixture(|ctx, repo, line| {
                let guard = repo.line_lock(line)?;
                let mut prepared = repo.prepare_save(ctx, line, &guard, automatic())?;
                let workspace = Workspace::new(&repo.p)?;
                let tree = PreparedTree { workspace, layout: Layout::Complete };
                std::fs::create_dir(tree.path())?;
                std::fs::write(tree.path().join("file"), b"last running capture")?;
                let mut state = repo.state_record(
                    ctx,
                    crate::ids::new_id(),
                    StateKind::Auto,
                    vec![line.head.clone()],
                    "unsettled",
                    Some(line),
                    String::new(),
                );
                state.running = true;
                let id = state.id.clone();
                prepared.capture = Capture::State { state: Box::new(state), tree };
                prepared.validation = Validation::Changed;
                prepared.unsettled = case != "settled";
                prepared.accept_unsettled();
                let lock = repo.p.lock_maintenance()?;
                if case == "config" {
                    let mut rec = crate::model::DraftRecord::new("config");
                    rec.data = serde_json::json!({"config":{"snapshot.interval":42}});
                    repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
                } else if case == "delete" {
                    let mut rec = crate::model::DraftRecord::new("line.delete");
                    rec.lines.insert(
                        line.id.clone(),
                        LineChange {
                            transition: crate::model::LineTransition::Delete { before: Some(repo.line_state(line, &line.head)) },
                            working_before: None,
                            working_after: None,
                        },
                    );
                    repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
                }
                let outcome = repo.publish_save(ctx, &lock, prepared)?;
                if case == "unsettled" {
                    assert!(matches!(outcome, PublishOutcome::Saved(state) if state.running && state.id == id));
                    assert!(repo.p.db.state(&id)?.is_some());
                    assert_eq!(std::fs::read(repo.p.state_path(&id).join("file"))?, b"last running capture");
                } else {
                    assert!(matches!(outcome, PublishOutcome::Stale), "{case}");
                    assert!(repo.p.db.state(&id)?.is_none());
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn head_config_location_and_deletion_invalidate_preparation() -> Result<()> {
        for change in ["head", "config", "location", "view", "delete"] {
            fixture(|ctx, repo, line| {
                let guard = repo.line_lock(line)?;
                let prepared = repo.prepare_save(ctx, line, &guard, automatic())?;
                let lock = repo.p.lock_maintenance()?;
                let mut rec = crate::model::DraftRecord::new("config");
                match change {
                    "config" => rec.data = serde_json::json!({"config":{"snapshot.interval":42}}),
                    "location" => {
                        std::fs::create_dir_all(repo.working(line))?;
                    }
                    "view" => {
                        let dir = repo.p.meta_dir().join("vlines").join(&line.id);
                        std::fs::create_dir_all(&dir)?;
                        std::fs::write(dir.join("view.json"), b"{}")?;
                    }
                    "head" => {
                        let state = repo.state_record(
                            ctx,
                            crate::ids::new_id(),
                            StateKind::Commit,
                            vec![line.head.clone()],
                            "move",
                            Some(line),
                            String::new(),
                        );
                        rec.op = "commit".into();
                        rec.lines.insert(
                            line.id.clone(),
                            LineChange {
                                transition: crate::model::LineTransition::Move {
                                    before: repo.line_state(line, &line.head),
                                    after: repo.line_state(line, &state.id),
                                },
                                working_before: None,
                                working_after: None,
                            },
                        );
                        rec.states.push(state);
                    }
                    "delete" => {
                        rec.op = "line.delete".into();
                        rec.lines.insert(
                            line.id.clone(),
                            LineChange {
                                transition: crate::model::LineTransition::Delete {
                                    before: Some(repo.line_state(line, &line.head)),
                                },
                                working_before: None,
                                working_after: None,
                            },
                        );
                    }
                    _ => unreachable!(),
                }
                if change != "location" && change != "view" {
                    repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
                }
                assert!(matches!(repo.publish_save(ctx, &lock, prepared)?, PublishOutcome::Stale), "{change}");
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn object_pruning_defers_until_preparation_is_released() -> Result<()> {
        // Other unit tests fork. An inherited flock remains held until their child exits,
        // so test immediate release in an isolated process, as the service tests do.
        if std::env::var_os("LAYR_TEST_SAVE_PRUNE_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "repo::save::tests::object_pruning_defers_until_preparation_is_released", "--nocapture"])
                .env("LAYR_TEST_SAVE_PRUNE_CHILD", "1")
                .status()?;
            ensure!(status.success(), "isolated preparation-lease test failed");
            return Ok(());
        }
        fixture(|ctx, repo, line| {
            let store = objstore::open(&repo.p)?;
            let guard = repo.line_lock(line)?;
            let prepared = repo.prepare_save(ctx, line, &guard, automatic())?;
            std::fs::create_dir_all(repo.p.work_dir())?;
            let file = repo.p.work_dir().join("blob");
            std::fs::write(&file, b"not yet recorded")?;
            let (blob, _) = store.put_file(&file, layr_objects::Temperature::Hot, None)?;
            let lock = repo.p.lock_maintenance()?;
            assert!(objstore::maintain(&repo.p, objstore::MaintenanceMode::Run)?.is_none());
            assert!(store.has(&blob));
            drop(prepared);
            drop(store);
            assert!(objstore::maintain(&repo.p, objstore::MaintenanceMode::Run)?.is_some());
            assert!(!objstore::open(&repo.p)?.has(&blob));
            drop(lock);
            Ok(())
        })
    }

    #[test]
    fn source_fence_excludes_nested_cache_churn_but_final_fence_keeps_it() -> Result<()> {
        let parent = std::env::temp_dir();
        if !storage::host().namespace.supported(&parent) {
            eprintln!("cache-fence test requires a supported local storage backend");
            return Ok(());
        }
        let root = parent.join(format!("layr-save-cache-fence-{}", crate::ids::new_id()));
        storage::create(&root)?;
        let cache = root.join("target");
        storage::create(&cache)?;
        std::fs::write(root.join("source"), b"stable source")?;
        std::fs::create_dir(root.join("target-sibling"))?;
        std::fs::write(root.join("target-sibling/source"), b"unregistered source folder")?;
        std::fs::write(cache.join("old"), b"already saved cache")?;
        let result = (|| -> Result<()> {
            let source = entries(&root, Boundary::Source)?;
            let final_state = entries(&root, Boundary::Final)?;
            std::fs::remove_file(cache.join("old"))?;
            std::fs::create_dir(cache.join("new"))?;
            std::fs::write(cache.join("new/object"), b"new compiler output")?;
            assert!(source == entries(&root, Boundary::Source)?);
            assert!(final_state != entries(&root, Boundary::Final)?);
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cache)?.mode();
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(mode ^ 0o100))?;
            assert!(source != entries(&root, Boundary::Source)?, "cache permissions still belong to the source boundary");
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(mode))?;
            std::fs::write(root.join("target-sibling/source"), b"changed unregistered source")?;
            assert!(source != entries(&root, Boundary::Source)?, "an unregistered sibling is source content");
            Ok(())
        })();
        storage::remove(&root)?;
        result
    }

    #[test]
    fn metadata_scan_removal_and_type_changes_invalidate_the_content_fence() -> Result<()> {
        for boundary in [Boundary::Source, Boundary::Final] {
            for directory in [false, true] {
                let root = std::env::temp_dir().join(format!("layr-save-scan-{}", crate::ids::new_id()));
                std::fs::create_dir(&root)?;
                let victim = root.join("victim");
                if directory {
                    std::fs::create_dir(&victim)?;
                } else {
                    std::fs::write(&victim, b"removed after enumeration")?;
                }
                let result = scan_entries(&root, boundary, |rel| {
                    if rel == Path::new("victim") {
                        if directory {
                            std::fs::remove_dir(&victim)?;
                            std::fs::write(&victim, b"directory became a file")?;
                        } else {
                            std::fs::remove_file(&victim)?;
                        }
                    }
                    Ok(())
                });
                let error = result.err().expect("scan race must be stale");
                assert!(error.is::<StalePreparation>());
                assert!(matches!(validation_scan(Err(error), boundary)?, Validation::Changed));
                std::fs::remove_dir_all(root)?;
            }
        }
        Ok(())
    }

    #[test]
    fn metadata_scan_preserves_permission_and_io_errors() {
        for errno in [libc::EACCES, libc::EPERM, libc::EIO] {
            let error = scan_io::<()>(Err(std::io::Error::from_raw_os_error(errno))).unwrap_err();
            let error = validation_scan(Err(error), Boundary::Source).err().expect("operational error must fail");
            assert!(!error.is::<StalePreparation>());
            assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().raw_os_error(), Some(errno));
        }
        for errno in [libc::ENOENT, libc::ENOTDIR, libc::ELOOP] {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(errno)).context("storage boundary query");
            assert!(scan_io::<()>(Err(error)).unwrap_err().is::<StalePreparation>());
        }
    }

    #[test]
    fn final_validation_detects_same_length_write_with_restored_mtime() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-save-metadata-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        let file = root.join("file");
        std::fs::write(&file, b"before")?;
        let time = 1_700_000_000_000_000_000;
        layr_objects::times::set(&file, time)?;
        let before = entries(&root, Boundary::Final)?;
        std::fs::write(&file, b"after!")?;
        layr_objects::times::set(&file, time)?;
        assert!(before != entries(&root, Boundary::Final)?);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn deletion_publication_keeps_going_after_cache_stat_permission_denied() -> Result<()> {
        if crate::test_runs_as_root() {
            return Ok(());
        }
        fixture(|ctx, repo, line| {
            use std::os::unix::fs::PermissionsExt;
            let mut config = crate::model::DraftRecord::new("config");
            config.data = serde_json::json!({"config": {
                "overlay.nested": {"owner":line.owner,"mode":"follow","root":"empty"},
                "overlay.nested.folder":"sub/cache"
            }});
            repo.commit_record(&repo.p.lock_or_borrow()?, ctx, config)?;
            let folder = repo.working(line);
            std::fs::create_dir_all(folder.join("sub/cache"))?;
            std::fs::set_permissions(folder.join("sub"), std::fs::Permissions::from_mode(0o000))?;
            assert_eq!(
                std::fs::symlink_metadata(folder.join("sub/cache")).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let lock = repo.p.lock_maintenance()?;
            let result = crate::cmd::branch::delete_recorded(&lock, ctx, repo, line, None, true, None);
            // Restore permissions before the lock releases the trash worker, even on error.
            if folder.exists() {
                std::fs::set_permissions(folder.join("sub"), std::fs::Permissions::from_mode(0o700))?;
            }
            for entry in std::fs::read_dir(repo.p.trash_dir()?)? {
                let sub = entry?.path().join("sub");
                if sub.exists() {
                    std::fs::set_permissions(sub, std::fs::Permissions::from_mode(0o700))?;
                }
            }
            result?;
            assert!(repo.p.db.line_deleted(&line.id)?);
            assert!(!folder.exists());
            assert_eq!(repo.p.db.records_with_op("line.delete")?.len(), 1);
            Ok(())
        })
    }

    #[test]
    fn prepared_objects_and_native_tree_survive_an_ambiguous_commit() -> Result<()> {
        fixture(|ctx, repo, line| {
            let mut rec = crate::model::DraftRecord::new("config");
            rec.data = serde_json::json!({"config":{"objects.enabled":true}});
            repo.commit_record(&repo.p.lock_or_borrow()?, ctx, rec)?;
            let guard = repo.line_lock(line)?;
            let mut prepared = repo.prepare_save(ctx, line, &guard, automatic())?;
            let workspace = Workspace::new(&repo.p)?;
            let tree = PreparedTree { workspace, layout: Layout::Complete };
            std::fs::create_dir(tree.path())?;
            std::fs::write(tree.path().join("file"), b"durable")?;
            let mut state = repo.state_record(
                ctx,
                crate::ids::new_id(),
                StateKind::Auto,
                vec![line.head.clone()],
                "prepared",
                Some(line),
                String::new(),
            );
            objstore::prepare_state(repo, &mut state, &tree.source(repo)?)?;
            let id = state.id.clone();
            prepared.capture = Capture::State { state: Box::new(state), tree };
            let lock = repo.p.lock_maintenance()?;
            crate::db::FAIL_AFTER_COMMIT.with(|f| f.set(true));
            assert!(repo.publish_save(ctx, &lock, prepared).is_err());
            assert!(repo.p.db.state(&id)?.is_some());
            assert!(repo.p.state_path(&id).exists());
            assert_eq!(crate::fsutil::read_file(&repo.state_src(&id)?, "file")?, b"durable");
            assert!(repo.p.meta_dir().join("object-native").join(&id).exists());
            drop(lock);
            objstore::retire_native(repo)?;
            assert!(!repo.p.has_state(&id));
            assert_eq!(repo.save_base(line)?, id, "object-backed automatic states remain save bases");
            Ok(())
        })
    }
}
