//! `layr gc` deduplication of cache folders (docs/cache-overlays.md, "Deduplication").
//!
//! A build tool writes whole new files into a cloned cache folder, so the clone stops sharing
//! with its source. Files with equal content get one copy again:
//!
//! - APFS: whole files. Files of equal size are hashed, compared byte by byte, and the duplicate
//!   is replaced by a clone (`clonefile` to a temporary name, then an atomic rename) that keeps
//!   its mode, owner and mtime. APFS has no block-level API, so partial matches stay.
//! - btrfs: 128 KiB blocks, like `duperemove`. The kernel does the work in `FIDEDUPERANGE`, and
//!   shares an extent only when the bytes are equal. 128 KiB is the largest uncompressed btrfs
//!   extent: smaller blocks split extents into many small ones for little extra gain.
//!
//! Only files in the cache folders of lines are changed. Files in cache states and cache overlay
//! states are read as sources: they are read-only. Symbolic links are never followed. A file
//! with more than one link is skipped. The pass runs after gc released the project lock.
// The block pass is pure code that every platform compiles and tests; only btrfs runs it.
#![cfg_attr(target_os = "macos", allow(dead_code))]

use crate::fsutil;
use crate::repo::Repo;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Block size of the btrfs pass.
pub const BLOCK: u64 = 128 * 1024;
/// Smaller files save too little to be worth a hash.
pub const MIN_FILE: u64 = 4096;
/// The kernel shares at most this much per `FIDEDUPERANGE` call.
const MAX_CALL: u64 = 16 * 1024 * 1024;

/// What a file looked like when it was scanned. A change of any field means the file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: i64,
}

impl Stamp {
    pub fn of(m: &fs::Metadata) -> Stamp {
        Stamp { ino: m.ino(), size: m.size(), mtime: m.mtime(), mtime_ns: m.mtime_nsec() }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub stamp: Stamp,
    /// In the cache folder of a line: may be replaced. Otherwise only a source.
    pub target: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Report {
    /// Physical bytes that the files no longer hold twice.
    pub bytes: u64,
    pub files: u64,
    /// Wall clock time of the pass.
    pub seconds: f64,
}

/// The regular files below `root` that are worth a hash. Links are not followed.
fn collect(root: &Path, target: bool, out: &mut Vec<Entry>) {
    match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => {}
        _ => return,
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(m) = fs::symlink_metadata(&path) else { continue };
            if m.is_dir() {
                stack.push(path);
            } else if m.is_file() && m.nlink() == 1 && m.size() >= MIN_FILE {
                out.push(Entry { path, stamp: Stamp::of(&m), target });
            }
        }
    }
}

/// Groups of two or more files of one size, with at least one file that may be replaced.
pub fn size_groups(entries: Vec<Entry>) -> Vec<Vec<Entry>> {
    let mut by_size: BTreeMap<u64, Vec<Entry>> = BTreeMap::new();
    for e in entries {
        by_size.entry(e.stamp.size).or_default().push(e);
    }
    by_size.into_values().filter(|g| g.len() > 1 && g.iter().any(|e| e.target)).collect()
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = open_plain(path)?;
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 20]);
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// Open a regular file without following a link in the last component.
fn open_plain(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
}

/// Hashes of the 128 KiB blocks of a file (the last block may be shorter), 16 bytes each.
fn block_hashes(path: &Path) -> io::Result<Vec<[u8; 16]>> {
    let mut f = open_plain(path)?;
    let (mut out, mut buf) = (Vec::new(), vec![0u8; BLOCK as usize]);
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        let d = Sha256::digest(&buf[..n]);
        out.push(d[..16].try_into().expect("16 bytes"));
    }
    Ok(out)
}

/// One range of equal blocks: `dst` file range takes the extents of the `src` file range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub src: usize,
    pub src_off: u64,
    pub dst: usize,
    pub dst_off: u64,
    pub len: u64,
}

