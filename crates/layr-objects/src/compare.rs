//! Compare two folders entry by entry and byte by byte (`layr objects verify`). This walk shares
//! no code with the tree writer, so a fault in the writer shows as a difference, not as two equal
//! faults.

use crate::fsx;
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
    let acc = Mutex::new(Acc::default());
    walk(a, b, b"", &acc)?;
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

fn names(dir: &Path) -> Result<BTreeSet<Vec<u8>>> {
    let mut out = BTreeSet::new();
    for e in fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        out.insert(e?.file_name().as_bytes().to_vec());
    }
    Ok(out)
}

fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
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

fn walk(a: &Path, b: &Path, rel: &[u8], acc: &Mutex<Acc>) -> Result<()> {
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
                if ma.len() != mb.len() || !same_bytes(&pa, &pb)? {
                    diffs.push(format!("content differs: {}", show(name)));
                }
            }
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
            walk(&pa, &pb, &child, acc)?;
        }
        Ok(())
    })
}
