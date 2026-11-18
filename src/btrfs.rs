//! btrfs operations: subvolumes, snapshots, reflinks (through `sys` and `rustix`), and the
//! `btrfs` command for send streams.

use crate::sys;
mod accounting;
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
pub(crate) mod entries;

const FIRST_FREE_OBJECTID: u64 = 256;

/// A nested subvolume leaves inode 2 at its boundary in a parent snapshot. Unlike a real
/// subvolume root (256), its inode times are invented whenever the kernel instantiates it.
pub fn entry_kind(path: &Path, meta: &fs::Metadata) -> Result<layr_objects::fsx::EntryKind> {
    use layr_objects::fsx::EntryKind;
    const EMPTY_SUBVOL_DIR_OBJECTID: u64 = 2;
    if meta.is_dir()
        && meta.ino() == EMPTY_SUBVOL_DIR_OBJECTID
        && rustix::fs::statfs(path)?.f_type as i64 == BTRFS_SUPER_MAGIC
        && fs::symlink_metadata(path.join(".."))?.dev() != meta.dev()
    {
        return Ok(EntryKind::SubvolumePlaceholder);
    }
    Ok(EntryKind::Ordinary)
}

thread_local! {
    // New snapshots can still be discarded by this command. Do not persist their diffs.
    static NEW_SNAPSHOTS: std::cell::RefCell<std::collections::HashSet<PathBuf>> = Default::default();
}

#[derive(Debug, Clone)]
pub struct SubvolInfo {
    pub uuid: String,
    pub received_uuid: Option<String>,
    /// The subvolume this one is a snapshot of.
    pub parent_uuid: Option<String>,
    pub readonly: bool,
}

fn fmt_uuid(b: &[u8; 16]) -> Option<String> {
    if b.iter().all(|x| *x == 0) {
        return None;
    }
    Some(uuid::Uuid::from_bytes(*b).hyphenated().to_string())
}

/// True when `path` lives on a btrfs file system.
pub(super) fn supports_volume(path: &Path) -> bool {
    match rustix::fs::statfs(path) {
        Ok(st) => st.f_type as i64 == BTRFS_SUPER_MAGIC,
        Err(_) => false,
    }
}

/// True when `path` is the root of a subvolume.
pub(super) fn managed_root(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.is_dir() && m.ino() == FIRST_FREE_OBJECTID && supports_volume(path),
        Err(_) => false,
    }
}

/// btrfs boundaries are encoded in the metadata already read by the validation walker.
pub(crate) struct NestedRoots;
impl NestedRoots {
    pub(crate) fn read(_root: &Path) -> Result<Self> {
        Ok(Self)
    }
    pub(crate) fn classify(&self, _path: &Path, metadata: &fs::Metadata) -> std::io::Result<crate::fsutil::StorageRoot> {
        Ok(if metadata.is_dir() && metadata.ino() == FIRST_FREE_OBJECTID {
            crate::fsutil::StorageRoot::Managed
        } else {
            crate::fsutil::StorageRoot::Unmanaged
        })
    }
}

/// Snapshot `src` (a subvolume) to `dst`. The destination must not exist.
/// A read-only snapshot for comparing content.
/// A writable copy of a cache folder: a subvolume snapshot, which is O(1) and keeps every link.
/// Returns false: a snapshot keeps each entry's owner, so every entry needs a check.
pub(crate) fn remember_snapshot(path: &Path) {
    NEW_SNAPSHOTS.with(|paths| paths.borrow_mut().insert(path.to_path_buf()));
}

/// Delete the subvolume at `path`. Nested subvolumes must be deleted first.
/// Make a subvolume read-only (a writable snapshot becomes a state after its changes).
/// Give a restored or received subvolume the received UUID and transaction of its signed state,
/// the identity that `recv` checks for a later send.

pub(super) fn seal_tree(path: &Path) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let flags = sys::subvol_flags(f.as_fd())?;
    sys::subvol_set_flags(f.as_fd(), flags | sys::SUBVOL_RDONLY).with_context(|| format!("make {} read-only", path.display()))
}