/// Block matches between files. `hashes[i]` are the block hashes of `files[i]` (empty for a
/// file that could not be read). The first file with a block is its source; every later block
/// of a file that may be replaced becomes a match. A file never matches itself.
pub fn plan_blocks(files: &[Entry], hashes: &[Vec<[u8; 16]>]) -> Vec<Match> {
    let len_of = |i: usize, b: usize| (files[i].stamp.size - b as u64 * BLOCK).min(BLOCK);
    let mut first: HashMap<[u8; 16], (usize, usize)> = HashMap::new();
    let mut out = Vec::new();
    for (i, hs) in hashes.iter().enumerate() {
        for (b, h) in hs.iter().enumerate() {
            match first.get(h) {
                None => {
                    first.insert(*h, (i, b));
                }
                Some(&(s, sb)) if s != i && files[i].target && len_of(s, sb) == len_of(i, b) => {
                    out.push(Match { src: s, src_off: sb as u64 * BLOCK, dst: i, dst_off: b as u64 * BLOCK, len: len_of(i, b) })
                }
                _ => {}
            }
        }
    }
    out
}

/// Join matches that follow each other in both files into one range. The input is any order.
pub fn merge_runs(mut m: Vec<Match>) -> Vec<Match> {
    m.sort_by_key(|x| (x.dst, x.src, x.dst_off));
    let mut out: Vec<Match> = Vec::new();
    for x in m {
        match out.last_mut() {
            Some(l) if l.dst == x.dst && l.src == x.src && l.dst_off + l.len == x.dst_off && l.src_off + l.len == x.src_off => {
                l.len += x.len
            }
            _ => out.push(x),
        }
    }
    out
}

/// The block pass. `shared(m)` tells that the range already shares its extents; `dedupe(m)`
/// asks the kernel and returns the bytes shared. A dry run counts the ranges that are not
/// shared yet. A "not supported" error ends the pass.
pub fn block_pass(
    files: &[Entry],
    hashes: &[Vec<[u8; 16]>],
    dry: bool,
    shared: &mut dyn FnMut(&Match) -> bool,
    dedupe: &mut dyn FnMut(&Match) -> io::Result<u64>,
) -> Report {
    let todo: Vec<Match> = plan_blocks(files, hashes).into_iter().filter(|m| !shared(m)).collect();
    let mut r = Report::default();
    let mut touched = std::collections::BTreeSet::new();
    for m in merge_runs(todo) {
        let got = if dry {
            m.len
        } else {
            match dedupe(&m) {
                Ok(n) => n,
                Err(e) if matches!(e.raw_os_error(), Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::ENOSYS)) => break,
                Err(_) => 0,
            }
        };
        if got > 0 {
            r.bytes += got;
            touched.insert(m.dst);
        }
    }
    r.files = touched.len() as u64;
    r
}

/// Share the ranges with `FIDEDUPERANGE`, at most 16 MiB per call.
fn dedupe_match(files: &[Entry], m: &Match) -> io::Result<u64> {
    let src = open_plain(&files[m.src].path)?;
    let dst = fs::OpenOptions::new().read(true).write(true).open(&files[m.dst].path)?;
    let mut done = 0;
    while done < m.len {
        let n = (m.len - done).min(MAX_CALL);
        let got = crate::sys::dedupe_range(&src, m.src_off + done, n, &dst, m.dst_off + done)?;
        if got == 0 {
            break;
        }
        done += got;
    }
    Ok(done)
}

