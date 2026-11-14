//! Compare two folders entry by entry and byte by byte (`layr objects verify`). This walk shares
//! no code with the tree writer, so a fault in the writer shows as a difference, not as two equal
//! faults.

use crate::fsx::{self, EntryKind};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;

/// What `compare` checked and what differs (empty when the folders are equal).
#[derive(Debug, Default)]
pub struct Comparison {
    pub entries: u64,
    pub bytes: u64,
    pub differences: Vec<String>,
}

/// The paths of each multiply linked inode, by (device, inode).
type Inodes = BTreeMap<(u64, u64), BTreeSet<Vec<u8>>>;

#[derive(Default)]
struct Acc {
    times: bool,
    entries: u64,
    bytes: u64,
    differences: Vec<String>,
    /// Paths of each inode with more than one link, per side.
    links: [Inodes; 2],
}

/// Compare folder `a` with folder `b`: the set of names, entry types, permission bits (not for
/// symbolic links), file bytes, link targets, kept extended attributes, and hard link groups.
/// Owners and times are not compared (a tree does not keep them).
pub fn compare(a: &Path, b: &Path) -> Result<Comparison> {
    compare_with(a, b, false)
}

/// `compare`, and with `times` the modification times of every entry too (the root included).
/// Use `compare_with_kinds` for storage readers that classify entries with no meaningful time.
pub fn compare_with(a: &Path, b: &Path, times: bool) -> Result<Comparison> {
    compare_with_kinds(a, b, times, |_, _| Ok(EntryKind::Ordinary))
}

/// Compare with entry kinds supplied by a filesystem reader. Only meaningful times are checked.
pub fn compare_with_kinds(
    a: &Path,
    b: &Path,
    times: bool,
    entry_kind: fn(&Path, &fs::Metadata) -> Result<EntryKind>,
) -> Result<Comparison> {
    let acc = Mutex::new(Acc { times, ..Acc::default() });
    if times && times_differ(a, &fs::symlink_metadata(a)?, b, &fs::symlink_metadata(b)?, entry_kind)? {
        acc.lock().expect("no panic holds the lock").differences.push("time differs: the root folder".into());
    }
    walk(a, b, b"", &acc, entry_kind)?;
    let mut acc = acc.into_inner().expect("no panic holds the lock");
    let groups = |m: &Inodes| -> BTreeSet<BTreeSet<Vec<u8>>> { m.values().filter(|g| g.len() > 1).cloned().collect() };
    let (ga, gb) = (groups(&acc.links[0]), groups(&acc.links[1]));
    for g in ga.symmetric_difference(&gb) {
        let names: Vec<String> = g.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        acc.differences.push(format!("hard link group differs: {}", names.join(", ")));
    }
    acc.differences.sort();
    Ok(Comparison { entries: acc.entries, bytes: acc.bytes, differences: acc.differences })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch;
    use crate::times;

    #[test]
    fn injected_placeholder_ignores_only_its_time_on_either_side() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("compare-placeholder");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(a.join("folder"))?;
        fs::create_dir_all(b.join("folder"))?;
        fs::set_permissions(a.join("folder"), fs::Permissions::from_mode(0o755))?;
        for path in [&a, &b, &a.join("folder")] {
            times::set(path, 0)?;
        }
        times::set(&b.join("folder"), 123_456_789)?;
        assert_eq!(compare_with(&a, &b, true)?.differences, ["time differs: folder"]);
        assert!(compare(&a, &b)?.differences.is_empty());
        let classify = |path: &Path, _: &fs::Metadata| {
            Ok(if path.ends_with("a/folder") { EntryKind::SubvolumePlaceholder } else { EntryKind::Ordinary })
        };
        assert!(compare_with_kinds(&a, &b, true, classify)?.differences.is_empty());
        assert!(compare_with_kinds(&b, &a, true, classify)?.differences.is_empty());
        times::set(&b, 1)?;
        assert_eq!(compare_with_kinds(&a, &b, true, classify)?.differences, ["time differs: the root folder"]);
        times::set(&b, 0)?;
        fs::set_permissions(b.join("folder"), fs::Permissions::from_mode(0o700))?;
        assert!(compare_with_kinds(&a, &b, true, classify)
            ?
            .differences
            .contains(&"mode differs: folder (755 and 700)".into()));
        fs::write(b.join("folder/extra"), "extra child")?;
        assert!(compare_with_kinds(&a, &b, true, classify)
            ?
            .differences
            .contains(&"only in the copy: folder/extra".into()));
        fs::remove_dir_all(b.join("folder"))?;
        fs::write(b.join("folder"), "changed type")?;
        assert!(compare_with_kinds(&a, &b, true, classify)
            ?
            .differences
            .contains(&"type differs: folder (folder and file)".into()));
        Ok(())
    }
}