/// Make a read-only subvolume writable again (btrfs does not rename a read-only subvolume).
pub(super) fn unseal_tree(path: &Path) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let flags = sys::subvol_flags(f.as_fd())?;
    sys::subvol_set_flags(f.as_fd(), flags & !sys::SUBVOL_RDONLY).with_context(|| format!("make {} writable", path.display()))
}

pub(super) fn native_info(path: &Path) -> Result<SubvolInfo> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let raw = sys::subvol_info(f.as_fd()).with_context(|| format!("subvolume info {}", path.display()))?;
    let flags = sys::subvol_flags(f.as_fd())?;
    Ok(SubvolInfo {
        uuid: fmt_uuid(&raw.uuid).unwrap_or_default(),
        received_uuid: fmt_uuid(&raw.received_uuid),
        parent_uuid: fmt_uuid(&raw.parent_uuid),
        readonly: flags & sys::SUBVOL_RDONLY != 0,
    })
}

/// Delete a subvolume and every subvolume nested inside it, or a plain folder tree. The tree is
/// walked through descriptors (no link is followed), because a line belongs to its owner.
/// Destroy the subvolume `path` without a search: O(1) in the usual case. A subvolume with
/// nested subvolumes (a working folder with its .git folder or caches) answers ENOTEMPTY; then
/// the `.git` folder and the `nested` paths (known cache roots) go first, through descriptors.
/// False when nested subvolumes remain: the caller searches for them (`delete_tree`) or defers.
/// Move a tree to `to` on the same file system (the store trash).
pub fn move_tree(src: &Path, to: &Path) -> Result<()> {
    std::fs::rename(src, to).with_context(|| format!("move {} to {}", src.display(), to.display()))
}

/// Reflink the whole content of `src` into `dst`.
/// Reflink a byte range. Offsets and length must be aligned to the block size, except a range
/// that ends at the end of the source file.
/// Copies what it reads into a buffer that stays at most `limit` bytes; past that it drops the
/// copy (`kept` then gives nothing).
struct Tee<R> {
    inner: R,
    copy: Option<Vec<u8>>,
    limit: usize,
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(copy) = &mut self.copy {
            if copy.len() + n > self.limit {
                self.copy = None;
            } else {
                copy.extend_from_slice(&buf[..n]);
            }
        }
        Ok(n)
    }
}

/// Ask the kernel for a metadata stream and decode it as it arrives, or decode an immutable
/// pair from the cache. Only a stream that the cache can keep is also buffered.
pub(crate) fn send_no_data_pinned(parent: &Path, child: &Path, p: &File, c: &File) -> Result<Vec<crate::stream::Entry>> {
    let _t = crate::trace::span("send --no-data");
    if sys::filesystem_uuid(p.as_fd())? != sys::filesystem_uuid(c.as_fd())? {
        bail!("the snapshots are on different filesystems");
    }
    let is_new = NEW_SNAPSHOTS.with(|paths| paths.borrow().contains(parent) || paths.borrow().contains(child));
    let cache = if is_new { None } else { crate::send_cache::Cache::for_states(parent, child, p, c) };
    if let Some(data) = cache.as_ref().and_then(|cache| cache.read()) {
        return crate::stream::entries_from(&data[..]);
    }
    let parent_id = sys::subvol_info(p.as_fd())?.tree_id;
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let (entries, kept) = std::thread::scope(|scope| {
        let writer = std::thread::Builder::new()
            .spawn_scoped(scope, move || {
                let result = sys::send_no_data(c.as_fd(), write.as_fd(), parent_id);
                drop(write);
                result
            })
            .context("start metadata stream thread")?;
        let mut tee = Tee {
            inner: File::from(read),
            copy: cache.as_ref().map(|_| Vec::new()),
            limit: crate::send_cache::MAX_ENTRY as usize,
        };
        let result = crate::stream::entries_from(&mut tee);
        // Close before joining, including on a read error, so the producer cannot block.
        let kept = tee.copy.take();
        drop(tee);
        let sent = writer.join().map_err(|_| anyhow!("metadata stream thread failed"))?;
        // A failed send closes the pipe, so the read fails too. Report the send error, which has the cause.
        sent.context("send metadata stream")?;
        let entries = result.context("read metadata stream")?;
        Ok::<_, anyhow::Error>((entries, kept))
    })?;
    if let (Some(cache), Some(data)) = (cache, kept) {
        // Cache failures do not change command results.
        let _ = cache.write(&data);
    }
    Ok(entries)
}

