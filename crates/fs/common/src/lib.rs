//! Shared full-copy and full-enumeration fallbacks. No filesystem identity decisions.
#![forbid(unsafe_code)]
mod layered;
pub use layered::{LayeredTree, ReadLayer};
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
                    temporary.acl = None;
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
pub fn changes(
    fs: &dyn Filesystem,
    before: &FrozenTree,
    after: &FrozenTree,
    request: layr_fs_core::ChangeRequest,
) -> Result<Vec<EntryChange>> {
    if let Some(hints) = fs.capabilities(&before.scope()).change_hints {
        match hints.between(before, after, request)? {
            hints::HintSet::Complete(entries) => return Ok(entries),
            hints::HintSet::Partial => {}
        }
    }
    let mut entries = candidates(before.reader(), after.reader())?;
    if request.detail == ChangeDetail::Equality {
        return Ok(entries);
    }
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
            if let Attempt::Applied(()) = clone.clone_new(CloneSource::Frozen(source), path, target, destination)? {
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

/// Export selected leaves for an external process. Namespace and metadata reads stay on the
/// logical reader; directories are opened for writing until their children are complete.
pub fn copy_selected(
    source: &dyn TreeRead,
    target: &dyn TreeWrite,
    selected: &[RelPath],
    mut clone: Option<&mut dyn FnMut(&RelPath) -> Result<Attempt<()>>>,
) -> Result<()> {
    use std::collections::BTreeSet;
    let mut directories = BTreeSet::new();
    let mut files = Vec::new();
    for path in selected {
        let Some(metadata) = source.metadata(&RelDir::Path(path.clone()))? else { continue };
        for (end, byte) in path.as_bytes().iter().enumerate() {
            if *byte == b'/' {
                directories.insert(RelPath::new(&path.as_bytes()[..end])?);
            }
        }
        if metadata.kind == FType::Dir {
            directories.insert(path.clone());
        } else {
            files.push((path.clone(), metadata));
        }
    }
    for path in &directories {
        let mut attributes = required(source, &RelDir::Path(path.clone()))?.attributes;
        attributes.mode |= 0o700;
        attributes.acl = None;
        target.put(path, EntryData::Directory, &attributes)?;
    }
    let mut links = HashMap::new();
    for (path, metadata) in files {
        match metadata.kind {
            FType::File => {
                let link = metadata.link.filter(|link| link.count > 1).map(|link| (link.device, link.inode));
                if let Some(first) = link.and_then(|key| links.get(&key)) {
                    target.put(&path, EntryData::HardLink(first), &metadata.attributes)?;
                } else {
                    let cloned = match clone.as_deref_mut() {
                        Some(clone) => matches!(clone(&path)?, Attempt::Applied(())),
                        None => false,
                    };
                    if !cloned {
                        target.put(&path, EntryData::File(&mut source.open_file(&path)?), &metadata.attributes)?;
                    }
                    if let Some(key) = link {
                        links.insert(key, path);
                    }
                }
            }
            FType::Symlink => target.put(&path, EntryData::Symlink(&source.read_link(&path)?), &metadata.attributes)?,
            _ => return Err(FsError::Invalid("cannot export a special entry")),
        }
    }
    for directory in directories.into_iter().rev() {
        let at = RelDir::Path(directory);
        target.set_metadata(&at, &required(source, &at)?.attributes)?;
    }
    Ok(())
}

/// Publish an already frozen capture through a reader copy when a driver cannot move it.
/// Consistency belongs to the original capture, and failed cleanup retains both receipts.
pub fn republish(fs: &dyn Filesystem, issuer: &TreeIssuer, capture: Capture, dst: NewTreeLocation) -> Result<Capture> {
    let checked = (|| -> Result<()> {
        issuer.location_key(&dst)?;
        if issuer.frozen_key(&capture.tree)? != issuer.owned_key(&capture.ownership)? {
            return Err(FsError::Invalid("capture ownership mismatch"));
        }
        Ok(())
    })();
    let result = checked.and_then(|()| fs.freeze(FreezeSource::Reader(capture.tree.reader()), dst));
    let Capture { tree, ownership, consistency } = capture;
    drop(tree);
    if let Err(failure) = fs.remove(ownership) {
        let cause = match result {
            Err(cause) => cause,
            Ok(published) => {
                drop(published.tree);
                match fs.remove(published.ownership) {
                    Ok(()) => FsError::Invalid("captured source cleanup failed"),
                    Err(next) => FsError::Cleanup {
                        cause: Box::new(FsError::Invalid("published capture cleanup failed")),
                        failure: Box::new(next),
                    },
                }
            }
        };
        return Err(FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) });
    }
    result.map(|mut published| {
        published.consistency = consistency;
        published
    })
}

/// A writable copy of a capture is distinct from materialising a live reader. Drivers may
/// accelerate this exact operation; the fallback freezes first and preserves its consistency.
pub fn copy_capture(
    fs: &dyn Filesystem,
    source: &WorkingTree,
    destination: NewTreeLocation,
    scratch: &mut dyn FnMut() -> Result<NewTreeLocation>,
) -> Result<WritableCapture> {
    if let Some(copy) = fs.capabilities(&source.scope()).capture_copy {
        return copy.copy_capture(source, destination);
    }
    let captured = fs.freeze(FreezeSource::Working(source), scratch()?)?;
    let consistency = captured.consistency;
    let materialised = fs.materialise(captured.tree.reader(), destination);
    drop(captured.tree);
    if let Err(failure) = fs.remove(captured.ownership) {
        let cause = match materialised {
            Err(cause) => cause,
            Ok(created) => {
                drop(created.tree);
                let cause = FsError::Invalid("capture cleanup failed");
                match fs.remove(created.ownership) {
                    Ok(()) => cause,
                    Err(failure) => FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) },
                }
            }
        };
        return Err(FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) });
    }
    Ok(WritableCapture { created: materialised?, consistency, owners: OwnerLayout::PerEntry })
}
