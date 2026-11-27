//! Reachability and bounded object-store maintenance. Filesystem and project policy live
//! above this crate; these plans are shared by dry runs and destructive execution.

use super::unreadable as unreadable_error;
use super::*;

/// A maintenance batch bounds file mutations and compression input. One oversized file is
/// admitted in each phase to guarantee progress; reachability walks still inspect all roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceLimit {
    files: usize,
    bytes: u64,
}

impl MaintenanceLimit {
    pub const PERIODIC: Self = Self { files: 4096, bytes: 1 << 30 };
    pub const UNLIMITED: Self = Self { files: usize::MAX, bytes: u64::MAX };

    fn admits(self, files: usize, bytes: u64, next: u64) -> bool {
        files < self.files && (files == 0 || bytes.saturating_add(next) <= self.bytes)
    }
}

#[derive(Debug)]
struct MaintenanceFile {
    id: Id,
    path: PathBuf,
    bytes: u64,
}

/// A read-only snapshot of prune, cold conversion and packing work. The caller serializes
/// maintenance and prevents state publication until `prune_planned` finishes. Compression
/// needs no project lock: content-addressed cold forms are verified before hot files go.
#[derive(Debug, Default)]
#[must_use]
pub struct MaintenancePlan {
    pub prune: PruneReport,
    pub evict: EvictReport,
    /// Existing loose objects eligible for packing, before new cold encodings are written.
    pub pack_objects: u64,
    pub pack_bytes: u64,
    pack_input: u64,
    dead: Vec<PathBuf>,
    cold: Vec<MaintenanceFile>,
    pub(super) pack: HashSet<Id>,
    pub(super) roots: Vec<StateRef>,
    packing: PackingScope,
    /// What the plan could not read and skipped: kept states and reached objects (`Store::damage`
    /// lists the damaged files for fsck). Maintenance of the rest goes on.
    pub damaged: Vec<String>,
    /// Packs of an older, larger block size written again (`Store::reblock`), set by the caller.
    pub reblocked: u64,
}

/// An object that `Store::problem` checks: the kind it must have (None: any), whether a hot blob
/// counts, the delta chain above it, and whether another object needs it.
struct Check {
    id: Id,
    want: Option<Kind>,
    hot: bool,
    chain: Vec<Id>,
    need: bool,
}

impl Check {
    /// A need of kind `want` (a hot blob counts for a delta base) below `chain`.
    fn of(id: Id, want: Option<Kind>, chain: Vec<Id>) -> Check {
        Check { id, want, hot: want == Some(Kind::Blob), chain, need: true }
    }
}

/// One damaged item of a store, from `Store::damage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Damaged {
    /// A loose object file that cannot be read, or whose chunk, delta base or dictionary is missing:
    /// `Store::quarantine` moves it, and the next write of the object writes it again.
    Object(Id),
    /// A dictionary file that cannot be read: `Store::quarantine` moves it.
    Dictionary(Id),
    /// A state record that cannot be read: rewrite it (`set_state`) or quarantine it.
    Record(String),
    /// An object that a state reaches and that is missing or cannot be read in a pack. Only a write of
    /// the same content (a full write, `layr objects import --full`) makes it again.
    Missing(Id),
}

/// A damaged item and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    pub item: Damaged,
    pub reason: String,
}

#[derive(Debug, Default)]
enum PackingScope {
    #[default]
    Planned,
    IncludingNewCold,
}

impl Store {
    pub(super) fn prune_lease(root: &Path) -> Result<File> {
        let path = root.join(PRUNE_LOCK);
        match File::open(&path) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o644).open(&path) {
                    Ok(file) => Ok(file),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(File::open(path)?),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// A pruning handle excludes every other store handle, including imports that have not
    /// published roots and Cloud wire objects being consumed. Never wait while holding a
    /// project lock: writers may need it to finish publication. A busy lease is retried later.
    pub fn try_open_for_prune(root: &Path, keying: Keying) -> Result<Option<Store>> {
        let lease = Self::prune_lease(root)?;
        match rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Self::open_leased(root, keying, Some(lease))?)),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Readers/writers can resume before the caller releases its project lock. The shared
    /// lease continues to protect the repack's inputs against another pruning process.
    pub fn finish_prune(&self) -> Result<()> {
        let lease = self.prune_lease.as_ref().context("the store has no prune lease")?;
        rustix::fs::flock(lease, rustix::fs::FlockOperation::LockShared)?;
        Ok(())
    }

    /// Turn the hot blobs that no tree of `keep` names into cold objects (a zstd frame, or chunks
    /// for a large one) and remove their files. A hot file goes only after its cold form reads
    /// back to the same id.
    pub fn evict_hot(&self, keep: &[Id]) -> Result<EvictReport> {
        let hot = self.hot_blobs(keep)?;
        let mut files = Vec::new();
        self.visit_maintenance_files("blobs", |file| {
            if !hot.contains(&file.id) {
                files.push(file);
            }
            Ok(())
        })?;
        self.evict_planned(&files)
    }

