//! Bulk accounting uses whole btrfs disk extents, including compressed and partially referenced
//! extents. Repeated references in one file count once. Inline items count their stored payload;
//! holes do not count. These keys describe the btrfs allocation, not FIEMAP's merged ranges.
//! Each tree is read once with TREE_SEARCH_V2, which requires CAP_SYS_ADMIN.
//! The bulk path reports committed on-disk allocation; very recent writes may not appear yet.

use crate::fsutil::{ExtentKey, Held};
use crate::sys::{self, SearchKey, TreeSearch};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

pub type PartId = usize;
pub struct TreeUnit {
    pub root: PathBuf,
    pub parts: HashMap<u64, PartId>,
    tree: u64,
    lineage: [u8; 16],
}
pub struct Plan {
    pub units: Vec<TreeUnit>,
    pub outside: Vec<PartId>,
}

fn mount_points(data: &[u8]) -> io::Result<Vec<PathBuf>> {
    data.split(|&b| b == b'\n').filter(|line| !line.is_empty()).map(|line| {
        let field = line.split(|&b| b == b' ').filter(|v| !v.is_empty()).nth(4).ok_or_else(invalid)?;
        let mut decoded = Vec::with_capacity(field.len());
        let mut i = 0;
        while i < field.len() {
            if field[i] == b'\\' {
                let digits = field.get(i + 1..i + 4).ok_or_else(invalid)?;
                if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) { return Err(invalid()); }
                let value = u16::from(digits[0] - b'0') * 64 + u16::from(digits[1] - b'0') * 8 + u16::from(digits[2] - b'0');
                decoded.push(u8::try_from(value).map_err(|_| invalid())?);
                i += 4;
            } else {
                decoded.push(field[i]);
                i += 1;
            }
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }).collect()
}

pub fn nested_mount(parts: &[PathBuf]) -> io::Result<Option<PathBuf>> {
    let mounts = mount_points(&std::fs::read("/proc/self/mountinfo")?)?;
    for part in parts {
        let part = match part.canonicalize() {
            Ok(part) => part,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if let Some(mount) = mounts.iter().find(|mount| *mount != &part && mount.starts_with(&part)) {
            return Ok(Some(mount.clone()));
        }
    }
    Ok(None)
}

fn add_part(trees: &mut HashMap<u64, HashMap<u64, PartId>>, tree: u64, ino: u64, part: PartId) {
    trees.entry(tree).or_default().insert(ino, part);
}

#[derive(Clone, Copy)]
struct RootIdentity {
    uuid: [u8; 16],
    parent: [u8; 16],
    received: [u8; 16],
}

fn root_identity(item: &[u8]) -> io::Result<RootIdentity> {
    Ok(RootIdentity { uuid: bytes(item, ROOT_UUID)?, parent: bytes(item, ROOT_PARENT_UUID)?, received: bytes(item, ROOT_RECEIVED_UUID)? })
}

fn lineage(start: RootIdentity, roots: &HashMap<[u8; 16], RootIdentity>) -> [u8; 16] {
    let mut current = start;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current.uuid) {
            return start.uuid;
        }
        let next = if current.parent != [0; 16] { current.parent } else { current.received };
        if next == [0; 16] {
            return current.uuid;
        }
        match roots.get(&next) {
            Some(root) => current = *root,
            None => return next,
        }
    }
}

const ROOT_TREE: u64 = 1;
const ROOT_INO: u64 = 256;
const INODE_ITEM: u32 = 1;
const INODE_REF: u32 = 12;
const INODE_EXTREF: u32 = 13;
const EXTENT_DATA: u32 = 108;
const ROOT_REF: u32 = 156;
const ROOT_ITEM: u32 = 132;
// Offsets of the UUID fields in the packed on-disk btrfs_root_item.
const ROOT_UUID: usize = 247;
const ROOT_PARENT_UUID: usize = 263;
const ROOT_RECEIVED_UUID: usize = 279;
const FS_TREE: u64 = 5;
const MAX_KEY_TYPE: u32 = u8::MAX as u32;
const SEARCH_BUFFER: usize = 1024 * 1024;
const LOOKUP_BUFFER: usize = 16 * 1024;
const HEADER_SIZE: usize = 32;
const HEADER_OBJECTID: usize = 8;
const HEADER_OFFSET: usize = 16;
const HEADER_TYPE: usize = 24;
const HEADER_LEN: usize = 28;
const INODE_MODE: usize = 52;
const REF_NAME_LEN: usize = 8;
const REF_HEADER: usize = 10;
const EXTREF_PARENT: usize = 0;
const EXTREF_NAME_LEN: usize = 16;
const EXTREF_HEADER: usize = 18;
const EXTENT_GENERATION: usize = 0;
const EXTENT_TYPE: usize = 20;
const EXTENT_INLINE_HEADER: usize = 21;
const EXTENT_BYTENR: usize = 21;
const EXTENT_DISK_BYTES: usize = 29;
const EXTENT_REGULAR_SIZE: usize = 53;
const FILE_EXTENT_INLINE: u8 = 0;
const FILE_EXTENT_REG: u8 = 1;
const FILE_EXTENT_PREALLOC: u8 = 2;
const ROOT_REF_DIRID: usize = 0;

