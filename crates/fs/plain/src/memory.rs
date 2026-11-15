use layr_fs_core::{evidence::FType, *};
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
enum Data {
    Directory,
    File(Arc<Vec<u8>>),
    Symlink(Vec<u8>),
}
#[derive(Clone)]
struct Entry {
    data: Data,
    attributes: EntryAttributes,
    inode: u64,
}
#[derive(Clone)]
struct State {
    entries: BTreeMap<Vec<u8>, Entry>,
    next_inode: u64,
}
/// Memory has no host paths, clocks, random IDs or global state.
pub struct MemoryTree {
    state: Mutex<State>,
}
impl MemoryTree {
    pub fn new(attributes: EntryAttributes) -> Self {
        Self {
            state: Mutex::new(State {
                entries: BTreeMap::from([(Vec::new(), Entry { data: Data::Directory, attributes, inode: 0 })]),
                next_inode: 1,
            }),
        }
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| FsError::Invalid("memory tree lock poisoned"))
    }
    fn parent(state: &State, path: &RelPath) -> Result<()> {
        let bytes = path.as_bytes();
        let parent = bytes.iter().rposition(|b| *b == b'/').map_or(b"".as_slice(), |i| &bytes[..i]);
        if matches!(state.entries.get(parent).map(|e| &e.data), Some(Data::Directory)) {
            Ok(())
        } else {
            Err(FsError::Invalid("parent is absent or not a directory"))
        }
    }
    fn snapshot(&self) -> Result<Self> {
        Ok(Self { state: Mutex::new(self.state()?.clone()) })
    }
}
impl TreeRead for MemoryTree {
    fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>> {
        let state = self.state()?;
        let Some(entry) = state.entries.get(path.as_bytes()) else { return Ok(None) };
        let (kind, size) = match &entry.data {
            Data::Directory => (FType::Dir, 0),
            Data::File(bytes) => (FType::File, bytes.len() as u64),
            Data::Symlink(target) => (FType::Symlink, target.len() as u64),
        };
        let link = if kind == FType::File {
            Some(LinkIdentity {
                device: 0,
                inode: entry.inode,
                count: state.entries.values().filter(|e| e.inode == entry.inode).count() as u64,
            })
        } else {
            None
        };
        Ok(Some(EntryMetadata { kind, size, link, attributes: entry.attributes.clone() }))
    }
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()> {
        let names = {
            let state = self.state()?;
            if !matches!(state.entries.get(dir.as_bytes()).map(|e| &e.data), Some(Data::Directory)) {
                return Err(FsError::Invalid("not a directory"));
            }
            let prefix = if dir.as_bytes().is_empty() { Vec::new() } else { [dir.as_bytes(), b"/"].concat() };
            state
                .entries
                .keys()
                .filter_map(|path| path.strip_prefix(prefix.as_slice()))
                .filter(|name| !name.is_empty() && !name.contains(&b'/'))
                .map(|name| RelPath::new(name.to_vec()))
                .collect::<Result<Vec<_>>>()?
        };
        for name in names {
            visit(&name)?;
        }
        Ok(())
    }
    fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>> {
        match self.state()?.entries.get(path.as_bytes()).map(|e| &e.data) {
            Some(Data::File(bytes)) => Ok(Box::new(Cursor::new(bytes.as_ref().clone()))),
            _ => Err(FsError::Invalid("not a regular file")),
        }
    }
    fn read_link(&self, path: &RelPath) -> Result<Vec<u8>> {
        match self.state()?.entries.get(path.as_bytes()).map(|e| &e.data) {
            Some(Data::Symlink(target)) => Ok(target.clone()),
            _ => Err(FsError::Invalid("not a symbolic link")),
        }
    }
}
impl TreeWrite for MemoryTree {
    fn put(&self, path: &RelPath, data: EntryData<'_>, attributes: &EntryAttributes) -> Result<()> {
        // Read before mutation, so an injected reader error cannot replace the old entry.
        let data = match data {
            EntryData::File(reader) => {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                Data::File(Arc::new(bytes))
            }
            EntryData::Directory => Data::Directory,
            EntryData::Symlink(target) => Data::Symlink(target.to_vec()),
            EntryData::HardLink(source) => {
                let mut state = self.state()?;
                Self::parent(&state, path)?;
                let entry = state
                    .entries
                    .get(source.as_bytes())
                    .filter(|e| matches!(e.data, Data::File(_)))
                    .cloned()
                    .ok_or(FsError::Invalid("hard-link source is not a regular file"))?;
                if state.entries.contains_key(path.as_bytes()) {
                    return Err(FsError::Invalid("hard-link target exists"));
                }
                state.entries.insert(path.as_bytes().to_vec(), entry);
                return Ok(());
            }
        };
        let mut state = self.state()?;
        Self::parent(&state, path)?;
        if matches!(data, Data::Directory) && state.entries.contains_key(path.as_bytes()) {
            return Err(FsError::Invalid("destination exists"));
        }
        if state.entries.get(path.as_bytes()).is_some_and(|e| matches!(e.data, Data::Directory)) {
            return Err(FsError::Invalid("destination directory exists"));
        }
        let inode = state.next_inode;
        state.next_inode = inode.checked_add(1).ok_or(FsError::Invalid("inode space exhausted"))?;
        state.entries.insert(path.as_bytes().to_vec(), Entry { data, attributes: attributes.clone(), inode });
        Ok(())
    }
    fn rename_new(&self, source: &RelPath, destination: &RelPath) -> Result<()> {
        let mut state = self.state()?;
        Self::parent(&state, destination)?;
        if state.entries.contains_key(destination.as_bytes()) {
            return Err(FsError::Invalid("destination exists"));
        }
        if !state.entries.contains_key(source.as_bytes()) {
            return Err(FsError::Invalid("source absent"));
        }
        let prefix = [source.as_bytes(), b"/"].concat();
        if destination.as_bytes().starts_with(&prefix) {
            return Err(FsError::Invalid("cannot move a directory inside itself"));
        }
        let moved: Vec<_> =
            state.entries.keys().filter(|p| p.as_slice() == source.as_bytes() || p.starts_with(&prefix)).cloned().collect();
        for path in moved {
            if let Some(entry) = state.entries.remove(&path) {
                let target = [destination.as_bytes(), &path[source.as_bytes().len()..]].concat();
                state.entries.insert(target, entry);
            }
        }
        Ok(())
    }
    fn remove_entry(&self, path: &RelPath) -> Result<()> {
        let mut state = self.state()?;
        let prefix = [path.as_bytes(), b"/"].concat();
        state.entries.retain(|key, _| key != path.as_bytes() && !key.starts_with(&prefix));
        Ok(())
    }
    fn remove_empty_dir(&self, path: &RelPath) -> Result<bool> {
        let mut state = self.state()?;
        match state.entries.get(path.as_bytes()).map(|e| &e.data) {
            None => return Ok(true),
            Some(Data::Directory) => {}
            _ => return Err(FsError::Invalid("not a directory")),
        }
        let prefix = [path.as_bytes(), b"/"].concat();
        if state.entries.keys().any(|p| p.starts_with(&prefix)) {
            return Ok(false);
        }
        state.entries.remove(path.as_bytes());
        Ok(true)
    }
    fn overwrite_file(&self, path: &RelPath, source: &mut dyn std::io::Read, modified_ns: i64) -> Result<Attempt<()>> {
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        let mut state = self.state()?;
        replace_linked_data(&mut state, path, bytes, modified_ns)?;
        Ok(Attempt::Applied(()))
    }
    fn write_range(&self, path: &RelPath, offset: u64, bytes: &[u8], modified_ns: i64) -> Result<()> {
        let mut state = self.state()?;
        let Some(Entry { data: Data::File(old), .. }) = state.entries.get(path.as_bytes()) else {
            return Err(FsError::Invalid("not a regular file"));
        };
        let start = usize::try_from(offset).map_err(|_| FsError::Invalid("range out of bounds"))?;
        let end =
            start.checked_add(bytes.len()).filter(|end| *end <= old.len()).ok_or(FsError::Invalid("range out of bounds"))?;
        let mut updated = old.as_ref().clone();
        updated[start..end].copy_from_slice(bytes);
        replace_linked_data(&mut state, path, updated, modified_ns)
    }
    fn set_metadata(&self, path: &RelDir, attributes: &EntryAttributes) -> Result<()> {
        let mut state = self.state()?;
        let inode = state.entries.get(path.as_bytes()).ok_or(FsError::Invalid("entry absent"))?.inode;
        for entry in state.entries.values_mut().filter(|e| e.inode == inode) {
            entry.attributes = attributes.clone();
        }
        Ok(())
    }
}