fn shared_match(files: &[Entry], m: &Match) -> bool {
    let (Ok(a), Ok(b)) = (open_plain(&files[m.src].path), open_plain(&files[m.dst].path)) else { return false };
    match (crate::sys::physical_at(&a, m.src_off), crate::sys::physical_at(&b, m.dst_off)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// An OverlayFS store has no extents to share.
#[cfg(feature = "overlayfs")]
fn share(_: Vec<Entry>, _: bool) -> Report {
    Report::default()
}

#[cfg(all(not(target_os = "macos"), not(feature = "overlayfs")))]
fn share(entries: Vec<Entry>, dry: bool) -> Report {
    let hashes: Vec<Vec<[u8; 16]>> = entries.iter().map(|e| block_hashes(&e.path).unwrap_or_default()).collect();
    let (a, b) = (&entries, &entries);
    block_pass(&entries, &hashes, dry, &mut |m| shared_match(a, m), &mut |m| dedupe_match(b, m))
}

#[cfg(target_os = "macos")]
fn share(entries: Vec<Entry>, dry: bool) -> Report {
    let mut r = Report::default();
    for group in size_groups(entries) {
        // Files that already share all their extents (a clone of a clone) are one class: one
        // hash and one compare serve the whole class.
        let mut classes: Vec<Vec<Entry>> = Vec::new();
        let mut by_extents: HashMap<Vec<(u64, u64, u64)>, usize> = HashMap::new();
        for e in group {
            match apfs::extents(&e.path) {
                Some(x) => match by_extents.get(&x) {
                    Some(&c) => classes[c].push(e),
                    None => {
                        by_extents.insert(x, classes.len());
                        classes.push(vec![e]);
                    }
                },
                None => classes.push(vec![e]),
            }
        }
        let mut by_hash: BTreeMap<[u8; 32], Vec<Vec<Entry>>> = BTreeMap::new();
        for c in classes {
            if let Ok(h) = hash_file(&c[0].path) {
                by_hash.entry(h).or_default().push(c);
            }
        }
        for mut cs in by_hash.into_values().filter(|c| c.len() > 1) {
            let canon_class = cs.remove(cs.iter().position(|c| c.iter().any(|e| !e.target)).unwrap_or(0));
            let canon = canon_class.iter().find(|e| !e.target).unwrap_or(&canon_class[0]);
            for class in &cs {
                if !class.iter().any(|e| e.target) || !apfs::same_bytes(&canon.path, &class[0].path) {
                    continue;
                }
                for dup in class.iter().filter(|e| e.target) {
                    if dry || apfs::replace(canon, dup).unwrap_or(false) {
                        r.bytes += dup.stamp.size;
                        r.files += 1;
                    }
                }
            }
        }
    }
    r
}

#[cfg(target_os = "macos")]
mod apfs {
    use super::*;
    use std::os::fd::AsFd;

    /// The physical extents of a file, as a key for "shares everything with".
    pub fn extents(path: &Path) -> Option<Vec<(u64, u64, u64)>> {
        let f = open_plain(path).ok()?;
        crate::sys::apfs_extents(&f, f.metadata().ok()?.len())
    }

    pub fn same_bytes(a: &Path, b: &Path) -> bool {
        let (Ok(mut fa), Ok(mut fb)) = (open_plain(a), open_plain(b)) else { return false };
        let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
        loop {
            let (Ok(n), Ok(k)) = (read_full(&mut fa, &mut x), read_full(&mut fb, &mut y)) else { return false };
            if n != k || x[..n] != y[..k] {
                return false;
            }
            if n == 0 {
                return true;
            }
        }
    }

    /// Replace `dup` by a clone of `canon` with the mode, owner and mtime of `dup`. Returns
    /// false, and changes nothing, when either file changed since the scan.
    pub fn replace(canon: &Entry, dup: &Entry) -> Result<bool> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(dup.path.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?)?;
        let cf = open_plain(&canon.path)?;
        if Stamp::of(&cf.metadata()?) != canon.stamp {
            return Ok(false);
        }
        let name = format!(".layr-dedup-{}", std::process::id());
        let tmp = dup.path.with_file_name(&name);
        crate::sys::apfs_clone_at(cf.as_fd(), dir.as_fd(), &name)?;
        let r = (|| -> Result<bool> {
            let df = open_plain(&dup.path)?;
            let dm = df.metadata()?;
            if Stamp::of(&dm) != dup.stamp {
                return Ok(false);
            }
            let tf = open_plain(&tmp)?;
            // A clone of a state file is immutable like its source.
            crate::sys::apfs_unseal_fd(tf.as_fd())?;
            crate::sys::apfs_copy_metadata_fd(df.as_fd(), tf.as_fd())?;
            crate::sys::apfs_mtime(&tmp, dm.mtime(), dm.mtime_nsec())?;
            // The check that guards the rename: the duplicate and the source are as scanned.
            if Stamp::of(&fs::symlink_metadata(&dup.path)?) != dup.stamp || Stamp::of(&cf.metadata()?) != canon.stamp {
                return Ok(false);
            }
            fs::rename(&tmp, &dup.path)?;
            Ok(true)
        })();
        if !matches!(r, Ok(true)) {
            if let Ok(tf) = open_plain(&tmp) {
                let _ = crate::sys::apfs_unseal_fd(tf.as_fd());
            }
            let _ = fs::remove_file(&tmp);
        }
        r
    }
}

/// Share equal content inside each cache overlay: the folders of its lines, its states and the
/// cache states of the same folder. Never between two overlays (`target` and `.layr-retest`) and
/// never between projects. A dry run changes nothing and counts what a run would share.
/// States are only read, so no state changes its content or its identifier.
pub fn run(repo: &Repo, dry: bool) -> Result<Report> {
    let started = std::time::Instant::now();
    let mut total = Report::default();
    let states = repo.p.db.present_states()?;
    for (name, folder) in repo.folder_overlays()? {
        let mut entries = Vec::new();
        for line in repo.lines()? {
            if let Ok(cp) = fsutil::safe_join(&repo.working(&line), &folder) {
                collect(&cp, true, &mut entries);
            }
        }
        if let Ok(orepo) = repo.overlay_repo(&name) {
            for (id, _) in orepo.p.db.present_states()? {
                collect(&orepo.p.state_path(&id), false, &mut entries);
            }
        }
        let snap = format!("@{}", folder.replace('/', "%2F"));
        for (id, _) in &states {
            if let Some(s) = repo.p.db.state(id)? {
                for n in s.caches.values().filter(|n| n.ends_with(&snap)) {
                    collect(&repo.p.cache_path(id, n), false, &mut entries);
                }
            }
        }
        let r = share(entries, dry);
        total.bytes += r.bytes;
        total.files += r.files;
    }
    total.seconds = started.elapsed().as_secs_f64();
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(size: u64, target: bool) -> Entry {
        Entry { path: PathBuf::from(format!("/x/{size}/{target}")), stamp: Stamp { ino: 1, size, mtime: 0, mtime_ns: 0 }, target }
    }

    #[test]
    fn candidates_are_groups_of_one_size_with_a_target() {
        let g = size_groups(vec![entry(10, true), entry(10, false), entry(20, true), entry(30, false), entry(30, false)]);
        assert_eq!(g.len(), 1, "a lone file and a group without a target are no candidates");
        assert_eq!(g[0].len(), 2);
        assert_eq!(g[0][0].stamp.size, 10);
    }

    #[test]
    fn a_changed_file_has_another_stamp() {
        let d = std::env::temp_dir().join(format!("layr-dedup-stamp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let f = d.join("f");
        fs::write(&f, vec![1u8; 5000]).unwrap();
        let before = Stamp::of(&fs::symlink_metadata(&f).unwrap());
        assert_eq!(before, Stamp::of(&fs::symlink_metadata(&f).unwrap()));
        fs::write(&f, vec![2u8; 5000]).unwrap();
        assert_ne!(before, Stamp::of(&fs::symlink_metadata(&f).unwrap()), "mtime moved");
        fs::write(&f, vec![2u8; 6000]).unwrap();
        assert_ne!(before.size, Stamp::of(&fs::symlink_metadata(&f).unwrap()).size);
        let g = d.join("g");
        fs::rename(&f, &g).unwrap();
        fs::write(&f, vec![1u8; 5000]).unwrap();
        assert_ne!(before.ino, Stamp::of(&fs::symlink_metadata(&f).unwrap()).ino, "a new file has a new inode");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn collect_skips_links_small_files_and_hard_links() {
        let d = std::env::temp_dir().join(format!("layr-dedup-collect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/big"), vec![0u8; 5000]).unwrap();
        fs::write(d.join("small"), b"x").unwrap();
        fs::write(d.join("linked"), vec![0u8; 5000]).unwrap();
        fs::hard_link(d.join("linked"), d.join("linked2")).unwrap();
        std::os::unix::fs::symlink(d.join("sub/big"), d.join("link")).unwrap();
        let mut out = Vec::new();
        collect(&d, true, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].path.ends_with("sub/big"));
        let mut via_link = Vec::new();
        collect(&d.join("link"), true, &mut via_link);
        assert!(via_link.is_empty(), "a link as the root is not followed");
        fs::remove_dir_all(&d).unwrap();
    }

    fn h(n: u8) -> [u8; 16] {
        [n; 16]
    }

    #[test]
    fn blocks_match_at_other_offsets_and_merge_into_runs() {
        let big = BLOCK * 3;
        let files = vec![entry(big, false), entry(big + BLOCK, true)];
        // File 1 holds the blocks of file 0 behind one new block.
        let hashes = vec![vec![h(1), h(2), h(3)], vec![h(9), h(1), h(2), h(3)]];
        let plan = plan_blocks(&files, &hashes);
        assert_eq!(plan.len(), 3);
        let runs = merge_runs(plan);
        assert_eq!(runs, vec![Match { src: 0, src_off: 0, dst: 1, dst_off: BLOCK, len: BLOCK * 3 }]);
    }

    #[test]
    fn a_source_only_file_is_never_changed_and_no_file_matches_itself() {
        let files = vec![entry(BLOCK * 2, true), entry(BLOCK * 2, false)];
        let hashes = vec![vec![h(1), h(1)], vec![h(1), h(2)]];
        assert!(plan_blocks(&files, &hashes).is_empty());
        let only_src = vec![entry(BLOCK, false), entry(BLOCK, false)];
        assert!(plan_blocks(&only_src, &[vec![h(1)], vec![h(1)]]).is_empty());
    }

    #[test]
    fn the_block_pass_skips_shared_ranges_and_dry_run_calls_nothing() {
        let files = vec![entry(BLOCK * 2, false), entry(BLOCK * 2, true)];
        let hashes = vec![vec![h(1), h(2)], vec![h(1), h(2)]];
        let mut calls = Vec::new();
        let dry = block_pass(&files, &hashes, true, &mut |_| false, &mut |m| {
            calls.push(*m);
            Ok(m.len)
        });
        assert_eq!(dry, Report { bytes: BLOCK * 2, files: 1, seconds: 0.0 });
        assert!(calls.is_empty());
        let r = block_pass(&files, &hashes, false, &mut |m| m.dst_off == 0, &mut |m| {
            calls.push(*m);
            Ok(m.len)
        });
        assert_eq!(r.bytes, BLOCK, "the first block already shares");
        assert_eq!(calls.len(), 1);
        let unsupported =
            block_pass(&files, &hashes, false, &mut |_| false, &mut |_| Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)));
        assert_eq!(unsupported, Report::default());
    }

    #[cfg(target_os = "macos")]
    fn pair(name: &str) -> (PathBuf, Entry, Entry) {
        let d = std::env::temp_dir().join(format!("layr-dedup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("canon"), vec![7u8; 9000]).unwrap();
        fs::write(d.join("dup"), vec![7u8; 9000]).unwrap();
        let e = |n: &str, target| {
            let p = d.join(n);
            Entry { stamp: Stamp::of(&fs::symlink_metadata(&p).unwrap()), path: p, target }
        };
        let (c, u) = (e("canon", false), e("dup", true));
        (d, c, u)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_replace_keeps_mtime_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (d, c, u) = pair("ok");
        fs::set_permissions(&u.path, fs::Permissions::from_mode(0o640)).unwrap();
        crate::sys::apfs_mtime(&u.path, 1_577_934_245, 123).unwrap();
        let u = Entry { stamp: Stamp::of(&fs::symlink_metadata(&u.path).unwrap()), ..u };
        assert!(apfs::replace(&c, &u).unwrap());
        let m = fs::symlink_metadata(&u.path).unwrap();
        assert_ne!(m.ino(), u.stamp.ino, "a new inode");
        assert_eq!((m.mtime(), m.mtime_nsec(), m.mode() & 0o777), (1_577_934_245, 123, 0o640));
        assert_eq!(fs::read(&u.path).unwrap(), vec![7u8; 9000]);
        assert_eq!(apfs::extents(&c.path), apfs::extents(&u.path));
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2, "no temporary file");
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_replace_skips_a_file_that_changed_after_the_scan() {
        let (d, c, u) = pair("changed");
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&u.path, vec![8u8; 9000]).unwrap();
        assert!(!apfs::replace(&c, &u).unwrap(), "the duplicate changed");
        assert_eq!(fs::read(&u.path).unwrap(), vec![8u8; 9000]);
        let (_, c2, u2) = pair("srcchanged");
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&c2.path, vec![7u8; 9000]).unwrap();
        assert!(!apfs::replace(&c2, &u2).unwrap(), "the source changed");
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2, "no temporary file");
        for n in ["changed", "srcchanged"] {
            let _ = fs::remove_dir_all(std::env::temp_dir().join(format!("layr-dedup-{n}-{}", std::process::id())));
        }
    }
}
