//! Reachability and bounded object-store maintenance. Filesystem and project policy live
//! above this crate; these plans are shared by dry runs and destructive execution.

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
}

#[derive(Debug, Default)]
enum PackingScope {
    #[default]
    Planned,
    IncludingNewCold,
}

impl Store {
    pub(super) fn prune_lease(root: &Path) -> Result<File> {
        let path = root.join("prune.lock");
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
            Ok(()) => Ok(Some(Self::open_leased(root, keying, lease)?)),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Readers/writers can resume before the caller releases its project lock. The shared
    /// lease continues to protect the repack's inputs against another pruning process.
    pub fn finish_prune(&self) -> Result<()> {
        rustix::fs::flock(&self.prune_lease, rustix::fs::FlockOperation::LockShared)?;
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
            for e in self.get_tree(&t)?.entries {
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
                if !self.has_object(id) {
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
                if let Some(r) = self.state_ref(&name)? {
                    roots.push(r);
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
        // Visit shared subtrees/times only once across the entire history, then follow
        // cold dependencies even when a hot copy of the logical blob also exists.
        let mut reach = HashSet::new();
        let mut work: Vec<_> = roots.iter().flat_map(|r| [(r.tree, Kind::Tree), (r.times, Kind::Times)]).collect();
        while let Some((id, kind)) = work.pop() {
            if !reach.insert(id) {
                continue;
            }
            match kind {
                Kind::Tree => {
                    for entry in self.get_tree(&id)?.entries {
                        match entry.node {
                            crate::tree::Node::Dir { tree } => work.push((tree, Kind::Tree)),
                            crate::tree::Node::File { blob, .. } => work.push((blob, Kind::Blob)),
                            crate::tree::Node::Symlink { .. } => {}
                        }
                    }
                }
                Kind::Times => {
                    for record in self.get_times(&id)?.records {
                        if let crate::times::Record::Folder(child) = record {
                            work.push((child, Kind::Times));
                        }
                    }
                }
                Kind::Blob => work.extend(self.blob_dependencies(&id)?),
                _ => {}
            }
        }
        plan.roots = roots;
        let hot = self.hot_blobs(hot_trees)?;
        for (folder, is_hot) in [("objects", false), ("blobs", true)] {
            self.visit_maintenance_files(folder, |file| {
                let id = file.id;
                if !reach.contains(&id) {
                    if limit.admits(plan.dead.len(), plan.prune.bytes, file.bytes) {
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
                } else if plan.pack.len() < limit.files {
                    // A tiny zstd frame can decode to a very large payload: bound the
                    // compressor's input, rather than its already-compressed disk size.
                    let input = Self::pack_input_bytes(&file)?;
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

    /// RAW blob contents have no dependencies. Inspect only a loose header or pack index;
    /// read bodies only for delta/chunk references, never for dependency-free RAW frames.
    fn blob_dependencies(&self, id: &Id) -> Result<Vec<(Id, Kind)>> {
        match File::open(self.object_path(id)) {
            Ok(mut file) => {
                let mut header = [0; 6];
                file.read_exact(&mut header)?;
                ensure!(&header[..4] == MAGIC && header[4] == Kind::Blob.byte(), "invalid blob header {id}");
                match header[5] {
                    WHOLE | WHOLE_DICT => return Ok(Vec::new()),
                    DELTA => {
                        let mut body = [0; 33];
                        file.read_exact(&mut body)?;
                        let mut base = [0; 32];
                        base.copy_from_slice(&body[1..]);
                        return Ok(vec![(Id(base), Kind::Blob)]);
                    }
                    CHUNKED => {
                        let mut body = Vec::new();
                        file.read_to_end(&mut body)?;
                        return Self::chunk_dependencies(&body);
                    }
                    form => bail!("unknown blob form {form:#x} for {id}"),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let set = self.pack_set();
        let Some((_, entry)) = set.find(id, &self.last_pack)? else {
            ensure!(self.hot_path(id).is_some(), "blob {id} is missing");
            return Ok(Vec::new());
        };
        ensure!(entry.kind == Kind::Blob, "object {id} is not a blob");
        match entry.form {
            pack::RAW => Ok(Vec::new()),
            pack::CHUNKED => Self::chunk_dependencies(&self.payload(id)?.bytes),
            pack::DELTA => {
                let body = self.payload(id)?.bytes;
                ensure!(body.len() > 33, "delta object {id} is cut short");
                let mut base = [0; 32];
                base.copy_from_slice(&body[1..33]);
                Ok(vec![(Id(base), Kind::Blob)])
            }
            form => bail!("unknown packed blob form {form:#x} for {id}"),
        }
    }

    fn chunk_dependencies(body: &[u8]) -> Result<Vec<(Id, Kind)>> {
        let mut reader = Reader::new(body);
        let mut out = Vec::new();
        for _ in 0..reader.varint()? {
            out.push((reader.id()?, Kind::Chunk));
            reader.varint()?;
        }
        ensure!(reader.done(), "bytes after chunk references");
        Ok(out)
    }

    fn pack_input_bytes(file: &MaintenanceFile) -> Result<u64> {
        let mut header = Vec::new();
        File::open(&file.path)?.take(56).read_to_end(&mut header)?;
        ensure!(header.len() >= 6 && &header[..4] == MAGIC, "object {} has no valid header", file.id);
        match header[5] {
            WHOLE | WHOLE_DICT => {
                let offset = if header[5] == WHOLE_DICT { 38 } else { 6 };
                let frame = header.get(offset..).context("truncated object frame")?;
                zstd_safe::get_frame_content_size(frame)
                    .map_err(|_| anyhow::anyhow!("object {} has an invalid zstd frame", file.id))?
                    .context("object frame has no content size")
            }
            DELTA | CHUNKED => Ok(file.bytes - 6), // The checked header proves this length.
            form => bail!("object {} has unknown form {form:#x}", file.id),
        }
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

    /// Apply only the planned deletions, while state publication is excluded by the caller.
    pub fn prune_planned(&self, plan: &MaintenancePlan) -> Result<()> {
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
    pub fn repack_planned(&self, plan: &MaintenancePlan) -> Result<()> {
        self.evict_planned(&plan.cold)?;
        match plan.packing {
            PackingScope::Planned => self.pack_selected(Some(plan))?,
            PackingScope::IncludingNewCold => self.pack()?,
        };
        Ok(())
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
        assert_eq!(Store::pack_input_bytes(&MaintenanceFile { id, path, bytes }).unwrap(), 800_000);
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
