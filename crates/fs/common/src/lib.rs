//! Shared full-copy and full-enumeration fallbacks. No filesystem identity decisions.
#![forbid(unsafe_code)]
use layr_fs_core::{
    evidence::{DataEvidence, EntryChange, FType, Meta},
    *,
};
use std::collections::{BTreeMap, HashMap};

pub fn required(tree: &dyn TreeRead, path: &RelDir) -> Result<EntryMetadata> {
    tree.metadata(path)?.ok_or(FsError::Invalid("entry disappeared during observation"))
}

pub fn copy_tree(source: &dyn TreeRead, target: &dyn TreeWrite) -> Result<()> {
    fn folder(
        source: &dyn TreeRead,
        target: &dyn TreeWrite,
        dir: &RelDir,
        links: &mut HashMap<(u64, u64), RelPath>,
    ) -> Result<()> {
        source.entries(dir, &mut |name| {
            if !name.is_name() {
                return Err(FsError::Invalid("reader returned a compound child name"));
            }
            let path = dir.join(name);
            let at = RelDir::Path(path.clone());
            let mut meta = required(source, &at)?;
            // A nonpersistent source timestamp has no physical time to preserve. Use a
            // fixed epoch for copies onto filesystems that must assign every entry a time.
            if meta.attributes.stored_time.is_none() {
                meta.attributes.modified_ns = 0;
            }
            match meta.kind {
                FType::Dir => {
                    let mut temporary = meta.attributes.clone();
                    temporary.mode |= 0o700;
                    target.put(&path, EntryData::Directory, &temporary)?;
                    folder(source, target, &at, links)?;
                }
                FType::File => {
                    let identity = meta.link.filter(|link| link.count > 1).map(|link| (link.device, link.inode));
                    if let Some(first) = identity.and_then(|key| links.get(&key)) {
                        target.put(&path, EntryData::HardLink(first), &meta.attributes)?;
                    } else {
                        let mut file = source.open_file(&path)?;
                        target.put(&path, EntryData::File(&mut file), &meta.attributes)?;
                        if let Some(key) = identity {
                            links.insert(key, path.clone());
                        }
                    }
                }
                FType::Symlink => target.put(&path, EntryData::Symlink(&source.read_link(&path)?), &meta.attributes)?,
                FType::Special => return Err(FsError::Invalid("cannot materialise a special file")),
            }
            Ok(())
        })?;
        let mut attributes = required(source, dir)?.attributes;
        if attributes.stored_time.is_none() {
            attributes.modified_ns = 0;
        }
        target.set_metadata(dir, &attributes)
    }
    folder(source, target, &RelDir::Root, &mut HashMap::new())
}

pub fn walk(tree: &dyn TreeRead, dir: &RelDir, visit: &mut dyn FnMut(&RelPath, &EntryMetadata) -> Result<bool>) -> Result<()> {
    tree.entries(dir, &mut |name| {
        if !name.is_name() {
            return Err(FsError::Invalid("reader returned a compound child name"));
        }
        let path = dir.join(name);
        let at = RelDir::Path(path.clone());
        let meta = required(tree, &at)?;
        if visit(&path, &meta)? && meta.kind == FType::Dir {
            walk(tree, &at, visit)?;
        }
        Ok(())
    })
}

/// Compatibility conversion is explicit: legacy comparison paths remain UTF-8.
pub fn observation(metadata: &EntryMetadata) -> Meta {
    let a = &metadata.attributes;
    Meta {
        ftype: metadata.kind,
        mode: a.mode,
        size: metadata.size,
        ino: metadata.link.map_or(0, |l| l.inode),
        uid: a.uid,
        gid: a.gid,
        mtime: a.modified_ns.div_euclid(1_000_000_000),
    }
}

/// All entries are candidates, including time-, attribute- and link-only changes. Confirmation
/// stays in the application comparison kernel; full enumeration never claims byte equality.
pub fn candidates(before: &dyn TreeRead, after: &dyn TreeRead) -> Result<Vec<EntryChange>> {
    let mut paths = BTreeMap::<String, (Option<Meta>, Option<Meta>)>::new();
    walk(before, &RelDir::Root, &mut |path, meta| {
        paths.entry(path.utf8()?.to_owned()).or_default().0 = Some(observation(meta));
        Ok(true)
    })?;
    walk(after, &RelDir::Root, &mut |path, meta| {
        paths.entry(path.utf8()?.to_owned()).or_default().1 = Some(observation(meta));
        Ok(true)
    })?;
    Ok(paths.into_iter().filter_map(|(path, (old, new))| EntryChange::observed(path, old, new, DataEvidence::Unknown)).collect())
}

