//! Bulk accounting uses whole btrfs disk extents, including compressed and partially referenced
//! extents. Repeated references in one file count once. Inline items count their stored payload;
//! holes do not count. These keys describe the btrfs allocation, not FIEMAP's merged ranges.
//! Each tree is read once with TREE_SEARCH_V2, which requires CAP_SYS_ADMIN.
//! The bulk path reports committed on-disk allocation; very recent writes may not appear yet.
//! Identical inline bytes independently written at the same inode and offset in two subvolumes within one transaction count once.

use crate::fsutil::{ExtentKey, Held};
use crate::sys::{self, SearchKey, TreeSearch};
use layr_hashers::{FastCryptographic, PurposeHasher};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use zerocopy::byteorder::{little_endian as le, native_endian as ne};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

pub type PartId = usize;
pub struct TreeUnit {
    pub root: PathBuf,
    pub parts: HashMap<u64, PartId>,
    tree: u64,
}
pub struct Plan {
    pub units: Vec<TreeUnit>,
    pub outside: Vec<PartId>,
}

fn mount_points(data: &[u8]) -> io::Result<Vec<PathBuf>> {
    sys::mountinfo_points(data)
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

const ROOT_TREE: u64 = 1;
const ROOT_INO: u64 = 256;
const INODE_ITEM: u32 = 1;
const INODE_REF: u32 = 12;
const INODE_EXTREF: u32 = 13;
const EXTENT_DATA: u32 = 108;
const ROOT_REF: u32 = 156;
const MAX_KEY_TYPE: u32 = u8::MAX as u32;
const SEARCH_BUFFER: usize = 1024 * 1024;
const LOOKUP_BUFFER: usize = 16 * 1024;
#[cfg(test)]
const HEADER_SIZE: usize = 32;
#[cfg(test)]
const HEADER_OBJECTID: usize = 8;
#[cfg(test)]
const HEADER_OFFSET: usize = 16;
#[cfg(test)]
const HEADER_TYPE: usize = 24;
#[cfg(test)]
const HEADER_LEN: usize = 28;
#[cfg(test)]
const EXTREF_NAME_LEN: usize = 16;
#[cfg(test)]
const EXTREF_HEADER: usize = 18;
#[cfg(test)]
const EXTENT_TYPE: usize = 20;
#[cfg(test)]
const EXTENT_INLINE_HEADER: usize = 21;
#[cfg(test)]
const EXTENT_BYTENR: usize = 21;
#[cfg(test)]
const EXTENT_DISK_BYTES: usize = 29;
#[cfg(test)]
const EXTENT_REGULAR_SIZE: usize = 53;
const FILE_EXTENT_INLINE: u8 = 0;
const FILE_EXTENT_REG: u8 = 1;
const FILE_EXTENT_PREALLOC: u8 = 2;

fn invalid() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "invalid btrfs tree search item")
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct SearchHeader {
    transid: ne::U64,
    objectid: ne::U64,
    offset: ne::U64,
    kind: ne::U32,
    len: ne::U32,
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct InodeItem {
    before_mode: [u8; 52],
    mode: le::U32,
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct InodeRef {
    index: le::U64,
    name_len: le::U16,
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct InodeExtref {
    parent: le::U64,
    index: le::U64,
    name_len: le::U16,
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct RootRef {
    dirid: le::U64,
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct ExtentPrefix {
    generation: le::U64,
    ram_bytes: le::U64,
    compression: u8,
    encryption: u8,
    other_encoding: le::U16,
    kind: u8,
}
#[repr(C)]
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
struct RegularExtent {
    prefix: ExtentPrefix,
    disk_bytenr: le::U64,
    disk_num_bytes: le::U64,
    offset: le::U64,
    num_bytes: le::U64,
}

fn parse<T: FromBytes + KnownLayout + Immutable>(bytes: &[u8]) -> io::Result<&T> {
    T::ref_from_prefix(bytes).map(|(value, _)| value).map_err(|_| invalid())
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
    let h: &SearchHeader = parse(b)?;
    Ok((Key { ino: h.objectid.get(), kind: h.kind.get(), offset: h.offset.get() }, h.len.get() as usize))
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
            let h = buf.get(pos..pos + std::mem::size_of::<SearchHeader>()).ok_or_else(invalid)?;
            let (k, len) = header(h)?;
            let start = pos + std::mem::size_of::<SearchHeader>();
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
    let mut pos = 0;
    while pos < item.len() {
        let rest = &item[pos..];
        let (hdr, len, parent) = match key.kind {
            INODE_REF => {
                let r: &InodeRef = parse(rest)?;
                (std::mem::size_of::<InodeRef>(), r.name_len.get() as usize, key.offset)
            }
            INODE_EXTREF => {
                let r: &InodeExtref = parse(rest)?;
                (std::mem::size_of::<InodeExtref>(), r.name_len.get() as usize, r.parent.get())
            }
            _ => return Ok(()),
        };
        pos = pos.checked_add(hdr + len).filter(|&n| n <= item.len()).ok_or_else(invalid)?;
        out.push(parent);
    }
    Ok(())
}
fn extent(ino: u64, offset: u64, item: &[u8]) -> io::Result<Option<(ExtentKey, u64)>> {
    let prefix: &ExtentPrefix = parse(item)?;
    match prefix.kind {
        FILE_EXTENT_INLINE => {
            let payload = &item[std::mem::size_of::<ExtentPrefix>()..];
            let hash = FastCryptographic::hash(payload);
            Ok(Some((
                ExtentKey::Inline {
                    ino,
                    offset,
                    generation: prefix.generation.get(),
                    payload: u64::from_le_bytes(hash[..8].try_into().map_err(|_| invalid())?),
                },
                payload.len() as u64,
            )))
        }
        FILE_EXTENT_REG | FILE_EXTENT_PREALLOC => {
            let extent: &RegularExtent = parse(item)?;
            let disk = extent.disk_bytenr.get();
            Ok((disk != 0).then_some((ExtentKey::Disk(disk), extent.disk_num_bytes.get())))
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
    let mut q = query(ROOT_TREE);
    q.min_type = ROOT_REF;
    q.max_type = ROOT_REF;
    search(&root, &mut TreeSearch::new(SEARCH_BUFFER), q, |k, item| {
        if k.kind == ROOT_REF {
            children.entry(k.ino).or_default().push((k.offset, parse::<RootRef>(item)?.dirid.get()));
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
    Ok(Plan {
        units: trees.into_iter().map(|(tree, parts)| TreeUnit { root: store.to_path_buf(), tree, parts }).collect(),
        outside,
    })
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
                let mode = parse::<InodeItem>(item)?.mode.get();
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
                if let Some(ext) = extent(k.ino, k.offset, item)? {
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
    #[test]
    fn uapi_layouts() {
        use super::*;
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<SearchHeader>(), 32);
        assert_eq!(offset_of!(SearchHeader, objectid), 8);
        assert_eq!(offset_of!(SearchHeader, offset), 16);
        assert_eq!(offset_of!(SearchHeader, kind), 24);
        assert_eq!(offset_of!(SearchHeader, len), 28);
        assert_eq!(size_of::<InodeItem>(), 56);
        assert_eq!(offset_of!(InodeItem, mode), 52);
        assert_eq!(size_of::<InodeRef>(), 10);
        assert_eq!(offset_of!(InodeRef, name_len), 8);
        assert_eq!(size_of::<InodeExtref>(), 18);
        assert_eq!(offset_of!(InodeExtref, parent), 0);
        assert_eq!(offset_of!(InodeExtref, name_len), 16);
        assert_eq!(size_of::<RootRef>(), 8);
        assert_eq!(offset_of!(RootRef, dirid), 0);
        assert_eq!(size_of::<ExtentPrefix>(), 21);
        assert_eq!(offset_of!(ExtentPrefix, generation), 0);
        assert_eq!(offset_of!(ExtentPrefix, kind), 20);
        assert_eq!(size_of::<RegularExtent>(), 53);
        assert_eq!(offset_of!(RegularExtent, disk_bytenr), 21);
        assert_eq!(offset_of!(RegularExtent, disk_num_bytes), 29);
    }
    use super::*;

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
        let first = extent(8, 9, &data).unwrap();
        assert!(matches!(first, Some((ExtentKey::Inline { ino: 8, offset: 9, generation: 42, .. }, 7))));
        data[EXTENT_INLINE_HEADER] = 1;
        assert_ne!(first, extent(8, 9, &data).unwrap());
        data.resize(EXTENT_REGULAR_SIZE, 0);
        data[EXTENT_TYPE] = FILE_EXTENT_REG;
        data[EXTENT_BYTENR..EXTENT_BYTENR + 8].copy_from_slice(&4096u64.to_le_bytes());
        data[EXTENT_DISK_BYTES..EXTENT_DISK_BYTES + 8].copy_from_slice(&8192u64.to_le_bytes());
        assert_eq!(extent(1, 0, &data).unwrap(), Some((ExtentKey::Disk(4096), 8192)));
        data[EXTENT_TYPE] = FILE_EXTENT_PREALLOC;
        assert_eq!(extent(1, 0, &data).unwrap(), Some((ExtentKey::Disk(4096), 8192)));
        data[EXTENT_BYTENR..EXTENT_BYTENR + 8].fill(0);
        assert_eq!(extent(1, 0, &data).unwrap(), None);
        assert!(extent(1, 0, &data[..EXTENT_REGULAR_SIZE - 1]).is_err());
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