fn names(dir: &Path) -> Result<BTreeSet<Vec<u8>>> {
    let mut out = BTreeSet::new();
    for e in fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        out.insert(e?.file_name().as_bytes().to_vec());
    }
    Ok(out)
}

fn same_bytes(a: &Path, b: &Path, len: u64) -> Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    thread_local! {
        static BUFFERS: std::cell::RefCell<[Vec<u8>; 2]> = const {
            std::cell::RefCell::new([Vec::new(), Vec::new()])
        };
    }
    BUFFERS.with_borrow_mut(|[a, b]| {
        let ba = fsx::file_buffer(a, len);
        let bb = fsx::file_buffer(b, len);
        loop {
            let na = read_full(&mut fa, ba)?;
            let nb = read_full(&mut fb, bb)?;
            if na != nb || ba[..na] != bb[..nb] {
                return Ok(false);
            }
            if na == 0 {
                return Ok(true);
            }
        }
    })
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let got = f.read(&mut buf[n..])?;
        if got == 0 {
            break;
        }
        n += got;
    }
    Ok(n)
}

fn times_differ(
    a: &Path,
    ma: &fs::Metadata,
    b: &Path,
    mb: &fs::Metadata,
    entry_kind: fn(&Path, &fs::Metadata) -> Result<EntryKind>,
) -> Result<bool> {
    Ok(matches!((entry_kind(a, ma)?.time(ma), entry_kind(b, mb)?.time(mb)), (Some(a), Some(b)) if a != b))
}

fn walk(
    a: &Path,
    b: &Path,
    rel: &[u8],
    acc: &Mutex<Acc>,
    entry_kind: fn(&Path, &fs::Metadata) -> Result<EntryKind>,
) -> Result<()> {
    let (na, nb) = (names(a)?, names(b)?);
    let show = |name: &[u8]| -> String {
        let p = if rel.is_empty() { name.to_vec() } else { [rel, b"/", name].concat() };
        String::from_utf8_lossy(&p).into_owned()
    };
    {
        let mut acc = acc.lock().expect("no panic holds the lock");
        for only in na.difference(&nb) {
            acc.differences.push(format!("only in the original: {}", show(only)));
        }
        for only in nb.difference(&na) {
            acc.differences.push(format!("only in the copy: {}", show(only)));
        }
    }
    let both: Vec<Vec<u8>> = na.intersection(&nb).cloned().collect();
    both.par_iter().try_for_each(|name| -> Result<()> {
        let (pa, pb) = (a.join(fsx::os(name)), b.join(fsx::os(name)));
        let child = if rel.is_empty() { name.clone() } else { [rel, b"/", name].concat() };
        let (ma, mb) = (fs::symlink_metadata(&pa)?, fs::symlink_metadata(&pb)?);
        let mut diffs = Vec::new();
        let (ta, tb) = (ma.file_type(), mb.file_type());
        let kind = |t: fs::FileType| {
            if t.is_dir() {
                "folder"
            } else if t.is_file() {
                "file"
            } else if t.is_symlink() {
                "symbolic link"
            } else {
                "special file"
            }
        };
        let mut bytes = 0;
        if kind(ta) != kind(tb) {
            diffs.push(format!("type differs: {} ({} and {})", show(name), kind(ta), kind(tb)));
        } else {
            if !ta.is_symlink() && ma.mode() & 0o7777 != mb.mode() & 0o7777 {
                diffs.push(format!("mode differs: {} ({:o} and {:o})", show(name), ma.mode() & 0o7777, mb.mode() & 0o7777));
            }
            if fsx::xattrs(&pa)? != fsx::xattrs(&pb)? {
                diffs.push(format!("extended attributes differ: {}", show(name)));
            }
            if ta.is_symlink() && fs::read_link(&pa)? != fs::read_link(&pb)? {
                diffs.push(format!("link target differs: {}", show(name)));
            }
            if ta.is_file() {
                bytes = ma.len();
                if ma.len() != mb.len() || !same_bytes(&pa, &pb, ma.len())? {
                    diffs.push(format!("content differs: {}", show(name)));
                }
            }
        }
        let check_times = acc.lock().expect("no panic holds the lock").times;
        if check_times && times_differ(&pa, &ma, &pb, &mb, entry_kind)? {
            diffs.push(format!("time differs: {}", show(name)));
        }
        {
            let mut acc = acc.lock().expect("no panic holds the lock");
            acc.entries += 1;
            acc.bytes += bytes;
            acc.differences.extend(diffs);
            for (side, m) in [&ma, &mb].into_iter().enumerate() {
                if m.is_file() && m.nlink() > 1 {
                    acc.links[side].entry((m.dev(), m.ino())).or_default().insert(child.clone());
                }
            }
        }
        if ta.is_dir() && tb.is_dir() {
            walk(&pa, &pb, &child, acc, entry_kind)?;
        }
        Ok(())
    })
}