/// One capability decision for every backend. Errors remain distinct from unavailable evidence.
pub fn changes(fs: &dyn Filesystem, before: &FrozenTree, after: &FrozenTree) -> Result<Vec<EntryChange>> {
    if let Some(hints) = fs.capabilities(&before.scope()).change_hints {
        match hints.between(before, after)? {
            hints::HintSet::Complete(entries) => return Ok(entries),
            hints::HintSet::Partial => {}
        }
    }
    let mut entries = candidates(before.reader(), after.reader())?;
    for entry in &mut entries {
        if let (Some(old), Some(new)) = (entry.old(), entry.new()) {
            if old.is_file() && new.is_file() {
                let mut a = before.reader().open_file(&RelPath::new(entry.src().as_bytes())?)?;
                let mut b = after.reader().open_file(&RelPath::new(entry.path().as_bytes())?)?;
                let ranges = changed_ranges(&mut a, old.size, &mut b, new.size)?;
                entry.set_data(DataEvidence::from_native(&ranges, old.size, new.size).map_err(FsError::Invalid)?);
            }
        }
    }
    Ok(entries)
}

/// Shared clone-to-new dispatch; an unavailable acceleration leaves the full fallback safe.
pub fn copy_file(
    fs: &dyn Filesystem,
    source: &FrozenTree,
    path: &RelPath,
    target: &WorkingTree,
    destination: &RelPath,
) -> Result<()> {
    if source.scope().same_instance(&target.scope()) {
        if let Some(clone) = fs.capabilities(&target.scope()).clone_new {
            if let Attempt::Applied(()) = clone.clone_new(source, path, target, destination)? {
                return Ok(());
            }
        }
    }
    let metadata = required(source.reader(), &RelDir::Path(path.clone()))?;
    if metadata.kind != FType::File {
        return Err(FsError::Invalid("clone source is not a file"));
    }
    let mut input = source.reader().open_file(path)?;
    target.writer().put(destination, EntryData::File(&mut input), &metadata.attributes)
}

pub fn copy_range(
    fs: &dyn Filesystem,
    scope: &TreeScope,
    source: &std::fs::File,
    range: ByteRange,
    target: &std::fs::File,
    offset: u64,
) -> Result<()> {
    use std::os::unix::fs::{FileExt, MetadataExt};
    offset.checked_add(range.length()).ok_or(FsError::Invalid("target range overflow"))?;
    if let Some(reflink) = fs.capabilities(scope).reflink_range {
        if let Attempt::Applied(()) = reflink.clone_range(source, range, target, offset)? {
            return Ok(());
        }
    }
    let (a, b) = (source.metadata()?, target.metadata()?);
    let reverse = a.dev() == b.dev() && a.ino() == b.ino() && offset > range.offset() && offset < range.offset() + range.length();
    let mut buffer = [0; 65536];
    let mut copied = 0;
    while copied < range.length() {
        let length = (range.length() - copied).min(buffer.len() as u64);
        let at = if reverse { range.length() - copied - length } else { copied };
        let bytes = &mut buffer[..usize::try_from(length).map_err(|_| FsError::Invalid("range buffer overflow"))?];
        source.read_exact_at(bytes, range.offset() + at)?;
        target.write_all_at(bytes, offset + at)?;
        copied += length;
    }
    Ok(())
}

/// Shared block evidence for immutable reads. Byte comparison is independent of the driver.
pub fn changed_ranges(a: &mut dyn std::io::Read, alen: u64, b: &mut dyn std::io::Read, blen: u64) -> Result<Vec<(u64, u64)>> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let (mut ab, mut bb) = ([0; 4096], [0; 4096]);
    let mut pos = 0;
    while pos < alen.max(blen) {
        let an = usize::try_from(alen.saturating_sub(pos).min(4096)).map_err(|_| FsError::Invalid("block length overflow"))?;
        let bn = usize::try_from(blen.saturating_sub(pos).min(4096)).map_err(|_| FsError::Invalid("block length overflow"))?;
        a.read_exact(&mut ab[..an])?;
        b.read_exact(&mut bb[..bn])?;
        let len = an.max(bn) as u64;
        if ab[..an] != bb[..bn] {
            match ranges.last_mut() {
                Some((start, size)) if *start + *size == pos => *size += len,
                _ => ranges.push((pos, len)),
            }
        }
        pos += len;
    }
    Ok(ranges)
}