/// Extra sources and compressed extents require an explicit agreement with the receiver.
pub(super) fn native_send(parent: Option<&Path>, child: &Path, clones: &[PathBuf], compressed: bool) -> Result<Command> {
    let mut c = Command::new("btrfs");
    c.arg("send").arg("-q");
    if compressed && supports_compressed_send() {
        c.arg("--proto").arg("2").arg("--compressed-data");
    }
    if let Some(p) = parent {
        c.arg("-p").arg(p);
    }
    for source in clones {
        c.arg("-c").arg(source);
    }
    c.arg(child);
    Ok(c)
}

fn tools_version() -> Option<(u32, u32)> {
    static VERSION: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = Command::new("btrfs").arg("version").output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).split_whitespace().find_map(|word| {
            let mut parts = word.strip_prefix('v')?.split(|c: char| !c.is_ascii_digit());
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
    })
}

pub(super) fn compressed_input() -> bool {
    tools_version().is_some_and(|v| v >= (6, 0))
}

// Before 6.11, receive searches clone sources only by received UUID, not local UUID.
pub(super) fn local_clone_input() -> bool {
    tools_version().is_some_and(|v| v >= (6, 11))
}

fn supports_compressed_send() -> bool {
    compressed_input()
        && std::fs::read_to_string("/sys/fs/btrfs/features/send_stream_version")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|version| version >= 2)
}

/// An immutable source identity accepted by the receiver. Local UUIDs do not need a remote transaction ID.
pub(super) fn native_clone_identity(path: &Path) -> Result<(String, Option<u64>)> {
    let f = File::open(path)?;
    if sys::subvol_flags(f.as_fd())? & sys::SUBVOL_RDONLY == 0 {
        bail!("clone source is not read-only");
    }
    let info = sys::subvol_info(f.as_fd())?;
    Ok(match fmt_uuid(&info.received_uuid) {
        Some(uuid) => (uuid, Some(info.stransid)),
        // A return stream carries the remote ctransid. The original local UUID
        // identifies its source without a transaction from the other filesystem.
        None => (fmt_uuid(&info.uuid).ok_or_else(|| anyhow!("clone source has no UUID"))?, None),
    })
}

/// The kernel writes its local ctransid even when the UUID comes from a received snapshot.
pub(super) fn native_send_identity(path: &Path) -> Result<(String, u64)> {
    let (uuid, _) = native_clone_identity(path)?;
    let f = File::open(path)?;
    Ok((uuid, sys::subvol_info(f.as_fd())?.ctransid))
}

/// Identity retained by btrfs snapshots, used to stage file moves.
pub(super) fn native_file_identity(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::symlink_metadata(path)?.ino().to_le_bytes().to_vec())
}

/// Disk measure_space of a folder (`layr du`): extents that a snapshot or a reflink shares are flagged.
pub(super) fn measure_space(path: &Path) -> Result<crate::fsutil::Space> {
    crate::fsutil::fiemap_space(path)
}

/// Bytes that a copy of `path` occupies on another btrfs file system: each distinct extent once.
pub(super) fn copy_allocation(path: &Path) -> Result<u64> {
    let mut keys: std::collections::HashMap<crate::fsutil::ExtentKey, u64> = Default::default();
    crate::fsutil::fiemap_visit(path, &mut |h| {
        for &(key, size) in h.shared {
            if !matches!(key, crate::fsutil::ExtentKey::Unaddressed { .. }) {
                keys.entry(key).or_insert(size);
            }
        }
    })?;
    Ok(keys.values().sum())
}