fn replace_linked_data(state: &mut State, path: &RelPath, bytes: Vec<u8>, modified_ns: i64) -> Result<()> {
    let Some(Entry { data: Data::File(_), inode, .. }) = state.entries.get(path.as_bytes()) else {
        return Err(FsError::Invalid("not a regular file"));
    };
    let inode = *inode;
    let bytes = Arc::new(bytes);
    for entry in state.entries.values_mut().filter(|e| e.inode == inode) {
        entry.data = Data::File(bytes.clone());
        entry.attributes.modified_ns = modified_ns;
        entry.attributes.stored_time = Some(modified_ns);
    }
    Ok(())
}

struct Stored {
    name: Vec<u8>,
    tree: Arc<MemoryTree>,
}
pub struct MemoryFs {
    issuer: TreeIssuer,
    trees: Mutex<HashMap<TreeKey, Stored>>,
    fail_publication: std::sync::atomic::AtomicBool,
}
impl Default for MemoryFs {
    fn default() -> Self {
        Self {
            issuer: TreeIssuer::default(),
            trees: Mutex::new(HashMap::new()),
            fail_publication: std::sync::atomic::AtomicBool::new(false),
        }
    }
}
impl MemoryFs {
    pub fn location(&self, name: RelPath) -> Result<NewTreeLocation> {
        self.issuer.location(name, Arc::new(()))
    }
    pub fn fail_next_publication(&self) {
        self.fail_publication.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn trees(&self) -> Result<std::sync::MutexGuard<'_, HashMap<TreeKey, Stored>>> {
        self.trees.lock().map_err(|_| FsError::Invalid("registry lock poisoned"))
    }
    fn publish(&self, name: &[u8], key: TreeKey, tree: Arc<MemoryTree>) -> Result<()> {
        if self.fail_publication.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return Err(FsError::Invalid("injected publication failure"));
        }
        let mut trees = self.trees()?;
        if trees.values().any(|stored| stored.name == name) {
            return Err(FsError::Invalid("destination exists"));
        }
        trees.insert(key, Stored { name: name.to_vec(), tree });
        Ok(())
    }
}
impl Filesystem for MemoryFs {
    fn create_empty(&self, dst: NewTreeLocation) -> Result<CreatedTree> {
        self.issuer.location_key(&dst)?;
        let tree = Arc::new(MemoryTree::new(EntryAttributes {
            mode: 0o755,
            uid: 0,
            gid: 0,
            modified_ns: 0,
            stored_time: Some(0),
            xattrs: Vec::new(),
            acl: None,
        }));
        let created = self.issuer.create(tree.clone());
        self.publish(dst.name().as_bytes(), self.issuer.working_key(&created.tree)?, tree)?;
        Ok(created)
    }
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> Result<CreatedTree> {
        self.issuer.location_key(&dst)?;
        let tree = Arc::new(MemoryTree::new(layr_fs_common::required(source, &RelDir::Root)?.attributes));
        layr_fs_common::copy_tree(source, tree.as_ref())?;
        let created = self.issuer.create(tree.clone());
        self.publish(dst.name().as_bytes(), self.issuer.working_key(&created.tree)?, tree)?;
        Ok(created)
    }
    fn freeze(&self, source: FreezeSource<'_>, dst: NewTreeLocation) -> Result<Capture> {
        let source = match source {
            FreezeSource::Captured(capture) => return layr_fs_common::republish(self, &self.issuer, capture, dst),
            FreezeSource::Working(source) => source,
            FreezeSource::Reader(reader) => {
                let created = self.materialise(reader, dst)?;
                return self.issuer.seal(created, CaptureConsistency::UnsettledCopy);
            }
        };
        self.issuer.location_key(&dst)?;
        let key = self.issuer.working_key(source)?;
        let snapshot = self.trees()?.get(&key).ok_or(FsError::Invalid("tree absent"))?.tree.snapshot()?;
        let snapshot = Arc::new(snapshot);
        let capture = self.issuer.capture(snapshot.clone(), CaptureConsistency::AtomicPoint);
        self.publish(dst.name().as_bytes(), self.issuer.frozen_key(&capture.tree)?, snapshot)?;
        Ok(capture)
    }
    fn changes(
        &self,
        before: &FrozenTree,
        after: &FrozenTree,
        request: layr_fs_core::ChangeRequest,
    ) -> Result<Vec<evidence::EntryChange>> {
        self.issuer.frozen_key(before)?;
        self.issuer.frozen_key(after)?;
        layr_fs_common::changes(self, before, after, request)
    }
    fn remove(&self, tree: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(tree, |key| {
            self.trees()?.remove(key).ok_or(FsError::Invalid("tree absent"))?;
            Ok(())
        })
    }
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_> {
        Capabilities {
            exchange: self
                .issuer
                .scope_key(scope)
                .ok()
                .filter(|key| self.trees().is_ok_and(|trees| trees.contains_key(key)))
                .map(|_| self as &dyn ExchangeTrees),
            ..Capabilities::default()
        }
    }
}

impl ExchangeTrees for MemoryFs {
    fn exchange(&self, first: OwnedTree, second: OwnedTree) -> std::result::Result<(OwnedTree, OwnedTree), ExchangeFailure> {
        self.issuer.exchange(first, second, |a, b| {
            let mut trees = self.trees()?;
            // TreeIssuer checked that the two keys are distinct before invoking this closure.
            let [Some(a), Some(b)] = trees.get_disjoint_mut([a, b]) else {
                return Err(FsError::Invalid("exchange tree absent").into());
            };
            std::mem::swap(&mut a.name, &mut b.name);
            Ok(())
        })
    }
}