fn invalid() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "invalid btrfs tree search item")
}
fn bytes<const N: usize>(b: &[u8], off: usize) -> io::Result<[u8; N]> {
    b.get(off..off.checked_add(N).ok_or_else(invalid)?).ok_or_else(invalid)?.try_into().map_err(|_| invalid())
}
fn le64(b: &[u8], off: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(bytes(b, off)?))
}
fn le16(b: &[u8], off: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(bytes(b, off)?))
}
fn native64(b: &[u8], off: usize) -> io::Result<u64> {
    Ok(u64::from_ne_bytes(bytes(b, off)?))
}
fn native32(b: &[u8], off: usize) -> io::Result<u32> {
    Ok(u32::from_ne_bytes(bytes(b, off)?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    ino: u64,
    kind: u32,
    offset: u64,
}
fn advance(k: Key) -> Option<Key> {
    if let Some(offset) = k.offset.checked_add(1) {
        Some(Key { offset, ..k })
    } else if k.kind < MAX_KEY_TYPE {
        Some(Key { kind: k.kind + 1, offset: 0, ..k })
    } else {
        k.ino.checked_add(1).map(|ino| Key { ino, kind: 0, offset: 0 })
    }
}
fn header(b: &[u8]) -> io::Result<(Key, usize)> {
    Ok((
        Key { ino: native64(b, HEADER_OBJECTID)?, kind: native32(b, HEADER_TYPE)?, offset: native64(b, HEADER_OFFSET)? },
        native32(b, HEADER_LEN)? as usize,
    ))
}
fn search(
    file: &File,
    scanner: &mut TreeSearch,
    mut key: SearchKey,
    mut f: impl FnMut(Key, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let first_type = key.min_type;
    loop {
        key.nr_items = u32::MAX;
        let (count, buf) = scanner.run(file.as_fd(), &mut key)?;
        if count == 0 {
            return Ok(());
        }
        let mut pos = 0usize;
        let mut last = None;
        for _ in 0..count {
            let h = buf.get(pos..pos + HEADER_SIZE).ok_or_else(invalid)?;
            let (k, len) = header(h)?;
            let start = pos + HEADER_SIZE;
            let end = start.checked_add(len).ok_or_else(invalid)?;
            f(k, buf.get(start..end).ok_or_else(invalid)?)?;
            pos = end;
            last = Some(k);
        }
        let Some(mut next) = last.and_then(advance) else {
            return Ok(());
        };
        if next.kind > key.max_type {
            let Some(ino) = next.ino.checked_add(1) else {
                return Ok(());
            };
            next = Key { ino, kind: first_type, offset: 0 };
        }
        if next.ino > key.max_objectid {
            return Ok(());
        }
        key.min_objectid = next.ino;
        key.min_type = next.kind;
        key.min_offset = next.offset;
    }
}
fn query(tree: u64) -> SearchKey {
    SearchKey {
        tree_id: tree,
        max_objectid: u64::MAX,
        max_offset: u64::MAX,
        max_transid: u64::MAX,
        max_type: MAX_KEY_TYPE,
        ..Default::default()
    }
}
fn parents(key: Key, item: &[u8], out: &mut Vec<u64>) -> io::Result<()> {
    let (hdr, name, parent) = match key.kind {
        INODE_REF => (REF_HEADER, REF_NAME_LEN, false),
        INODE_EXTREF => (EXTREF_HEADER, EXTREF_NAME_LEN, true),
        _ => return Ok(()),
    };
    let mut pos = 0;
    while pos < item.len() {
        let rest = &item[pos..];
        let p = if parent { le64(rest, EXTREF_PARENT)? } else { key.offset };
        let len = le16(rest, name)? as usize;
        pos = pos.checked_add(hdr + len).filter(|&n| n <= item.len()).ok_or_else(invalid)?;
        out.push(p);
    }
    Ok(())
}
fn extent(lineage: [u8; 16], ino: u64, offset: u64, item: &[u8]) -> io::Result<Option<(ExtentKey, u64)>> {
    let ty = *item.get(EXTENT_TYPE).ok_or_else(invalid)?;
    match ty {
        FILE_EXTENT_INLINE => Ok(Some((
            // Snapshot copies retain the origin lineage, inode, offset and item generation.
            ExtentKey::Inline { lineage, ino, offset, generation: le64(item, EXTENT_GENERATION)? },
            item.len().checked_sub(EXTENT_INLINE_HEADER).ok_or_else(invalid)? as u64,
        ))),
        FILE_EXTENT_REG | FILE_EXTENT_PREALLOC => {
            if item.len() < EXTENT_REGULAR_SIZE {
                return Err(invalid());
            }
            let disk = le64(item, EXTENT_BYTENR)?;
            Ok((disk != 0).then_some((ExtentKey::Disk(disk), le64(item, EXTENT_DISK_BYTES)?)))
        }
        _ => Err(invalid()),
    }
}

#[derive(Default)]
struct Refs {
    first: HashMap<u64, u64>,
    extra: HashMap<u64, Vec<u64>>,
}
impl Refs {
    fn add(&mut self, ino: u64, parent: u64) {
        match self.first.entry(ino) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(parent);
            }
            std::collections::hash_map::Entry::Occupied(e) if *e.get() != parent => {
                self.extra.entry(ino).or_default().push(parent);
            }
            _ => {}
        }
    }
}

// Directories have one parent. The sentinel prevents loops on corrupt metadata.
fn directory_part(
    ino: u64,
    parts: &HashMap<u64, PartId>,
    refs: &Refs,
    memo: &mut HashMap<u64, Option<PartId>>,
) -> Option<PartId> {
    if let Some(&p) = parts.get(&ino) {
        return Some(p);
    }
    if let Some(&p) = memo.get(&ino) {
        return p;
    }
    memo.insert(ino, None);
    let result = refs.first.get(&ino).and_then(|&p| directory_part(p, parts, refs, memo));
    memo.insert(ino, result);
    result
}

fn file_parts(
    ino: u64,
    parts: &HashMap<u64, PartId>,
    refs: &Refs,
    memo: &mut HashMap<u64, Option<PartId>>,
    out: &mut Vec<PartId>,
) {
    out.clear();
    for &parent in refs.first.get(&ino).into_iter().chain(refs.extra.get(&ino).into_iter().flatten()) {
        if let Some(part) = directory_part(parent, parts, refs, memo) {
            if !out.contains(&part) {
                out.push(part);
            }
        }
    }
    out.sort_unstable();
}

#[derive(Default)]
struct FileGroups {
    current: Option<u64>,
    scratch: Vec<(ExtentKey, u64)>,
    extents: Vec<(ExtentKey, u64)>,
    files: Vec<(u64, std::ops::Range<usize>)>,
}
impl FileGroups {
    fn finish(&mut self) {
        if let Some(ino) = self.current.take() {
            self.scratch.sort_unstable_by_key(|&(key, _)| key);
            let start = self.extents.len();
            for (key, size) in self.scratch.drain(..) {
                if self.extents.len() > start && self.extents.last().is_some_and(|last| last.0 == key) {
                    let last = self.extents.last_mut().unwrap();
                    last.1 = last.1.max(size);
                } else {
                    self.extents.push((key, size));
                }
            }
            self.files.push((ino, start..self.extents.len()));
        }
    }
    fn start(&mut self, ino: u64) {
        self.finish();
        self.current = Some(ino);
    }
}

fn lookup_part(
    file: &File,
    tree: u64,
    ino: u64,
    scanner: &mut TreeSearch,
    parts: &HashMap<u64, PartId>,
    memo: &mut HashMap<u64, Option<PartId>>,
) -> io::Result<Option<PartId>> {
    if let Some(&part) = parts.get(&ino) {
        return Ok(Some(part));
    }
    if let Some(&part) = memo.get(&ino) {
        return Ok(part);
    }
    if ino == ROOT_INO {
        return Ok(None);
    }
    // Directory hard links are prohibited by btrfs; one parent is enough.
    let mut query = query(tree);
    query.min_objectid = ino;
    query.max_objectid = ino;
    query.min_type = INODE_REF;
    query.max_type = INODE_EXTREF;
    let mut refs = Vec::new();
    search(file, scanner, query, |k, v| parents(k, v, &mut refs))?;
    // Insert a sentinel first to guard corrupt parent cycles.
    memo.insert(ino, None);
    let mut part = None;
    for parent in refs {
        if let Some(p) = lookup_part(file, tree, parent, scanner, parts, memo)? {
            part = Some(p);
            break;
        }
    }
    memo.insert(ino, part);
    Ok(part)
}

/// Plan one unit for each reachable subvolume. Other filesystems are returned to the caller.
pub fn plan(store: &Path, parts: &[PathBuf]) -> io::Result<Plan> {
    let root = File::open(store)?;
    let fsid = sys::filesystem_uuid(root.as_fd())?;
    let mut trees: HashMap<u64, HashMap<u64, PartId>> = HashMap::new();
    let mut outside = Vec::new();
    for (part, path) in parts.iter().enumerate() {
        // Trash or a line can disappear after the caller lists the parts; it is then empty.
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        match sys::filesystem_uuid(file.as_fd()) {
            Ok(id) if id == fsid => {}
            Ok(_) => {
                outside.push(part);
                continue;
            }
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTTY | libc::EINVAL)) => {
                outside.push(part);
                continue;
            }
            Err(e) => return Err(e),
        }
        let tree = sys::subvol_id(file.as_fd())?;
        add_part(&mut trees, tree, metadata.ino(), part);
    }
    let mut children: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();
    let mut identities = HashMap::new();
    let mut q = query(ROOT_TREE);
    q.min_type = ROOT_ITEM;
    q.max_type = ROOT_REF;
    search(&root, &mut TreeSearch::new(SEARCH_BUFFER), q, |k, item| {
        if k.kind == ROOT_REF {
            children.entry(k.ino).or_default().push((k.offset, le64(item, ROOT_REF_DIRID)?));
        } else if k.kind == ROOT_ITEM {
            if let Ok(identity) = root_identity(item) {
                identities.insert(k.ino, identity);
            }
        }
        Ok(())
    })?;
    // Iterate until no new root is reached; a unit explicitly named by a part already wins at
    // its root, while ordinary nested part folders still win during ancestry resolution.
    let mut scanner = TreeSearch::new(LOOKUP_BUFFER);
    let mut pending: Vec<_> = trees.keys().copied().collect();
    while let Some(tree) = pending.pop() {
        let mut memo = HashMap::new();
        for &(child, dir) in children.get(&tree).into_iter().flatten() {
            if let Some(part) = lookup_part(&root, tree, dir, &mut scanner, &trees[&tree], &mut memo)? {
                if let std::collections::hash_map::Entry::Vacant(entry) = trees.entry(child).or_default().entry(ROOT_INO) {
                    entry.insert(part);
                    pending.push(child);
                }
            }
        }
    }
    let known: HashMap<_, _> = identities.values().filter(|id| id.uuid != [0; 16]).map(|id| (id.uuid, *id)).collect();
    let mut units = Vec::new();
    for (tree, parts) in trees {
        let origin = match identities.get(&tree) {
            Some(id) if id.uuid != [0; 16] => lineage(*id, &known),
            _ if tree == FS_TREE => fsid,
            _ => return Err(invalid()),
        };
        units.push(TreeUnit { root: store.to_path_buf(), tree, parts, lineage: origin });
    }
    Ok(Plan { units, outside })
}