/// Allocated bytes of `path` with shared data counted per file (the size of a flattened copy).
pub(super) fn copy_flat_allocation(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    crate::fsutil::fiemap_visit(path, &mut |h| total += h.alloc)?;
    Ok(total)
}

/// Copies a project to another btrfs file system (`src/project_copy.rs`). Each subvolume goes
/// with `btrfs send` and `btrfs receive`. Sending with clone sources (`-c`) or a parent (`-p`)
/// fails for sibling snapshots ("parent determination failed"), so each subvolume is sent whole
/// and then its files that have the same extents as an earlier copy are reflinked from it.
pub(crate) struct Copier {
    root: PathBuf,
    /// The extents (logical, physical, length) of each file content already copied, with the
    /// copy of its first file. A file with the same extents shares that copy.
    seen: std::collections::HashMap<Vec<(u64, u64, u64)>, PathBuf>,
    /// (device, inode) -> the first copy of a hard-linked file.
    links: std::collections::HashMap<(u64, u64), PathBuf>,
    serial: u64,
}

impl Copier {
    pub(crate) fn new(src_root: &Path, _dst_root: &Path) -> Result<Copier> {
        // Snapshots left by an interrupted copy of this project.
        let tmp = src_root.join("meta").join(crate::project_copy::TMP);
        if tmp.exists() {
            crate::fs::remove(&tmp)?;
        }
        Ok(Copier { root: src_root.to_path_buf(), seen: Default::default(), links: Default::default(), serial: 0 })
    }

    pub(crate) fn native_subvolume(&self, path: &Path) -> bool {
        managed_root(path)
    }