    fn hot_blobs(&self, keep: &[Id]) -> Result<HashSet<Id>> {
        let mut hot = HashSet::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<Id> = keep.to_vec();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            // A tree that cannot be read keeps nothing hot; the plan reports it.
            let tree = match self.get_tree(&t) {
                Ok(tree) => tree,
                Err(error) if is_unreadable(error.as_ref()) => continue,
                Err(error) => return Err(error),
            };
            for e in tree.entries {
                match e.node {
                    crate::tree::Node::Dir { tree } => stack.push(tree),
                    crate::tree::Node::File { blob, .. } => {
                        hot.insert(blob);
                    }
                    crate::tree::Node::Symlink { .. } => {}
                }
            }
        }
        Ok(hot)
    }

    fn evict_planned(&self, files: &[MaintenanceFile]) -> Result<EvictReport> {
        use rayon::prelude::*;
        let done = files
            .par_iter()
            .map(|file| -> Result<u64> {
                let id = &file.id;
                let path = &file.path;
                let len = file.bytes;
                if !self.present(id, false)? {
                    if len >= self.options.chunk_min_file {
                        let (got, _) = self.put_chunked_file(path)?;
                        ensure!(got == *id, "hot blob {id} does not match its id");
                    } else {
                        let data = fs::read(path)?;
                        ensure!(self.keying.id(Kind::Blob, &data) == *id, "hot blob {id} does not match its id");
                        let (form, prefix, frame) = self.whole_frame(&data)?;
                        self.write_object(id, Kind::Blob, form, &[&prefix, &frame])?;
                    }
                }
                let cold = self.read_blob_unchecked(id, 0, false)?;
                ensure!(self.keying.id(Kind::Blob, &cold) == *id, "the cold form of {id} does not match its id");
                fs::remove_file(path)?;
                Ok(len)
            })
            .collect::<Result<Vec<u64>>>()?;
        Ok(EvictReport { blobs: done.len() as u64, bytes: done.iter().sum() })
    }

    /// Plan deletions, cold conversions and packing from the same reachability snapshot.
    /// Packed garbage stays until a future compaction pass rewrites packs.
    pub fn plan_maintenance(
        &self,
        keep: &std::collections::BTreeSet<String>,
        hot_trees: &[Id],
        limit: MaintenanceLimit,
    ) -> Result<MaintenancePlan> {
        self.plan_maintenance_keeping(keep, &HashSet::new(), hot_trees, limit)
    }

    /// `plan_maintenance` that also keeps the blobs of `blobs` and what their stored forms need:
    /// another index names them, for example the git ids of a project (docs/git-on-objects.md).
    pub fn plan_maintenance_keeping(
        &self,
        keep: &std::collections::BTreeSet<String>,
        blobs: &HashSet<Id>,
        hot_trees: &[Id],
        limit: MaintenanceLimit,
    ) -> Result<MaintenancePlan> {
        let mut plan = MaintenancePlan {
            packing: if limit == MaintenanceLimit::UNLIMITED { PackingScope::IncludingNewCold } else { PackingScope::Planned },
            ..Default::default()
        };
        let mut roots = Vec::new();
        let states = self.root.join("states");
        for entry in fs::read_dir(&states)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if keep.contains(&name) {
                match self.state_ref(&name) {
                    Ok(Some(r)) => roots.push(r),
                    Ok(None) => {}
                    Err(error) if is_unreadable(error.as_ref()) => plan.damaged.push(format!("{error:#}")),
                    Err(error) => return Err(error),
                }
            } else {
                let bytes = entry.metadata()?.len();
                if limit.admits(plan.dead.len(), plan.prune.bytes, bytes) {
                    plan.dead.push(entry.path());
                    plan.prune.bytes += bytes;
                    plan.prune.states += 1;
                }
            }
        }
        let (reach, unreadable) = self.walk(&roots, blobs)?;
        plan.damaged.extend(unreadable.values().cloned());
        // A kept record or object that cannot be read may name anything: the walk is incomplete, so
        // nothing is pruned until the damage is repaired or written again (fsck reports it).
        let prunes = plan.damaged.is_empty();
        if !prunes {
            plan.dead.clear();
            plan.prune = PruneReport::default();
        }
        plan.roots = roots;
        let hot = self.hot_blobs(hot_trees)?;
        for (folder, is_hot) in [("objects", false), ("blobs", true)] {
            self.visit_maintenance_files(folder, |file| {
                let id = file.id;
                if !reach.contains(&id) {
                    if prunes && limit.admits(plan.dead.len(), plan.prune.bytes, file.bytes) {
                        plan.prune.bytes += file.bytes;
                        if is_hot {
                            plan.prune.blobs += 1;
                        } else {
                            plan.prune.objects += 1;
                        }
                        plan.dead.push(file.path);
                    }
                } else if is_hot {
                    if !hot.contains(&id) && limit.admits(plan.cold.len(), plan.evict.bytes, file.bytes) {
                        plan.evict.blobs += 1;
                        plan.evict.bytes += file.bytes;
                        plan.cold.push(file);
                    }
                } else if plan.pack.len() < limit.files && !unreadable.contains_key(&id) {
                    // Reached and damaged: the walk reported it; kept for fsck, never packed. The input
                    // bounds the compressor, rather than the already-compressed disk size.
                    let Some(input) = check_loose(&file.path)?.map(|loose| loose.input) else { return Ok(()) };
                    if limit.admits(plan.pack.len(), plan.pack_input, input) {
                        plan.pack_objects += 1;
                        plan.pack_bytes += file.bytes;
                        plan.pack_input += input;
                        plan.pack.insert(id);
                    }
                }
                Ok(())
            })?;
        }
        Ok(plan)
    }

    /// Every object that `roots` and the blobs of `blobs` reach, and those that cannot be read with
    /// why: a tree or times table gives its children, any other object is checked with what it needs
    /// (`problem`). Each id is visited once. An object that cannot be read ends its branch; an I/O
    /// error ends the walk.
    pub(super) fn walk(&self, roots: &[StateRef], blobs: &HashSet<Id>) -> Result<(HashSet<Id>, HashMap<Id, String>)> {
        let (mut reach, mut unreadable) = (HashSet::new(), HashMap::new());
        let mut work: Vec<_> = roots.iter().flat_map(|r| [(r.tree, Kind::Tree), (r.times, Kind::Times)]).collect();
        work.extend(blobs.iter().map(|b| (*b, Kind::Blob)));
        while let Some((id, kind)) = work.pop() {
            let next: Result<Vec<(Id, Kind)>> = match kind {
                Kind::Tree | Kind::Times if !reach.insert(id) => continue,
                Kind::Tree => self.get_tree(&id).map(|tree| {
                    tree.entries
                        .into_iter()
                        .filter_map(|entry| match entry.node {
                            crate::tree::Node::Dir { tree } => Some((tree, Kind::Tree)),
                            crate::tree::Node::File { blob, .. } => Some((blob, Kind::Blob)),
                            crate::tree::Node::Symlink { .. } => None,
                        })
                        .collect()
                }),
                Kind::Times => self.get_times(&id).map(|times| {
                    times
                        .records
                        .into_iter()
                        .filter_map(|record| match record {
                            crate::times::Record::Folder(child) => Some((child, Kind::Times)),
                            _ => None,
                        })
                        .collect()
                }),
                _ => {
                    unreadable.extend(self.problem(vec![Check::of(id, Some(kind), Vec::new())], &mut reach, false, true)?);
                    continue;
                }
            };
            match next {
                Ok(next) => work.extend(next),
                Err(error) if is_unreadable(error.as_ref()) => {
                    unreadable.insert(id, format!("{error:#}"));
                }
                Err(error) => return Err(error),
            }
        }
        Ok((reach, unreadable))
    }

    /// Where `id` is stored and what that form needs directly: its delta base, its chunks, its
    /// dictionary (`Kind::Opaque`). A loose file first, then a pack, then (with `hot`) a hot blob,
    /// which needs nothing. `Unreadable` when it is missing or damaged; any other error goes back as it is.
    fn direct(&self, id: &Id, hot: bool) -> Result<(Kind, Vec<(Id, Kind)>)> {
        let loose = check_loose(&self.object_path(id)).map_err(|error| {
            if is_unreadable(error.as_ref()) {
                unreadable_error(format!("object {id} {error:#}"))
            } else {
                error
            }
        })?;
        if let Some(loose) = loose {
            let mut needs = loose.needs;
            needs.extend(loose.dict.map(|dict| (dict, Kind::Opaque)));
            return Ok((loose.header.kind, needs));
        }
        let set = self.pack_set();
        if let Some((pack, entry)) = set.find(id, &self.last_pack)? {
            return Ok((entry.kind, self.entry_needs(&set, pack, &entry)?));
        }
        match self.hot_path(id).filter(|_| hot) {
            Some(_) => Ok((Kind::Blob, Vec::new())),
            None => Err(unreadable_error(format!("object {id} is missing"))),
        }
    }

    /// What a pack entry needs. Only a raw entry is not decoded.
    fn entry_needs(&self, set: &PackSet, pack: u32, entry: &pack::Entry) -> Result<Vec<(Id, Kind)>> {
        if entry.form == pack::RAW {
            return Ok(Vec::new());
        }
        let payload = self.packed(set, pack, entry)?;
        damage_of(|| body_needs(entry.form, &payload.bytes).with_context(|| format!("object {}", entry.id)))
    }

    /// The objects of `work` or of what they need that cannot be read, with why: the first only, or
    /// with `all` every one (fsck); none when all read. Iterative, each id once (`seen`). With `memo`,
    /// the chunks and dictionaries that this handle found readable before are skipped and those found
    /// now are remembered. A delta chain that loops or passes 255 bases is damage. An I/O error goes
    /// back as it is.
    fn problem(&self, mut work: Vec<Check>, seen: &mut HashSet<Id>, memo: bool, all: bool) -> Result<Vec<(Id, String)>> {
        let (mut found, mut kinds, mut damaged) = (Vec::new(), HashMap::new(), Vec::new());
        let wrong =
            |id: &Id, kind: Kind, want: Kind| (id.to_owned(), format!("object {id} is a {}, not a {}", kind.name(), want.name()));
        // Damage ends this object's branch: with `all` the walk goes on with the others, else it stops
        // at once, remembering nothing (what it read so far may need what is damaged).
        macro_rules! damage {
            ($found:expr) => {{
                damaged.push($found);
                if all {
                    continue;
                }
                return Ok(damaged);
            }};
        }
        while let Some(Check { id, want, hot, chain, need }) = work.pop() {
            if chain.contains(&id) || chain.len() > 255 {
                damage!((id, format!("the delta chain of object {id} loops or is too long")));
            }
            // An object named again as another kind (a chunk list that names itself) is damage.
            if let (Some(kind), Some(want)) = (kinds.get(&id).copied(), want.filter(|w| kinds.get(&id) != Some(w))) {
                damage!(wrong(&id, kind, want));
            }
            // Delta bases are always walked, so the chain limit holds; chunks and dictionaries, which
            // many objects share, are read once per handle.
            let shared = memo && need && want != Some(Kind::Blob);
            let known = || shared && self.readable().contains(&id);
            if !seen.insert(id) || known() {
                continue;
            }
            let read = match want {
                Some(Kind::Opaque) => self.dict_bytes(&id).map(|_| (Kind::Opaque, Vec::new())),
                _ => self.direct(&id, hot),
            };
            let (kind, needs) = match read {
                Ok(read) => read,
                Err(error) if is_unreadable(error.as_ref()) => damage!((id, format!("{error:#}"))),
                Err(error) => return Err(error),
            };
            if let Some(want) = want.filter(|want| *want != kind) {
                damage!(wrong(&id, kind, want));
            }
            kinds.insert(id, kind);
            if shared {
                found.push(id);
            }
            for (need, need_kind) in needs {
                let chain = if need_kind == Kind::Blob { [chain.as_slice(), &[id]].concat() } else { Vec::new() };
                work.push(Check::of(need, Some(need_kind), chain));
            }
        }
        if memo {
            self.readable().extend(found);
        }
        Ok(damaged)
    }

    /// The objects this handle found readable (`problem`).
    fn readable(&self) -> std::sync::MutexGuard<'_, HashSet<Id>> {
        self.readable.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Why a packed entry cannot be read with what it needs, or None: a loose copy of it may go only
    /// then.
    pub(super) fn packed_problem(&self, set: &PackSet, pack: u32, entry: &pack::Entry) -> Result<Option<String>> {
        let needs = match self.packed(set, pack, entry).and_then(|p| damage_of(|| body_needs(entry.form, &p.bytes))) {
            Ok(needs) => needs,
            Err(error) if is_unreadable(error.as_ref()) => return Ok(Some(format!("{error:#}"))),
            Err(error) => return Err(error),
        };
        // The entry is above its needs, as if its loose copy were gone: a path back to it is a loop.
        let start = needs.into_iter().map(|(id, kind)| Check::of(id, Some(kind), vec![entry.id])).collect();
        Ok(self.problem(start, &mut HashSet::new(), true, false)?.into_iter().next().map(|(_, why)| why))
    }

    fn visit_maintenance_files(&self, folder: &str, mut visit: impl FnMut(MaintenanceFile) -> Result<()>) -> Result<()> {
        for fan in fan_folders(&self.root.join(folder))? {
            let prefix = fan.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for entry in fs::read_dir(&fan)? {
                let entry = entry?;
                let Ok(id) = Id::parse(&format!("{prefix}{}", entry.file_name().to_string_lossy())) else { continue };
                visit(MaintenanceFile { id, path: entry.path(), bytes: entry.metadata()?.len() })?;
            }
        }
        Ok(())
    }

    /// Why the stored object `id` cannot be read with what its form needs (its chunks, delta base and
    /// dictionary), or None: the one check of presence for dedupe, writers, gc and fsck (`problem`).
    /// A hot blob counts with `hot`. An I/O error is returned, never taken for damage.
    pub(crate) fn object_problem(&self, id: &Id, hot: bool) -> Result<Option<String>> {
        if hot && self.hot_path(id).is_some() {
            return Ok(None);
        }
        let start = Check { id: *id, want: None, hot, chain: Vec::new(), need: false };
        Ok(self.problem(vec![start], &mut HashSet::new(), true, false)?.into_iter().next().map(|(at, why)| {
            if at == *id {
                why
            } else {
                format!("object {id}: {why}")
            }
        }))
    }

    /// Why the stored object `id` (a hot blob counts) cannot be read, or None (`object_problem`).
    pub fn object_damage(&self, id: &Id) -> Result<Option<String>> {
        self.object_problem(id, true)
    }

    /// The record of the imported state `state` when it and both of its roots can be read; None when
    /// the store has no record; `Unreadable` when either cannot be read. The one check of a state for
    /// writers and gc.
    pub fn checked_state(&self, state: &str) -> Result<Option<StateRef>> {
        let Some(r) = self.state_ref(state)? else { return Ok(None) };
        for root in [r.tree, r.times] {
            if let Some(why) = self.object_damage(&root)? {
                return Err(unreadable_error(format!("state {state}: {why}")));
            }
        }
        Ok(Some(r))
    }

    /// Whether the tree and times, including their descendants, are available locally.
    pub fn has_state_objects(&self, refs: &StateRef) -> Result<bool> {
        Ok(self.walk(std::slice::from_ref(refs), &HashSet::new())?.1.is_empty())
    }

    /// Every damaged item of the store (fsck): loose object files (their first bytes, the fan-out
    /// folders in parallel), dictionaries, state records, and every object that a state reaches and
    /// that cannot be read with what it needs (the walk of gc, over every record). An I/O error stops it.
    pub fn damage(&self) -> Result<Vec<Damage>> {
        use rayon::prelude::*;
        let folders = fan_folders(&self.root.join("objects"))?;
        let mut out: Vec<Damage> = folders
            .par_iter()
            .map(|fan| -> Result<Vec<Damage>> {
                let prefix = fan.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mut found = Vec::new();
                for entry in fs::read_dir(fan)? {
                    let entry = entry?;
                    let Ok(id) = Id::parse(&format!("{prefix}{}", entry.file_name().to_string_lossy())) else { continue };
                    match check_loose(&entry.path()) {
                        Ok(_) => {}
                        Err(error) if is_unreadable(error.as_ref()) => {
                            found.push(Damage { item: Damaged::Object(id), reason: format!("object {id} {error:#}") })
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(found)
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        for entry in fs::read_dir(self.root.join("dicts"))? {
            let Ok(id) = Id::parse(&entry?.file_name().to_string_lossy()) else { continue };
            match self.dict_bytes(&id) {
                Ok(_) => {}
                Err(error) if is_unreadable(error.as_ref()) => {
                    out.push(Damage { item: Damaged::Dictionary(id), reason: format!("{error:#}") })
                }
                Err(error) => return Err(error),
            }
        }
        let mut states: Vec<String> = fs::read_dir(self.root.join("states"))?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        states.sort();
        let mut roots = Vec::new();
        for state in &states {
            match self.state_ref(state) {
                Ok(Some(r)) => roots.push(r),
                Ok(None) => {}
                Err(error) if is_unreadable(error.as_ref()) => {
                    out.push(Damage { item: Damaged::Record(state.clone()), reason: format!("{error:#}") })
                }
                Err(error) => return Err(error),
            }
        }
        let reported: HashSet<Id> = out
            .iter()
            .filter_map(|d| match d.item {
                Damaged::Object(id) | Damaged::Dictionary(id) => Some(id),
                _ => None,
            })
            .collect();
        let mut missing: Vec<(Id, String)> = self.walk(&roots, &HashSet::new())?.1.into_iter().filter(|(id, _)| !reported.contains(id)).collect();
        missing.sort();
        out.extend(missing.into_iter().map(|(id, reason)| Damage { item: Damaged::Missing(id), reason }));
        Ok(out)
    }

    /// Move a damaged object file or state record to `damaged/` (kept for inspection, out of every
    /// reader's way). A later write of the same object writes it again. Returns the new path.
    pub fn quarantine(&self, item: &Damaged) -> Result<PathBuf> {
        let (from, name) = match item {
            Damaged::Object(id) => (self.object_path(id), format!("object-{id}")),
            Damaged::Dictionary(id) => (self.root.join("dicts").join(id.hex()), format!("dictionary-{id}")),
            Damaged::Record(state) => {
                ensure!(state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid state id {state}");
                (self.root.join("states").join(state), format!("state-{state}"))
            }
            // Nothing is there to move: a write of the same content makes it again.
            Damaged::Missing(id) => bail!("object {id} has no file to move"),
        };
        self.readable().clear();
        let dir = self.root.join("damaged");
        fs::create_dir_all(&dir)?;
        let to = dir.join(name);
        fs::rename(&from, &to).with_context(|| format!("cannot move {} to {}", from.display(), to.display()))?;
        Ok(to)
    }

    /// Apply only the planned deletions, while state publication is excluded by the caller.
    pub fn prune_planned(&self, plan: &MaintenancePlan) -> Result<()> {
        self.readable().clear();
        for path in &plan.dead {
            if fs::remove_file(path).is_err() {
                // A clone of a sealed native state can retain its immutable flag.
                if let Some(hook) = self.clone_hook {
                    if let Ok(f) = File::open(path) {
                        let _ = hook(std::os::fd::AsFd::as_fd(&f));
                    }
                }
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Finish outside the project lock. A bounded batch packs the snapshot's selected loose
    /// objects; an explicit unbounded run also packs the newly created cold encodings.
    pub fn repack_planned(&self, plan: &MaintenancePlan) -> Result<PackReport> {
        self.evict_planned(&plan.cold)?;
        match plan.packing {
            PackingScope::Planned => self.pack_selected(Some(plan)),
            PackingScope::IncludingNewCold => self.pack(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{noise, scratch};

    #[test]
    fn handles_protect_unpublished_objects_through_consumption() {
        let dir = scratch("maintenance-leases");
        let root = dir.join("store");
        let store = Store::open(&root, Keying::Plain).unwrap();
        let (wire, _) = store.put_cold_bytes(b"temporary Cloud wire object", None).unwrap();
        assert!(Store::try_open_for_prune(&root, Keying::Plain).unwrap().is_none());
        // A Cloud fetch reopens its store after each pack. The replacement handle must
        // protect temporary objects after the previous handle goes.
        let replacement = Store::open_existing(&root, Keying::Plain).unwrap();
        drop(store);
        assert!(Store::try_open_for_prune(&root, Keying::Plain).unwrap().is_none());
        assert_eq!(replacement.read_blob(&wire).unwrap(), b"temporary Cloud wire object");
        drop(replacement);
        let pruning = Store::try_open_for_prune(&root, Keying::Plain).unwrap().unwrap();
        let probe = Store::prune_lease(&root).unwrap();
        assert_eq!(
            rustix::fs::flock(&probe, rustix::fs::FlockOperation::NonBlockingLockShared),
            Err(rustix::io::Errno::WOULDBLOCK)
        );
        pruning.finish_prune().unwrap();
        let reader = Store::open_existing(&root, Keying::Plain).unwrap();
        assert_eq!(reader.read_blob(&wire).unwrap(), b"temporary Cloud wire object");
    }

    #[test]
    fn planning_does_not_decode_loose_or_packed_raw_blobs() {
        use std::os::unix::fs::PermissionsExt;
        for packed in [false, true] {
            let dir = scratch(if packed { "plan-packed-raw" } else { "plan-loose-raw" });
            let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
            let src = dir.join("src");
            fs::create_dir(&src).unwrap();
            let data = noise(20_000, 771);
            fs::write(src.join("file"), &data).unwrap();
            let opts =
                crate::WriteOptions { temperature: Temperature::Cold, previous: None, exclude: Vec::new(), similar: false };
            let r = crate::write_tree(&store, &src, &opts).unwrap();
            store.set_state("kept", &StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() }).unwrap();
            let blob = Keying::Plain.id(Kind::Blob, &data);
            let path = if packed {
                // Only the RAW blob goes into this pack: trees/times stay loose, so planning
                // never needs to decode its block to read another object.
                let mut builder = Builder::new(store.options.block_size);
                builder.add(blob, Kind::Blob, pack::RAW, &data);
                let codec = crate::zstdx::Codec { level: 3, long: false, dict: None };
                store.write_pack(builder, &codec).unwrap();
                fs::read_dir(store.root.join("packs"))
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| p.extension().is_some_and(|e| e == "pack"))
                    .unwrap()
            } else {
                store.object_path(&blob)
            };
            // A loose frame's header is used for the packing budget. Truncate its body;
            // the header stays readable but any attempt to decode the RAW content fails.
            let mut bytes = fs::read(&path).unwrap();
            if packed {
                bytes[4] ^= 0xff;
            } else {
                bytes.truncate(bytes.len() - 5);
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&path, bytes).unwrap();
            assert!(store.read_blob(&blob).is_err());
            let plan = store.plan_maintenance(&["kept".to_string()].into(), &[], MaintenanceLimit::PERIODIC).unwrap();
            assert_eq!(plan.prune.objects, 0);
        }
    }

    #[test]
    fn maintenance_plan_is_read_only_and_matches_execution() {
        let dir = scratch("maintenance-plan");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let src = dir.join("src");
        fs::create_dir(&src).unwrap();
        let mut refs = Vec::new();
        for (name, content) in [("old", b"old history".as_slice()), ("head", b"current"), ("dead", b"unreachable")] {
            fs::write(src.join("file"), content).unwrap();
            let r = crate::write_tree(
                &store,
                &src,
                &crate::WriteOptions { temperature: Temperature::Hot, previous: None, exclude: Vec::new(), similar: false },
            )
            .unwrap();
            let r = StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() };
            store.set_state(name, &r).unwrap();
            refs.push(r);
        }
        let keep = ["old".to_string(), "head".to_string()].into();
        let before = store.stat().unwrap();
        let plan = store.plan_maintenance(&keep, &[refs[1].tree], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!((plan.prune.states, plan.prune.blobs), (1, 1));
        assert!(plan.prune.objects > 0);
        assert_eq!((plan.evict.blobs, plan.evict.bytes), (1, 11));
        assert!(plan.pack_objects > 0 && plan.pack_bytes > 0);
        assert_eq!(before.total_bytes(), store.stat().unwrap().total_bytes(), "planning writes nothing");
        assert!(store.state_ref("dead").unwrap().is_some());
        store.prune_planned(&plan).unwrap();
        let pruned = store.stat().unwrap();
        store.repack_planned(&plan).unwrap();
        let after = store.stat().unwrap();
        assert_eq!(pruned.hot_blobs - after.hot_blobs, plan.evict.blobs);
        assert_eq!(pruned.hot_bytes - after.hot_bytes, plan.evict.bytes);
        assert!(after.packed_objects - pruned.packed_objects >= plan.pack_objects);
        assert!(store.state_ref("dead").unwrap().is_none());
        let old = Keying::Plain.id(Kind::Blob, b"old history");
        assert!(store.hot_path(&old).is_none());
        assert_eq!(store.read_blob(&old).unwrap(), b"old history");
        assert!(store.hot_path(&Keying::Plain.id(Kind::Blob, b"current")).is_some());
        let next = store.plan_maintenance(&keep, &[refs[1].tree], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!((next.prune.states, next.prune.objects, next.prune.blobs, next.evict.blobs), (0, 0, 0, 0));
    }

    #[test]
    fn maintenance_plan_bounds_each_phase_and_makes_progress() {
        let dir = scratch("maintenance-bounded");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let src = dir.join("src");
        fs::create_dir(&src).unwrap();
        for n in 0..4 {
            fs::write(src.join(format!("file{n}")), vec![n; 100]).unwrap();
        }
        let r = crate::write_tree(
            &store,
            &src,
            &crate::WriteOptions { temperature: Temperature::Hot, previous: None, exclude: Vec::new(), similar: false },
        )
        .unwrap();
        store.set_state("kept", &StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() }).unwrap();
        let keep = ["kept".to_string()].into();
        let count_limit = MaintenanceLimit { files: 2, bytes: u64::MAX };
        let count_plan = store.plan_maintenance(&keep, &[], count_limit).unwrap();
        assert_eq!(count_plan.evict.blobs, 2, "the file limit is independent of the byte limit");
        // A byte budget smaller than one file still converts one oversized blob.
        let limit = MaintenanceLimit { files: 2, bytes: 1 };
        for remaining in (0..4).rev() {
            let plan = store.plan_maintenance(&keep, &[], limit).unwrap();
            assert_eq!((plan.evict.blobs, plan.evict.bytes), (1, 100));
            assert!(plan.pack_objects <= 1);
            store.prune_planned(&plan).unwrap();
            store.repack_planned(&plan).unwrap();
            assert_eq!(store.stat().unwrap().hot_blobs, remaining);
        }
        let plan = store.plan_maintenance(&Default::default(), &[], limit).unwrap();
        assert_eq!(plan.dead.len(), 1, "prune also admits at most one oversized file");
    }

    #[test]
    fn packing_budget_counts_decoded_input() {
        let dir = scratch("maintenance-pack-input");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (id, _) = store.put_cold_bytes(&vec![42; 800_000], None).unwrap();
        let path = store.object_path(&id);
        let bytes = fs::metadata(&path).unwrap().len();
        assert!(bytes < 1000);
        assert_eq!(check_loose(&path).unwrap().unwrap().input, 800_000);
    }

    /// Empty a stored file as a crash before its data reached the disk leaves it (#452).
    fn empty(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(path, b"").unwrap();
    }

    fn saved(store: &Store, dir: &Path, name: &str, files: &[(&str, &[u8])]) -> StateRef {
        let src = dir.join(name);
        fs::create_dir_all(&src).unwrap();
        for (file, data) in files {
            let path = src.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
        let opts = crate::WriteOptions { temperature: Temperature::Cold, previous: None, exclude: Vec::new(), similar: false };
        let r = crate::write_tree(store, &src, &opts).unwrap();
        let refs = StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() };
        store.set_state(name, &refs).unwrap();
        refs
    }

    #[test]
    fn a_damaged_loose_object_is_absent_reported_and_written_again() {
        let dir = scratch("damaged-object");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (id, _) = store.put_cold_bytes(b"content of a crashed save", None).unwrap();
        assert_eq!(store.object_damage(&id).unwrap(), None);
        empty(&store.object_path(&id));
        assert!(!store.has(&id), "an empty file is no object");
        assert_eq!(store.object_damage(&id).unwrap(), Some(format!("object {id} is empty")));
        assert_eq!(store.damage().unwrap(), vec![Damage { item: Damaged::Object(id), reason: format!("object {id} is empty") }]);
        // A header cut short and a foreign header are damage too.
        fs::write(store.object_path(&id), b"LOB1t").unwrap();
        assert_eq!(store.object_damage(&id).unwrap(), Some(format!("object {id} is cut short (5 bytes)")));
        fs::write(store.object_path(&id), b"LOB2tzxx").unwrap();
        assert_eq!(
            store.object_damage(&id).unwrap(),
            Some(format!("object {id} has no valid header: no valid loose object header"))
        );
        // A valid header and a byte: the frame header does not parse.
        fs::write(store.object_path(&id), b"LOB1bz\x28").unwrap();
        assert_eq!(store.object_damage(&id).unwrap(), Some(format!("object {id} has no valid zstd frame")));
        assert!(!store.has(&id));
        empty(&store.object_path(&id));
        let before = store.added.objects.load(Ordering::Relaxed);
        assert_eq!(store.put_cold_bytes(b"content of a crashed save", None).unwrap().0, id);
        assert_eq!(store.added.objects.load(Ordering::Relaxed), before + 1, "the write replaced the damaged file");
        assert_eq!(store.read_blob(&id).unwrap(), b"content of a crashed save");
        assert!(store.damage().unwrap().is_empty());
        assert!(fs::read_dir(dir.join("store/tmp")).unwrap().next().is_none(), "no temporary file is left");
        // A missing object is damage of whatever names it, not of the store.
        let missing = Keying::Plain.id(Kind::Blob, b"never written");
        assert_eq!(store.object_damage(&missing).unwrap(), Some(format!("object {missing} is missing")));
    }

    #[test]
    fn damaged_states_are_checked_reported_and_quarantined() {
        let dir = scratch("damaged-state");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let refs = saved(&store, &dir, "a", &[("f", b"file of a")]);
        assert_eq!(store.checked_state("a").unwrap(), Some(refs));
        assert_eq!(store.checked_state("never-saved").unwrap(), None);
        let unreadable = |state: &str| {
            let error = store.checked_state(state).unwrap_err();
            assert!(is_unreadable(error.as_ref()), "{error:#}");
            format!("{error:#}")
        };
        let record = dir.join("store/states/a");
        empty(&record);
        assert_eq!(unreadable("a"), "state a has no tree");
        let broken = Damage { item: Damaged::Record("a".into()), reason: "state a has no tree".into() };
        assert_eq!(store.damage().unwrap(), vec![broken]);
        store.set_state("a", &refs).unwrap();
        empty(&store.object_path(&refs.tree));
        assert_eq!(unreadable("a"), format!("state a: object {} is empty", refs.tree));
        let tree = Damage { item: Damaged::Object(refs.tree), reason: format!("object {} is empty", refs.tree) };
        assert_eq!(store.damage().unwrap(), vec![tree], "the walk does not report an object twice");
        let moved = store.quarantine(&Damaged::Object(refs.tree)).unwrap();
        assert_eq!(moved, dir.join("store/damaged").join(format!("object-{}", refs.tree)));
        assert!(!store.object_path(&refs.tree).exists());
        assert_eq!(unreadable("a"), format!("state a: object {} is missing", refs.tree));
        let missing = Damage { item: Damaged::Missing(refs.tree), reason: format!("object {} is missing", refs.tree) };
        assert_eq!(store.damage().unwrap(), vec![missing.clone()], "the walk reports what a state reaches");
        assert!(store.quarantine(&missing.item).is_err(), "nothing is there to move");
        empty(&record);
        store.quarantine(&Damaged::Record("a".into())).unwrap();
        assert_eq!(store.checked_state("a").unwrap(), None);
    }

    #[test]
    fn maintenance_skips_what_it_cannot_read_and_prunes_only_with_every_kept_record() {
        let dir = scratch("maintenance-damaged");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let a = saved(&store, &dir, "a", &[("f", b"file of a"), ("sub/g", b"shared")]);
        saved(&store, &dir, "dead", &[("f", b"only in dead")]);
        let keep: std::collections::BTreeSet<String> = ["a".to_string()].into();
        let only_dead = Keying::Plain.id(Kind::Blob, b"only in dead");
        // A kept record that cannot be read may reach anything: nothing goes.
        empty(&dir.join("store/states/a"));
        let plan = store.plan_maintenance(&keep, &[], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!(plan.damaged, vec!["state a has no tree".to_string()]);
        assert_eq!((plan.prune.states, plan.prune.objects, plan.prune.blobs), (0, 0, 0));
        store.prune_planned(&plan).unwrap();
        assert!(store.has(&only_dead));
        // A tree that cannot be read hides what it names: the walk is incomplete, nothing goes, and
        // the blob below it stays for a write that makes the tree again.
        store.set_state("a", &a).unwrap();
        let b = saved(&store, &dir, "b", &[("sub/h", b"only below a cut tree")]);
        let below = Keying::Plain.id(Kind::Blob, b"only below a cut tree");
        let sub = match store.get_tree(&b.tree).unwrap().entries[0].node {
            crate::tree::Node::Dir { tree } => tree,
            _ => unreachable!("sub is a folder"),
        };
        empty(&store.object_path(&sub));
        let keep: std::collections::BTreeSet<String> = ["a".to_string(), "b".to_string()].into();
        let plan = store.plan_maintenance(&keep, &[a.tree], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!(plan.damaged, vec![format!("object {sub} has no valid header: no valid loose object header")]);
        assert_eq!((plan.prune.states, plan.prune.objects, plan.prune.blobs), (0, 0, 0));
        store.prune_planned(&plan).unwrap();
        assert!(store.has(&only_dead) && store.has(&below));
        assert!(store.object_path(&sub).exists(), "a damaged object stays for fsck");
        // The same content written again repairs the tree; then the rest is pruned.
        assert_eq!(saved(&store, &dir, "b", &[("sub/h", b"only below a cut tree")]).tree, b.tree);
        let plan = store.plan_maintenance(&keep, &[a.tree], MaintenanceLimit::UNLIMITED).unwrap();
        assert!(plan.damaged.is_empty(), "{:?}", plan.damaged);
        assert_eq!(plan.prune.states, 1, "the record of dead goes once the walk is complete");
        store.prune_planned(&plan).unwrap();
        assert!(!store.has(&only_dead));
        assert_eq!(store.read_blob(&below).unwrap(), b"only below a cut tree");
        assert_eq!(store.read_blob(&Keying::Plain.id(Kind::Blob, b"shared")).unwrap(), b"shared");
        let tables = store.times_ids(&a.times).unwrap();
        assert_eq!(tables.len(), 2, "the times table of the folder stays with its root");
        // A body cut after a valid frame header shows only on a read: the repack leaves it loose,
        // reports it, and packs the rest.
        let file = Keying::Plain.id(Kind::Blob, b"file of a");
        let path = store.object_path(&file);
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        empty(&path);
        fs::write(&path, bytes).unwrap();
        let packed = store.repack_planned(&plan).unwrap();
        assert_eq!(packed.unreadable.len(), 1, "{:?}", packed.unreadable);
        assert!(packed.unreadable[0].contains(&file.hex()), "{:?}", packed.unreadable);
        assert!(packed.objects > 0 && path.exists());
    }

    #[test]
    fn maintenance_keeps_the_base_of_a_loose_delta_and_hot_blobs_read() {
        let dir = scratch("maintenance-delta-base");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let v1 = noise(200_000, 5);
        let mut v2 = v1.clone();
        v2[1000] ^= 1;
        let (base, _) = store.put_cold_bytes(&v1, None).unwrap();
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), &v2).unwrap();
        let (delta, _) = store.put_cold_bytes(&v2, Some(base)).unwrap();
        let opts = crate::WriteOptions { temperature: Temperature::Cold, previous: None, exclude: Vec::new(), similar: false };
        let r = crate::write_tree(&store, &src, &opts).unwrap();
        assert_eq!(store.form(&delta).unwrap(), Form::Delta { base, depth: 1 });
        store.set_state("kept", &StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() }).unwrap();
        let plan = store.plan_maintenance(&["kept".to_string()].into(), &[], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!(plan.prune.objects, 0, "the base of the delta stays");
        store.prune_planned(&plan).unwrap();
        assert_eq!(store.read_blob(&delta).unwrap(), v2);
        let (hot, _) = store.put_file(&src.join("f"), Temperature::Hot, None).unwrap();
        assert_eq!(store.object_damage(&hot).unwrap(), None, "a hot blob reads");
    }

    #[test]
    fn an_io_error_is_never_damage_nor_rewritten() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads any file: the refused open below cannot happen.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = scratch("io-error-not-damage");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (id, _) = store.put_cold_bytes(b"a valid object behind an I/O error", None).unwrap();
        let path = store.object_path(&id);
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let fsck = store.damage().unwrap_err();
        assert!(!is_unreadable(fsck.as_ref()), "fsck stops on it instead of reporting damage: {fsck:#}");
        assert!(store.object_damage(&id).is_err());
        assert!(store.put_cold_bytes(b"a valid object behind an I/O error", None).is_err(), "the writer stops too");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before, "nothing was rewritten");
        assert!(!dir.join("store/damaged").exists(), "nothing was moved aside");
    }

    #[test]
    fn dedupe_needs_the_chunks_base_and_dictionary_of_an_object() {
        let dir = scratch("dedupe-needs");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        // A loose chunk list whose chunk is gone.
        let big = noise(2 << 20, 21);
        let (chunked, _) = store.put_cold_bytes(&big, None).unwrap();
        let (chunk, _) = chunk_list(&store.payload(&chunked).unwrap().bytes).unwrap()[0];
        fs::remove_file(store.object_path(&chunk)).unwrap();
        assert!(!store.has(&chunked), "dedupe does not take a chunk list without its chunk");
        assert_eq!(store.object_damage(&chunked).unwrap(), Some(format!("object {chunked}: object {chunk} is missing")));
        assert!(store.damage().unwrap().is_empty(), "no state reaches it: it is garbage, not damage");
        store.put_cold_bytes(&big, None).unwrap();
        assert_eq!(store.read_blob(&chunked).unwrap(), big, "the write made the chunk again");
        assert!(store.damage().unwrap().is_empty());
        // A packed chunk list whose chunk is gone, reached from a state.
        let refs = saved(&store, &dir, "s", &[("big", &big)]);
        let list = store.payload(&chunked).unwrap().bytes;
        let mut builder = Builder::new(store.options.block_size);
        builder.add(chunked, Kind::Blob, pack::CHUNKED, &list);
        store.write_pack(builder, &crate::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
        assert!(!store.object_path(&chunked).exists(), "the list is packed");
        fs::remove_file(store.object_path(&chunk)).unwrap();
        // A handle remembers what it found readable; the chunk went behind its back.
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        assert!(!store.has(&chunked));
        let missing = store.damage().unwrap();
        assert_eq!(missing, vec![Damage { item: Damaged::Missing(chunk), reason: format!("object {chunk} is missing") }]);
        assert_eq!(store.checked_state("s").unwrap(), Some(refs), "the roots read; the walk finds the chunk");
        store.put_cold_bytes(&big, None).unwrap();
        assert_eq!(store.read_blob(&chunked).unwrap(), big);
        // A delta whose base is gone is written again whole.
        let v1 = noise(100_000, 22);
        let mut v2 = v1.clone();
        v2[9] ^= 1;
        let (base, _) = store.put_cold_bytes(&v1, None).unwrap();
        let (delta, _) = store.put_cold_bytes(&v2, Some(base)).unwrap();
        fs::remove_file(store.object_path(&base)).unwrap();
        assert!(!store.has(&delta));
        store.put_cold_bytes(&v2, Some(base)).unwrap();
        assert_eq!(store.form(&delta).unwrap(), Form::Whole);
        assert_eq!(store.read_blob(&delta).unwrap(), v2);
        // A dictionary frame whose dictionary is gone, and a dictionary that cannot be read.
        let dict = noise(4096, 23);
        let dict_id = Keying::Plain.id(Kind::Opaque, &dict);
        fs::write(dir.join("store/dicts").join(dict_id.hex()), &dict).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (small, _) = store.put_cold_bytes(b"a small file in a dictionary frame", None).unwrap();
        assert_eq!(&fs::read(store.object_path(&small)).unwrap()[..6], b"LOB1by");
        fs::write(dir.join("store/dicts").join(dict_id.hex()), b"").unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).expect("a damaged dictionary does not stop opening");
        assert!(!store.has(&small));
        let reported = store.damage().unwrap();
        assert!(
            reported.contains(&Damage {
                item: Damaged::Dictionary(dict_id),
                reason: format!("dictionary {dict_id} does not match its id")
            }),
            "{reported:?}"
        );
        assert_eq!(
            store.object_damage(&small).unwrap(),
            Some(format!("object {small}: dictionary {dict_id} does not match its id"))
        );
        store.quarantine(&Damaged::Dictionary(dict_id)).unwrap();
        store.put_cold_bytes(b"a small file in a dictionary frame", None).unwrap();
        assert_eq!(&fs::read(store.object_path(&small)).unwrap()[..6], b"LOB1bz", "written again without it");
        assert!(store.damage().unwrap().is_empty());
    }

    #[test]
    fn an_empty_pack_index_is_damage() {
        let dir = scratch("empty-pack-index");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (id, _) = store.put_cold_bytes(b"packed then cut", None).unwrap();
        store.pack().unwrap();
        let index = fs::read_dir(dir.join("store/packs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "idx"))
            .unwrap();
        empty(&index);
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let error = store.read_blob(&id).unwrap_err();
        assert!(is_unreadable(error.as_ref()), "{error:#}");
        assert!(!store.has(&id));
    }

    #[test]
    fn io_errors_stop_maintenance_fsck_and_opening_instead_of_counting_as_damage() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = scratch("io-errors-stop");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let refs = saved(&store, &dir, "a", &[("sub/f", b"behind an I/O error")]);
        let keep: std::collections::BTreeSet<String> = ["a".to_string()].into();
        let refuse = |path: &Path| fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        let allow = |path: &Path, mode| fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        let not_damage = |result: Result<()>| {
            let error = result.unwrap_err();
            assert!(!is_unreadable(error.as_ref()), "{error:#}");
        };
        // A tree that the walk reads, and the same tree as a hot tree outside the kept states.
        let tree = store.object_path(&refs.tree);
        refuse(&tree);
        not_damage(store.plan_maintenance(&keep, &[], MaintenanceLimit::UNLIMITED).map(drop));
        not_damage(store.plan_maintenance(&Default::default(), &[refs.tree], MaintenanceLimit::UNLIMITED).map(drop));
        not_damage(store.damage().map(drop));
        not_damage(store.pack().map(drop));
        allow(&tree, 0o444);
        // A state record.
        let record = dir.join("store/states/a");
        refuse(&record);
        not_damage(store.plan_maintenance(&keep, &[], MaintenanceLimit::UNLIMITED).map(drop));
        not_damage(store.damage().map(drop));
        allow(&record, 0o644);
        // A dictionary: fsck stops on it, and so does opening the store.
        let dict = noise(4096, 24);
        let dict_path = dir.join("store/dicts").join(Keying::Plain.id(Kind::Opaque, &dict).hex());
        fs::write(&dict_path, &dict).unwrap();
        refuse(&dict_path);
        not_damage(store.damage().map(drop));
        not_damage(Store::open(&dir.join("store"), Keying::Plain).map(drop));
        allow(&dict_path, 0o644);
        // A hot tree that cannot be read is skipped, not fatal: the walk reports it.
        let sub = match store.get_tree(&refs.tree).unwrap().entries[0].node {
            crate::tree::Node::Dir { tree } => tree,
            _ => unreachable!("sub is a folder"),
        };
        empty(&store.object_path(&sub));
        let plan = store.plan_maintenance(&keep, &[sub], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!(plan.damaged.len(), 1, "{:?}", plan.damaged);
    }

    #[test]
    fn a_delta_over_a_hot_base_reads() {
        let dir = scratch("delta-hot-base");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let v1 = noise(100_000, 25);
        let file = dir.join("v1");
        fs::write(&file, &v1).unwrap();
        let (base, _) = store.put_file(&file, Temperature::Hot, None).unwrap();
        let mut v2 = v1.clone();
        v2[3] ^= 1;
        let (delta, _) = store.put_cold_bytes(&v2, Some(base)).unwrap();
        assert_eq!(store.form(&delta).unwrap(), Form::Delta { base, depth: 1 });
        assert!(store.has(&delta), "its base is a hot blob");
        assert_eq!(store.object_damage(&delta).unwrap(), None);
    }

    #[test]
    fn a_repack_keeps_a_loose_copy_over_a_damaged_packed_form() {
        let dir = scratch("repack-keeps-healed");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let v1 = noise(100_000, 31);
        let mut v2 = v1.clone();
        v2[5] ^= 1;
        let (base, _) = store.put_cold_bytes(&v1, None).unwrap();
        let (delta, _) = store.put_cold_bytes(&v2, Some(base)).unwrap();
        let mut builder = Builder::new(store.options.block_size);
        let body = store.payload(&delta).unwrap().bytes;
        builder.add(delta, Kind::Blob, pack::DELTA, &body);
        store.write_pack(builder, &crate::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
        fs::remove_file(store.object_path(&base)).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        assert!(!store.has(&delta), "the packed delta lost its base");
        store.put_cold_bytes(&v2, None).unwrap();
        assert!(store.object_path(&delta).exists(), "healed as a loose whole object");
        store.pack().unwrap();
        assert!(store.object_path(&delta).exists(), "the repack keeps it: the packed form is damaged");
        assert_eq!(store.read_blob(&delta).unwrap(), v2);
    }

    #[test]
    fn an_unreadable_dependency_stops_the_writer() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = scratch("unreadable-dependency");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let v1 = noise(100_000, 32);
        let mut v2 = v1.clone();
        v2[6] ^= 1;
        let (base, _) = store.put_cold_bytes(&v1, None).unwrap();
        let (delta, _) = store.put_cold_bytes(&v2, Some(base)).unwrap();
        let before = fs::read(store.object_path(&delta)).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        fs::set_permissions(store.object_path(&base), fs::Permissions::from_mode(0o000)).unwrap();
        let error = store.put_cold_bytes(&v2, None).unwrap_err();
        assert!(!is_unreadable(error.as_ref()), "{error:#}");
        fs::set_permissions(store.object_path(&base), fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(fs::read(store.object_path(&delta)).unwrap(), before, "the delta was not rewritten without its base");
    }

    #[test]
    fn self_referencing_chunk_lists_and_deltas_are_damage() {
        let dir = scratch("self-reference");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let frame = zst::compress(b"x", None).unwrap();
        let list = Keying::Plain.id(Kind::Blob, b"a chunk list that names itself");
        let mut bytes = b"LOB1bc".to_vec();
        varint_into(&mut bytes, 1);
        bytes.extend_from_slice(&list.0);
        varint_into(&mut bytes, 1);
        let place = |id: &Id, bytes: &[u8]| {
            let path = store.object_path(id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        };
        place(&list, &bytes);
        assert_eq!(store.object_damage(&list).unwrap(), Some(format!("object {list} is a blob, not a chunk")));
        let delta = Keying::Plain.id(Kind::Blob, b"a delta over itself");
        let mut bytes = b"LOB1bd\x01".to_vec();
        bytes.extend_from_slice(&delta.0);
        bytes.extend_from_slice(&frame);
        place(&delta, &bytes);
        assert_eq!(store.object_damage(&delta).unwrap(), Some(format!("the delta chain of object {delta} loops or is too long")));
        assert!(!store.has(&delta) && !store.has(&list));
    }

    #[test]
    fn a_handle_checks_each_need_once() {
        let dir = scratch("needs-once");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let big = noise(2 << 20, 33);
        let (chunked, _) = store.put_cold_bytes(&big, None).unwrap();
        assert!(store.has(&chunked));
        let (chunk, _) = chunk_list(&store.payload(&chunked).unwrap().bytes).unwrap()[0];
        fs::remove_file(store.object_path(&chunk)).unwrap();
        assert!(store.has(&chunked), "this handle read the chunk once and does not read it again");
        assert!(!Store::open(&dir.join("store"), Keying::Plain).unwrap().has(&chunked), "a new handle reads it again");
    }

    #[test]
    fn a_delta_chain_is_limited_like_a_read() {
        let dir = scratch("delta-chain-limit");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let (whole, _) = store.put_cold_bytes(b"the end of a chain", None).unwrap();
        let frame = zst::compress(b"x", None).unwrap();
        // Delta n names delta n - 1 as its base; delta 0 names the whole blob.
        let mut base = whole;
        let mut chain = Vec::new();
        for n in 0..257u32 {
            let id = Keying::Plain.id(Kind::Blob, format!("delta {n}").as_bytes());
            let mut bytes = b"LOB1bd\x01".to_vec();
            bytes.extend_from_slice(&base.0);
            bytes.extend_from_slice(&frame);
            let path = store.object_path(&id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            chain.push(id);
            base = id;
        }
        assert_eq!(store.object_damage(&chain[254]).unwrap(), None, "255 bases read");
        let why = store.object_damage(&chain[256]).unwrap().unwrap();
        assert!(
            why.starts_with(&format!("object {}: the delta chain", chain[256])) && why.ends_with("loops or is too long"),
            "{why}"
        );
    }

    #[test]
    fn a_repack_checks_the_packed_form_before_a_loose_copy_goes() {
        let dir = scratch("repack-packed-form");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let v1 = noise(100_000, 34);
        let file = dir.join("v1");
        fs::write(&file, &v1).unwrap();
        let (hot, _) = store.put_file(&file, Temperature::Hot, None).unwrap();
        let mut v2 = v1.clone();
        v2[7] ^= 1;
        let (delta, _) = store.put_cold_bytes(&v2, Some(hot)).unwrap();
        let loose = fs::read(store.object_path(&delta)).unwrap();
        let mut builder = Builder::new(store.options.block_size);
        builder.add(delta, Kind::Blob, pack::DELTA, &loose[6..]);
        store.write_pack(builder, &crate::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
        let restore = || fs::write(store.object_path(&delta), &loose).unwrap();
        // A packed delta over a hot base reads: the loose copy goes.
        restore();
        store.pack().unwrap();
        assert!(!store.object_path(&delta).exists(), "the packed form reads over its hot base");
        // A packed form whose block does not decode keeps the loose copy; the repack goes on.
        let data = fs::read_dir(dir.join("store/packs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "pack"))
            .unwrap();
        let mut bytes = fs::read(&data).unwrap();
        bytes[pack::DATA_MAGIC.len()] ^= 0xff;
        empty(&data);
        fs::write(&data, bytes).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        fs::write(store.object_path(&delta), &loose).unwrap();
        store.pack().unwrap();
        assert!(store.object_path(&delta).exists(), "the loose copy stays over a damaged block");
        assert_eq!(store.read_blob(&delta).unwrap(), v2);
    }

    #[test]
    fn a_repack_keeps_a_loose_copy_that_a_dependency_of_its_packed_form_needs() {
        let dir = scratch("repack-cycle");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let vb = noise(100_000, 41);
        let mut va = vb.clone();
        va[11] ^= 1;
        // A is packed as a delta over B; B is lost.
        let (b, _) = store.put_cold_bytes(&vb, None).unwrap();
        let (a, _) = store.put_cold_bytes(&va, Some(b)).unwrap();
        let body = store.payload(&a).unwrap().bytes;
        let mut builder = Builder::new(store.options.block_size);
        builder.add(a, Kind::Blob, pack::DELTA, &body);
        store.write_pack(builder, &crate::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
        fs::remove_file(store.object_path(&b)).unwrap();
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        // A is healed as a loose whole object, and B comes back as a delta over that loose A.
        store.put_cold_bytes(&va, None).unwrap();
        store.put_cold_bytes(&vb, Some(a)).unwrap();
        assert_eq!(store.form(&b).unwrap(), Form::Delta { base: a, depth: 1 });
        store.pack().unwrap();
        assert!(store.object_path(&a).exists(), "the packed A needs B, which needs this loose A");
        assert_eq!(store.read_blob(&a).unwrap(), va);
        assert_eq!(store.read_blob(&b).unwrap(), vb);
    }

    #[test]
    fn fsck_reports_every_missing_chunk() {
        let dir = scratch("fsck-every-chunk");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let big = noise(3 << 20, 42);
        saved(&store, &dir, "s", &[("big", &big)]);
        let chunked = Keying::Plain.id(Kind::Blob, &big);
        let chunks = chunk_list(&store.payload(&chunked).unwrap().bytes).unwrap();
        assert!(chunks.len() > 2);
        let lost: Vec<Id> = chunks.iter().take(2).map(|(chunk, _)| *chunk).collect();
        for chunk in &lost {
            fs::remove_file(store.object_path(chunk)).unwrap();
        }
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let mut missing: Vec<Damaged> = store.damage().unwrap().into_iter().map(|d| d.item).collect();
        missing.sort_by_key(|d| format!("{d:?}"));
        let mut want: Vec<Damaged> = lost.into_iter().map(Damaged::Missing).collect();
        want.sort_by_key(|d| format!("{d:?}"));
        assert_eq!(missing, want);
    }

    #[test]
    fn a_chunk_without_its_dictionary_stays_damage_when_checked_again() {
        let dir = scratch("chunk-dictionary-again");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let place = |id: &Id, bytes: &[u8]| {
            let path = store.object_path(id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        };
        // A chunk in a dictionary frame whose dictionary is missing, named by a chunk list.
        let dict = Keying::Plain.id(Kind::Opaque, b"a dictionary that is not there");
        let chunk = Keying::Plain.id(Kind::Chunk, b"a chunk");
        let mut bytes = b"LOB1cy".to_vec();
        bytes.extend_from_slice(&dict.0);
        bytes.extend_from_slice(&zst::compress(b"a chunk", None).unwrap());
        place(&chunk, &bytes);
        let list = Keying::Plain.id(Kind::Blob, b"a chunk");
        let mut bytes = b"LOB1bc".to_vec();
        varint_into(&mut bytes, 1);
        bytes.extend_from_slice(&chunk.0);
        varint_into(&mut bytes, 7);
        place(&list, &bytes);
        for _ in 0..2 {
            let why = store.object_damage(&list).unwrap().unwrap();
            assert!(why.contains(&format!("dictionary {dict} is missing")), "{why}");
            assert!(!store.has(&list));
        }
    }

    #[test]
    fn maintenance_keeps_cold_dependencies_even_with_a_hot_copy() {
        let dir = scratch("maintenance-cold-dependencies");
        let store = Store::open(&dir.join("store"), Keying::Plain).unwrap();
        let src = dir.join("src");
        fs::create_dir(&src).unwrap();
        let data = noise(2 << 20, 17);
        fs::write(src.join("large"), &data).unwrap();
        let opts = crate::WriteOptions { temperature: Temperature::Cold, previous: None, exclude: Vec::new(), similar: false };
        let r = crate::write_tree(&store, &src, &opts).unwrap();
        let refs = StateRef { tree: r.tree.unwrap(), times: r.times.unwrap() };
        store.set_state("kept", &refs).unwrap();
        let (blob, _) = store.put_file(&src.join("large"), Temperature::Hot, None).unwrap();
        let plan = store.plan_maintenance(&["kept".to_string()].into(), &[], MaintenanceLimit::UNLIMITED).unwrap();
        assert_eq!(plan.prune.objects, 0, "the cold encoding's chunks must survive pruning");
        store.prune_planned(&plan).unwrap();
        store.repack_planned(&plan).unwrap();
        assert_eq!(store.read_blob(&blob).unwrap(), data);
    }
}