/// Visit each regular inode once for each part that contains one of its hard links.
pub fn visit(unit: &TreeUnit, f: &mut dyn FnMut(PartId, &Held)) -> io::Result<()> {
    let file = File::open(&unit.root)?;
    let mut refs = Refs::default();
    let mut groups = FileGroups::default();
    let mut parent_scratch = Vec::new();
    search(&file, &mut TreeSearch::new(SEARCH_BUFFER), query(unit.tree), |k, item| {
        if groups.current.is_some_and(|ino| ino != k.ino) {
            groups.finish();
        }
        match k.kind {
            INODE_ITEM => {
                let mode = u32::from_le_bytes(bytes(item, INODE_MODE)?);
                if mode & libc::S_IFMT == libc::S_IFREG {
                    groups.start(k.ino);
                }
            }
            INODE_REF | INODE_EXTREF => {
                parent_scratch.clear();
                parents(k, item, &mut parent_scratch)?;
                for &parent in &parent_scratch {
                    refs.add(k.ino, parent);
                }
            }
            EXTENT_DATA if groups.current == Some(k.ino) => {
                if let Some(ext) = extent(unit.lineage, k.ino, k.offset, item)? {
                    groups.scratch.push(ext);
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    groups.finish();
    let mut memo = HashMap::new();
    let mut parts = Vec::new();
    for (ino, range) in groups.files {
        let shared = &groups.extents[range];
        let alloc = shared.iter().map(|x| x.1).sum();
        let held = Held { alloc, own: 0, exclusive: 0, shared };
        file_parts(ino, &unit.parts, &refs, &mut memo, &mut parts);
        for &part in &parts {
            f(part, &held);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_follows_snapshots_and_received_origins_and_rejects_cycles() {
        let root = RootIdentity { uuid: [1; 16], parent: [0; 16], received: [0; 16] };
        let child = RootIdentity { uuid: [2; 16], parent: root.uuid, received: [0; 16] };
        let leaf = RootIdentity { uuid: [3; 16], parent: child.uuid, received: [0; 16] };
        let received = RootIdentity { uuid: [4; 16], parent: [0; 16], received: root.uuid };
        let roots = [root, child, leaf, received].into_iter().map(|r| (r.uuid, r)).collect();
        assert_eq!(lineage(leaf, &roots), root.uuid);
        assert_eq!(lineage(received, &roots), root.uuid);
        let missing = RootIdentity { uuid: [5; 16], parent: [9; 16], received: [0; 16] };
        assert_eq!(lineage(missing, &roots), [9; 16]);
        let cycle = RootIdentity { uuid: [6; 16], parent: [7; 16], received: [0; 16] };
        let other = RootIdentity { uuid: [7; 16], parent: cycle.uuid, received: [0; 16] };
        let cycle_roots = [cycle, other].into_iter().map(|r| (r.uuid, r)).collect();
        assert_eq!(lineage(cycle, &cycle_roots), cycle.uuid);
    }

    #[test]
    fn root_item_uuid_offsets_are_checked() {
        let mut item = vec![0; ROOT_RECEIVED_UUID + 16];
        item[ROOT_UUID..ROOT_UUID + 16].fill(1);
        item[ROOT_PARENT_UUID..ROOT_PARENT_UUID + 16].fill(2);
        item[ROOT_RECEIVED_UUID..ROOT_RECEIVED_UUID + 16].fill(3);
        let root = root_identity(&item).unwrap();
        assert_eq!((root.uuid, root.parent, root.received), ([1; 16], [2; 16], [3; 16]));
        assert!(root_identity(&item[..item.len() - 1]).is_err());
    }

    #[test]
    fn parses_mountinfo_escapes() {
        let mounts = mount_points(b"1 0 8:1 / / rw - ext4 /dev/sda rw\n2 1 8:2 / /store/a\\040b rw - tmpfs tmpfs rw\n").unwrap();
        assert_eq!(mounts, [PathBuf::from("/"), PathBuf::from("/store/a b")]);
        assert!(mount_points(b"1 0 8:1 / /bad\\99 rw - x x rw\n").is_err());
    }

    #[test]
    fn grouped_extents_deduplicate_per_inode() {
        let mut groups = FileGroups::default();
        groups.start(10);
        groups.scratch.extend([(ExtentKey::Disk(100), 4), (ExtentKey::Disk(100), 8)]);
        groups.start(11);
        groups.scratch.push((ExtentKey::Disk(100), 8));
        groups.finish();
        assert_eq!(groups.files, [(10, 0..1), (11, 1..2)]);
        assert_eq!(groups.extents, [(ExtentKey::Disk(100), 8), (ExtentKey::Disk(100), 8)]);
    }

    #[test]
    fn grouping_parts_by_tree() {
        let mut trees = HashMap::new();
        add_part(&mut trees, 8, 300, 0);
        add_part(&mut trees, 8, 301, 1);
        add_part(&mut trees, 9, ROOT_INO, 2);
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[&8], HashMap::from([(300, 0), (301, 1)]));
        assert_eq!(trees[&9], HashMap::from([(ROOT_INO, 2)]));
    }

    #[test]
    fn headers_and_advance() {
        let mut h = [0; HEADER_SIZE];
        h[HEADER_OBJECTID..HEADER_OBJECTID + 8].copy_from_slice(&17u64.to_ne_bytes());
        h[HEADER_OFFSET..HEADER_OFFSET + 8].copy_from_slice(&24u64.to_ne_bytes());
        h[HEADER_TYPE..HEADER_TYPE + 4].copy_from_slice(&INODE_REF.to_ne_bytes());
        h[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&9u32.to_ne_bytes());
        let (k, len) = header(&h).unwrap();
        assert_eq!((k, len), (Key { ino: 17, kind: INODE_REF, offset: 24 }, 9));
        assert!(header(&h[..HEADER_SIZE - 1]).is_err());
        assert_eq!(advance(Key { ino: 1, kind: 2, offset: u64::MAX }), Some(Key { ino: 1, kind: 3, offset: 0 }));
        assert_eq!(advance(Key { ino: 1, kind: MAX_KEY_TYPE, offset: u64::MAX }), Some(Key { ino: 2, kind: 0, offset: 0 }));
        assert_eq!(advance(Key { ino: u64::MAX, kind: MAX_KEY_TYPE, offset: u64::MAX }), None);
    }

    #[test]
    fn extent_fixtures() {
        let mut data = vec![0; EXTENT_INLINE_HEADER + 7];
        data[..8].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(extent([1; 16], 8, 9, &data).unwrap(), Some((ExtentKey::Inline { lineage: [1; 16], ino: 8, offset: 9, generation: 42 }, 7)));
        data.resize(EXTENT_REGULAR_SIZE, 0);
        data[EXTENT_TYPE] = FILE_EXTENT_REG;
        data[EXTENT_BYTENR..EXTENT_BYTENR + 8].copy_from_slice(&4096u64.to_le_bytes());
        data[EXTENT_DISK_BYTES..EXTENT_DISK_BYTES + 8].copy_from_slice(&8192u64.to_le_bytes());
        assert_eq!(extent([1; 16], 1, 0, &data).unwrap(), Some((ExtentKey::Disk(4096), 8192)));
        data[EXTENT_TYPE] = FILE_EXTENT_PREALLOC;
        assert_eq!(extent([1; 16], 1, 0, &data).unwrap(), Some((ExtentKey::Disk(4096), 8192)));
        data[EXTENT_BYTENR..EXTENT_BYTENR + 8].fill(0);
        assert_eq!(extent([1; 16], 1, 0, &data).unwrap(), None);
        assert!(extent([1; 16], 1, 0, &data[..EXTENT_REGULAR_SIZE - 1]).is_err());
    }

    #[test]
    fn refs_and_attribution() {
        let mut item = vec![0; EXTREF_HEADER + 1];
        item[..8].copy_from_slice(&20u64.to_le_bytes());
        item[EXTREF_NAME_LEN..EXTREF_NAME_LEN + 2].copy_from_slice(&1u16.to_le_bytes());
        let mut p = Vec::new();
        parents(Key { ino: 9, kind: INODE_EXTREF, offset: 0 }, &item, &mut p).unwrap();
        assert_eq!(p, [20]);
        assert!(parents(Key { ino: 9, kind: INODE_EXTREF, offset: 0 }, &item[..EXTREF_HEADER], &mut Vec::new()).is_err());
        let parts = HashMap::from([(10, 0), (20, 1)]);
        let mut refs = Refs::default();
        for (ino, parent) in [(30, 10), (40, 30), (40, 20), (40, 20)] {
            refs.add(ino, parent);
        }
        let mut out = Vec::new();
        file_parts(40, &parts, &refs, &mut HashMap::new(), &mut out);
        assert_eq!(out, [0, 1]);
        let nested = HashMap::from([(10, 0), (30, 1)]);
        file_parts(40, &nested, &refs, &mut HashMap::new(), &mut out);
        assert_eq!(out, [1]);
    }
}