    /// Create the folder `dst` for `src`: a subvolume for a subvolume, else a folder.
    pub(crate) fn shell_dir(&mut self, src: &Path, dst: &Path) -> Result<()> {
        if managed_root(src) {
            crate::fs::create(dst)
        } else {
            fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))
        }
    }

    pub(crate) fn dir_close(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        let to = File::open(dst)?;
        // The owner first: a change of owner clears the set-id bits of the mode.
        std::os::unix::fs::lchown(dst, Some(m.uid()), Some(m.gid())).with_context(|| format!("owner of {}", dst.display()))?;
        crate::fsutil::copy_metadata(&File::open(src)?, &to)
    }

    pub(crate) fn file(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        let link = (m.dev(), m.ino());
        if m.nlink() > 1 {
            if let Some(first) = self.links.get(&link) {
                return fs::hard_link(first, dst).with_context(|| format!("link {}", dst.display()));
            }
        }
        let mut from = File::open(src).with_context(|| format!("open {}", src.display()))?;
        let mut to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dst)
            .with_context(|| format!("create {}", dst.display()))?;
        std::io::copy(&mut from, &mut to).with_context(|| format!("copy {}", src.display()))?;
        rustix::fs::fchown(&to, Some(rustix::fs::Uid::from_raw(m.uid())), Some(rustix::fs::Gid::from_raw(m.gid())))
            .with_context(|| format!("owner of {}", dst.display()))?;
        crate::fsutil::copy_metadata(&from, &to)?;
        if m.nlink() > 1 {
            self.links.insert(link, dst.to_path_buf());
        }
        Ok(())
    }

    pub(crate) fn symlink(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst).with_context(|| format!("link {}", dst.display()))?;
        crate::fsutil::copy_symlink_xattrs(src, dst)?;
        std::os::unix::fs::lchown(dst, Some(m.uid()), Some(m.gid())).with_context(|| format!("owner of {}", dst.display()))
    }

    /// Send one subvolume and receive it at `dst`, then the subvolumes nested inside it. A
    /// writable subvolume is sent from a temporary read-only snapshot in the source.
    pub(crate) fn subvolume(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let info = native_info(src)?;
        self.serial += 1;
        let serial = self.serial;
        let name = src.file_name().context("subvolume without a name")?.to_owned();
        let (send, temp) = if info.readonly {
            (src.to_path_buf(), None)
        } else {
            let dir = self.root.join("meta").join(crate::project_copy::TMP).join(serial.to_string());
            // A frozen snapshot needs a private parent (owner only).
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
            for d in [self.root.join("meta").join(crate::project_copy::TMP), dir.clone()] {
                fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
            }
            let snap = dir.join(&name);
            crate::fs::snapshot(src, &snap, true)?;
            (snap, Some(dir))
        };
        let inbox = dst.parent().context("subvolume target has no parent")?.join(format!(".layr-copy-in-{serial}"));
        fs::create_dir(&inbox).with_context(|| format!("create {}", inbox.display()))?;
        let mut sender = Command::new("btrfs");
        sender.arg("send").arg("-q").arg(&send);
        sender.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut sending = sender.spawn().context("start btrfs send")?;
        let pipe = sending.stdout.take().context("btrfs send has no output")?;
        let received = Command::new("btrfs")
            .arg("receive")
            .arg(&inbox)
            .stdin(std::process::Stdio::from(pipe))
            .stderr(std::process::Stdio::piped())
            .output()
            .context("start btrfs receive")?;
        let sent = sending.wait_with_output().context("wait for btrfs send")?;
        if !received.status.success() || !sent.status.success() {
            bail!(
                "btrfs send of {} failed: {}{}",
                src.display(),
                String::from_utf8_lossy(&sent.stderr).trim(),
                String::from_utf8_lossy(&received.stderr).trim()
            );
        }
        // btrfs does not rename a read-only subvolume. The copy is writable while it is placed
        // and while nested subvolumes come in; a read-only original is made read-only again.
        unseal_tree(&inbox.join(&name))?;
        fs::rename(inbox.join(&name), dst).with_context(|| format!("place {}", dst.display()))?;
        fs::remove_dir(&inbox)?;
        self.share_files(src, dst)?;
        self.restore_folders(src, dst)?;
        for rel in nested_subvolumes(src)? {
            let target = dst.join(&rel);
            // The send leaves an empty folder where a nested subvolume is.
            if fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir() && !managed_root(&target)) {
                fs::remove_dir(&target).with_context(|| format!("clear {}", target.display()))?;
            }
            self.subvolume(&src.join(&rel), &target)?;
        }
        if info.readonly {
            seal_tree(dst)?;
        }
        if let Some(dir) = temp {
            crate::fs::remove(&send)?;
            fs::remove_dir(&dir)?;
        }
        Ok(())
    }

    /// Create the folders of a received subvolume that `btrfs send` left out. A folder that holds a
    /// nested subvolume's place in a snapshot (a state's `.git`) is an empty folder in the source,
    /// and the send does not carry it. Nested subvolumes are left to `subvolume`.
    fn restore_folders(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            for e in fs::read_dir(src.join(&rel))? {
                let e = e?;
                if !e.file_type()?.is_dir() {
                    continue;
                }
                let child = rel.join(e.file_name());
                if managed_root(&src.join(&child)) {
                    continue;
                }
                let (s_path, d_path) = (src.join(&child), dst.join(&child));
                if fs::symlink_metadata(&d_path).is_err() {
                    fs::create_dir(&d_path).with_context(|| format!("create {}", d_path.display()))?;
                    let m = fs::symlink_metadata(&s_path)?;
                    std::os::unix::fs::lchown(&d_path, Some(m.uid()), Some(m.gid()))
                        .with_context(|| format!("owner of {}", d_path.display()))?;
                    crate::fsutil::copy_metadata(&File::open(&s_path)?, &File::open(&d_path)?)?;
                }
                stack.push(child);
            }
        }
        Ok(())
    }

    /// Reflink the files of a received subvolume that have the same extents as an earlier copy.
    /// The extents are compared with their physical addresses, so equal extents hold equal data.
    fn share_files(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            for e in fs::read_dir(src.join(&rel))? {
                let e = e?;
                let m = e.metadata()?;
                let child = rel.join(e.file_name());
                if m.is_dir() {
                    if !managed_root(&src.join(&child)) {
                        stack.push(child);
                    }
                } else if m.is_file() {
                    self.share_file(&src.join(&child), &dst.join(&child))?;
                }
            }
        }
        Ok(())
    }

    fn share_file(&mut self, src: &Path, dst: &Path) -> Result<()> {
        // Only plain extents: inline and encoded data have no physical address to compare.
        const LAST: u32 = 0x1;
        const SHARED: u32 = 0x2000;
        let extents = crate::sys::fiemap_full(&File::open(src).with_context(|| format!("open {}", src.display()))?)
            .with_context(|| format!("extents of {}", src.display()))?;
        if extents.is_empty() || extents.iter().any(|e| e.3 & !(LAST | SHARED) != 0) {
            return Ok(());
        }
        let key: Vec<(u64, u64, u64)> = extents.iter().map(|&(logical, physical, len, _)| (logical, physical, len)).collect();
        match self.seen.get(&key) {
            Some(first) => {
                let (from, to) = (File::open(first)?, fs::OpenOptions::new().write(true).open(dst)?);
                layr_fs_unix::copy_contents(&from, &to).with_context(|| format!("share {}", dst.display()))?;
            }
            None => {
                self.seen.insert(key, dst.to_path_buf());
            }
        }
        Ok(())
    }

    /// Remove the temporary snapshots.
    pub(crate) fn finish(self) -> Result<()> {
        let tmp = self.root.join("meta").join(crate::project_copy::TMP);
        if tmp.exists() {
            crate::fs::remove(&tmp)?;
        }
        Ok(())
    }
}

/// Subvolumes inside `root`, not inside another subvolume (relative paths).
fn nested_subvolumes(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in fs::read_dir(root.join(&rel))? {
            let e = e?;
            if e.metadata()?.is_dir() {
                let child = rel.join(e.file_name());
                if managed_root(&root.join(&child)) {
                    out.push(child);
                } else {
                    stack.push(child);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// btrfs has no layer registry: nothing to verify beyond the tree.
use crate::linux_status::status_stage_identity as index_identity;

fn status_evidence(
    root: &Path,
    scan: &mut std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>,
    inodes: Option<&std::collections::BTreeSet<u64>>,
) {
    if let Ok(ids) = crate::btrfs_scan::status_identities(root, inodes) {
        for entry in scan.values_mut() {
            if let crate::status_cache::FileFingerprint::Verified { stat, evidence } = entry {
                *evidence = ids.get(&stat.ino).map(|id| crate::status_cache::ObservationEvidence::Counter([id[0], id[1], 0]));
            }
        }
    }
}

pub(super) fn observe_tree(root: &Path) -> Result<std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>> {
    let mut scan = crate::linux_status::scan(root, None, &mut |p, m| crate::linux_status::native_fingerprint(&root.join(p), m))?;
    status_evidence(root, &mut scan, None);
    Ok(scan)
}

pub(super) fn observe_paths(
    root: &Path,
    paths: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>> {
    if paths.is_empty() {
        return Ok(Default::default());
    }
    let mut scan =
        crate::linux_status::scan(root, Some(paths), &mut |p, m| crate::linux_status::native_fingerprint(&root.join(p), m))?;
    let inodes = scan
        .values()
        .filter_map(|entry| match entry {
            crate::status_cache::FileFingerprint::Verified { stat, .. } => Some(stat.ino),
            crate::status_cache::FileFingerprint::Unverifiable => None,
        })
        .collect();
    status_evidence(root, &mut scan, Some(&inodes));
    Ok(scan)
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;
    use layr_objects::{fsx::EntryKind, write::write_tree_from, Keying, Store, Temperature, WriteOptions};

    #[test]
    fn fallback_keeps_independent_changes_with_colliding_subvolume_inodes() -> Result<()> {
        let tmp = std::env::temp_dir();
        if !supports_volume(&tmp) {
            return Ok(());
        }
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = crate::fs::remove(&self.0);
            }
        }
        let fixture = Fixture(tmp.join(format!("layr-unrelated-hints-{}", crate::ids::new_id())));
        fs::create_dir(&fixture.0)?;
        let (base, tree) = (fixture.0.join("base"), fixture.0.join("tree"));
        crate::fs::create(&base)?;
        crate::fs::create(&tree)?;
        fs::write(base.join("old"), b"deleted file")?;
        fs::write(tree.join("new"), b"independent addition")?;
        assert_eq!(fs::symlink_metadata(base.join("old"))?.ino(), fs::symlink_metadata(tree.join("new"))?.ino());
        assert_ne!(native_info(&base)?.uuid, native_info(&tree)?.uuid);
        // Writable unrelated roots force Partial without invoking privileged send.
        let changes = crate::changes::candidates(&base, &tree)?;
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|change| matches!(change, crate::compare::EntryChange::Remove { path, .. } if path == "old")));
        assert!(changes.iter().any(|change| matches!(change, crate::compare::EntryChange::Add { path, .. } if path == "new")));
        Ok(())
    }

    #[test]
    fn snapshot_placeholder_is_classified_at_the_driver_boundary() -> Result<()> {
        let tmp = std::env::temp_dir();
        if !supports_volume(&tmp) {
            return Ok(());
        }
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = unseal_tree(&self.0.join("snapshot"));
                let _ = crate::fs::remove(&self.0);
            }
        }
        let fixture = Fixture(tmp.join(format!("layr-placeholder-{}", crate::ids::new_id())));
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&fixture.0)?;
        let source = fixture.0.join("source");
        let saved = fixture.0.join("snapshot");
        crate::fs::create(&source)?;
        crate::fs::create(&source.join(".git"))?;
        fs::write(source.join(".git/marker"), "not in the parent snapshot")?;
        fs::write(source.join("file"), "saved content")?;
        crate::fs::snapshot(&source, &saved, true)?;
        assert_eq!(entry_kind(&source.join(".git"), &fs::symlink_metadata(source.join(".git"))?)?, EntryKind::Ordinary);
        let stub = saved.join(".git");
        assert_eq!(entry_kind(&stub, &fs::symlink_metadata(&stub)?)?, EntryKind::SubvolumePlaceholder);
        let objects = Store::open(&fixture.0.join("objects"), Keying::Plain)?;
        let snap = crate::repo::snap(&saved);
        let source = crate::objstore::SnapshotSource(&snap);
        let options = WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![], similar: false };
        let written = write_tree_from(&objects, &source, &options)?;
        let copy = fixture.0.join("copy");
        layr_objects::checkout_with(
            &objects,
            &written.tree.ok_or_else(|| anyhow::anyhow!("missing tree"))?,
            Some(&written.times.ok_or_else(|| anyhow::anyhow!("missing times"))?),
            &copy,
        )?;
        assert_eq!(layr_objects::times::ns_of(&fs::symlink_metadata(copy.join(".git"))?), 0);
        assert!(fs::read_dir(copy.join(".git"))?.next().is_none());
        layr_objects::times::set(&copy.join(".git"), 123_456_789)?;
        assert!(layr_objects::compare::compare_with_kinds(&saved, &copy, true, entry_kind)?.differences.is_empty());
        assert!(layr_objects::compare::compare_with_kinds(&copy, &saved, true, entry_kind)?.differences.is_empty());
        unseal_tree(&saved)?;
        crate::fs::remove(&fixture.0)?;
        Ok(())
    }
}

/// Native send is evidence only for sealed snapshots; failures select the shared full scan.
use crate::fs::{Accounting, Namespace, Observations, ProjectCopier, Transfer};
pub(crate) struct HostServices;
impl Namespace for HostServices {
    fn entry_kind(&self, path: &Path, metadata: &fs::Metadata) -> Result<layr_objects::fsx::EntryKind> {
        entry_kind(path, metadata)
    }
    fn supported(&self, path: &Path) -> bool {
        supports_volume(path)
    }
    fn managed(&self, path: &Path) -> bool {
        managed_root(path)
    }
    fn info(&self, path: &Path) -> Result<crate::fs::TreeInfo> {
        let info = native_info(path)?;
        Ok(crate::fs::TreeInfo {
            uuid: info.uuid,
            received_uuid: info.received_uuid,
            parent_uuid: info.parent_uuid,
            readonly: info.readonly,
        })
    }
    #[cfg(test)]
    fn protect(&self, path: crate::fs::ManagedPath, protection: crate::fs::Protection) -> Result<()> {
        match protection {
            crate::fs::Protection::Frozen => seal_tree(path.path()),
            crate::fs::Protection::Writable => unseal_tree(path.path()),
        }
    }
    fn relocate(&self, path: crate::fs::ManagedPath, to: &Path) -> Result<()> {
        move_tree(path.path(), to)
    }
    fn file_identity(&self, path: &Path) -> Result<Vec<u8>> {
        native_file_identity(path)
    }
}
impl Transfer for HostServices {
    fn send(&self, parent: Option<&Path>, child: &Path, clones: &[PathBuf], compressed: bool) -> Result<Command> {
        native_send(parent, child, clones, compressed)
    }
    fn offer(&self) -> crate::fs::StreamOffer {
        crate::fs::StreamOffer {
            compressed: compressed_input(),
            clone_lookup: if local_clone_input() {
                crate::fs::CloneLookup::LocalOrReceived
            } else {
                crate::fs::CloneLookup::ReceivedOnly
            },
        }
    }
    fn receive(
        &self,
        source: &mut dyn std::io::Read,
        destination: &Path,
        expected: &crate::receive_stream::Expect,
    ) -> Result<()> {
        crate::fs::receive_native::receive(source, destination, expected)
    }
    fn clone_identity(&self, path: &Path) -> Result<(String, Option<u64>)> {
        native_clone_identity(path)
    }
    fn send_identity(&self, path: &Path) -> Result<(String, u64)> {
        native_send_identity(path)
    }
}
impl Accounting for HostServices {
    fn line_spaces(&self, paths: &[PathBuf]) -> Result<Vec<Result<crate::fsutil::Space>>> {
        accounting::line_spaces(paths)
    }
    fn allocations(
        &self,
        root: &Path,
        parts: &[crate::fs::accounting::AllocationPart],
    ) -> Result<crate::fs::accounting::AllocationReport> {
        accounting::allocations(root, parts)
    }
    fn space(&self, path: &Path) -> Result<crate::fsutil::Space> {
        measure_space(path)
    }
    fn copy_need(&self, path: &Path) -> Result<u64> {
        copy_allocation(path)
    }
    fn copy_total(&self, path: &Path) -> Result<u64> {
        copy_flat_allocation(path)
    }
}
impl Observations for HostServices {
    fn read_policy(&self) -> crate::fs::observations::ReadPolicy {
        crate::fs::observations::ReadPolicy::Capture
    }
    fn scan(
        &self,
        path: &Path,
        paths: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>> {
        match paths {
            Some(paths) => observe_paths(path, paths),
            None => observe_tree(path),
        }
    }
    fn stage_identity(&self, path: &Path) -> Result<Vec<u8>> {
        index_identity(path)
    }
}
impl HostServices {
    pub(crate) fn copier(src: &Path, dst: &Path) -> Result<Box<dyn ProjectCopier>> {
        Ok(Box::new(Copier::new(src, dst)?))
    }
}
impl ProjectCopier for Copier {
    fn native_subvolume(&self, path: &Path) -> bool {
        Copier::native_subvolume(self, path)
    }
    fn shell_dir(&mut self, src: &Path, dst: &Path) -> Result<()> {
        Copier::shell_dir(self, src, dst)
    }
    fn dir_close(&mut self, src: &Path, dst: &Path, metadata: &fs::Metadata) -> Result<()> {
        Copier::dir_close(self, src, dst, metadata)
    }
    fn file(&mut self, src: &Path, dst: &Path, metadata: &fs::Metadata) -> Result<()> {
        Copier::file(self, src, dst, metadata)
    }
    fn symlink(&mut self, src: &Path, dst: &Path, metadata: &fs::Metadata) -> Result<()> {
        Copier::symlink(self, src, dst, metadata)
    }
    fn subvolume(&mut self, src: &Path, dst: &Path) -> Result<()> {
        Copier::subvolume(self, src, dst)
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Copier::finish(*self)
    }
}
