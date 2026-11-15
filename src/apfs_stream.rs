//! APFS transport using the existing btrfs v1 wire format.
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

use layr_stream_codec::{CommandHeader, DecodeLimits, ReceiveDecoder, Version};

fn command(out: &mut dyn Write, code: u16, attrs: &[(u16, &[u8])]) -> Result<()> {
    out.write_all(&layr_stream_codec::encode_command(code, attrs, DecodeLimits::default())?)?;
    Ok(())
}
fn time(sec: i64, nano: i64) -> Vec<u8> {
    let mut v = sec.to_le_bytes().to_vec();
    v.extend((nano as u32).to_le_bytes());
    v
}
/// Send the immutable tree `root`. With a `parent` that the receiver has (`parent_uuid` names
/// it), the stream holds only the differences; a tree with hard links is sent complete.
pub fn send(root: &Path, parent: Option<(&Path, &str)>, out: &mut dyn Write) -> Result<()> {
    let info = crate::fs::host().namespace.info(root)?;
    if !info.readonly {
        bail!("only immutable snapshots can be sent");
    }
    let uuid = uuid::Uuid::parse_str(info.received_uuid.as_deref().unwrap_or(&info.uuid))?;
    let name = root.file_name().context("snapshot has no name")?.as_bytes();
    let plan = match parent {
        Some((p, id)) => plan(p, root)?.map(|plan| (uuid::Uuid::parse_str(id), plan)),
        None => None,
    };
    out.write_all(b"btrfs-stream\0\x01\0\0\0")?;
    match plan {
        Some((parent_uuid, plan)) => {
            let parent_uuid = parent_uuid?;
            command(
                out,
                2,
                &[
                    (15, name),
                    (1, uuid.as_bytes()),
                    (2, &1u64.to_le_bytes()),
                    (20, parent_uuid.as_bytes()),
                    (21, &1u64.to_le_bytes()),
                ],
            )?;
            send_plan(root, &plan, out)?;
        }
        _ => {
            command(out, 1, &[(15, name), (1, uuid.as_bytes()), (2, &1u64.to_le_bytes())])?;
            visit(root, Path::new(""), out, &mut BTreeMap::new())?;
        }
    }
    command(out, 21, &[])
}

fn visit(root: &Path, rel: &Path, out: &mut dyn Write, links: &mut BTreeMap<(u64, u64), Vec<u8>>) -> Result<()> {
    let full = root.join(rel);
    let m = fs::symlink_metadata(&full)?;
    let p = rel.as_os_str().as_bytes();
    let mut linked = false;
    if !p.is_empty() {
        if m.is_file() {
            let key = (m.dev(), m.ino());
            if let Some(first) = links.get(&key) {
                command(out, 10, &[(15, p), (17, first)])?;
                linked = true;
            } else {
                links.insert(key, p.to_vec());
                create(&full, p, &m, out)?;
            }
        } else {
            create(&full, p, &m, out)?;
        }
    }
    if m.is_dir() {
        for entry in fs::read_dir(&full)? {
            visit(root, &rel.join(entry?.file_name()), out, links)?;
        }
    }
    if !linked {
        metadata(&full, p, &m, &[], out)?;
    }
    Ok(())
}

/// The commands that make the entry `p` (at `full`) with its content.
fn create(full: &Path, p: &[u8], m: &fs::Metadata, out: &mut dyn Write) -> Result<()> {
    if crate::stream_fs::has_acl(full)? {
        bail!("cannot transfer macOS access control lists to btrfs: {}", full.display());
    }
    let ino = m.ino().to_le_bytes();
    if m.is_dir() {
        command(out, 4, &[(15, p), (3, &ino)])?;
    } else if m.file_type().is_symlink() {
        command(out, 8, &[(15, p), (3, &ino), (17, fs::read_link(full)?.as_os_str().as_bytes())])?;
    } else if m.is_file() {
        command(out, 3, &[(15, p), (3, &ino)])?;
        let mut f = File::open(full)?;
        let mut buf = vec![0; 48 * 1024];
        let mut offset = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if buf[..n].iter().any(|b| *b != 0) {
                command(out, 15, &[(15, p), (18, &offset.to_le_bytes()), (19, &buf[..n])])?;
            }
            offset += n as u64;
        }
        command(out, 17, &[(15, p), (4, &m.len().to_le_bytes())])?;
    } else {
        bail!("cannot send special file {}", full.display());
    }
    Ok(())
}

/// The attributes of an entry as the stream names them, without Layr's own markers.
fn stream_xattrs(full: &Path, m: &fs::Metadata) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    for (name, value) in crate::stream_fs::xattrs(full)? {
        if name == b"com.layr.subvolume"
            || name == b"com.layr.origin"
            || name == b"trusted.layr.origin"
            || name == b"trusted.layr.cache-restore"
        {
            continue;
        }
        // Linux forbids user.* attributes on symlinks. Its root receiver can
        // retain them in trusted.*, including attributes specific to macOS.
        #[cfg(target_os = "macos")]
        let name = if m.file_type().is_symlink() {
            format!("trusted.layr.apfs.{}", hex::encode(name)).into_bytes()
        } else if name.starts_with(b"user.") {
            name
        } else {
            format!("user.layr.apfs.{}", hex::encode(name)).into_bytes()
        };
        #[cfg(feature = "overlayfs")]
        if !name.starts_with(b"user.") {
            bail!("cannot transfer attribute {}", String::from_utf8_lossy(&name));
        }
        let _ = m;
        out.push((name, value));
    }
    Ok(out)
}

/// Attributes, mode and times of an entry. `gone` names attributes that the receiver has and
/// the entry no longer has.
fn metadata(full: &Path, p: &[u8], m: &fs::Metadata, gone: &[Vec<u8>], out: &mut dyn Write) -> Result<()> {
    if crate::stream_fs::has_acl(full)? {
        bail!("cannot transfer macOS access control lists to btrfs: {}", full.display());
    }
    for name in gone {
        command(out, 14, &[(15, p), (13, name)])?;
    }
    for (name, value) in stream_xattrs(full, m)? {
        command(out, 13, &[(15, p), (13, &name), (14, &value)])?;
    }
    if !m.file_type().is_symlink() {
        command(out, 18, &[(15, p), (5, &((m.mode() & 0o7777) as u64).to_le_bytes())])?;
    }
    command(
        out,
        20,
        &[
            (15, p),
            (9, &time(m.ctime(), m.ctime_nsec())),
            (10, &time(m.mtime(), m.mtime_nsec())),
            (11, &time(m.atime(), m.atime_nsec())),
        ],
    )
}

/// The differences from a parent tree to a child tree, in stream order.
struct Plan {
    /// Paths to remove (and whether each is a folder), content before its folder.
    remove: Vec<(String, bool)>,
    /// Paths to make with their content, folders before their content.
    create: Vec<String>,
    /// Paths whose attributes, mode or times to send, with the attributes that went away.
    meta: Vec<(String, Vec<Vec<u8>>)>,
}

/// Request complete state evidence from the same reader comparison used by save/status.
/// Native codecs execute the resulting plan; backend identity never selects the algorithm.
fn plan(parent: &Path, child: &Path) -> Result<Option<Plan>> {
    if !parent.is_dir() || !crate::fs::host().namespace.info(parent).is_ok_and(|info| info.readonly) {
        return Ok(None);
    }
    let before = crate::source::Source::native(parent);
    let after = crate::source::Source::native(child);
    let changes = crate::changes::state_changes(&before, &after)?;
    plan_from_changes(parent, child, changes)
}
fn plan_from_changes(parent: &Path, child: &Path, changes: Vec<crate::compare::EntryChange>) -> Result<Option<Plan>> {
    use crate::fsutil::FType;
    use layr_fs_core::{RelDir, RelPath, TreeRead};
    let before = crate::source::Source::native(parent);
    let after = crate::source::Source::native(child);
    let changes = crate::changes::without_moves(changes);
    let mut remove = BTreeMap::new();
    let mut create = std::collections::BTreeSet::new();
    let mut attributes = std::collections::BTreeSet::from([String::new()]);
    for change in &changes {
        let relative = RelPath::new(change.path().as_bytes())?;
        let at = RelDir::Path(relative.clone());
        for metadata in [before.metadata(&at)?, after.metadata(&at)?].into_iter().flatten() {
            // Unchanged groups outside complete evidence remain in the received parent.
            // A touched link group needs the inode map of a full stream.
            if metadata.kind == FType::File && metadata.link.is_some_and(|link| link.count > 1) {
                return Ok(None);
            }
        }
        let renew = match (change.old(), change.new()) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(old), Some(new)) if old.ftype != new.ftype => true,
            (Some(old), Some(new)) if old.is_file() => {
                !(old.size == new.size && matches!(change.data(), crate::compare::DataEvidence::Equal))
                    && !crate::fsutil::same_entry_observed((&before, change.path(), old), (&after, change.path(), new), None)?
            }
            (Some(_), Some(new)) if new.ftype == FType::Symlink => before.read_link(&relative)? != after.read_link(&relative)?,
            _ => false,
        };
        if change.old().is_some() && (change.new().is_none() || renew) {
            if change.old().is_some_and(|metadata| metadata.is_dir()) {
                layr_fs_common::walk(&before, &at, &mut |path, metadata| {
                    remove.insert(path.utf8()?.to_owned(), metadata.kind == FType::Dir);
                    Ok(true)
                })?;
            }
            remove.insert(change.path().clone(), change.old().is_some_and(|metadata| metadata.is_dir()));
        }
        if renew {
            create.insert(change.path().clone());
        }
        if change.new().is_some() {
            attributes.insert(change.path().clone());
        }
    }
    // Namespace edits change directory times. Restore every surviving ancestor, including
    // ancestors absent from the data candidates supplied by a native hint.
    for path in remove.keys().chain(&create) {
        let mut parent = path.as_str();
        while let Some((ancestor, _)) = parent.rsplit_once('/') {
            attributes.insert(ancestor.to_owned());
            parent = ancestor;
        }
    }
    let xattrs = |path: &Path| -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        Ok(stream_xattrs(path, &fs::symlink_metadata(path)?)?.into_iter().collect())
    };
    let mut meta = Vec::new();
    for path in attributes {
        if after.kind(&RelDir::from_bytes(path.as_bytes())?)?.is_none() {
            continue;
        }
        let new = xattrs(&child.join(&path))?;
        let old = if !create.contains(&path) && before.kind(&RelDir::from_bytes(path.as_bytes())?)?.is_some() {
            xattrs(&parent.join(&path))?
        } else {
            BTreeMap::new()
        };
        meta.push((path, old.keys().filter(|name| !new.contains_key(*name)).cloned().collect()));
    }
    Ok(Some(Plan { remove: remove.into_iter().rev().collect(), create: create.into_iter().collect(), meta }))
}

fn send_plan(root: &Path, plan: &Plan, out: &mut dyn Write) -> Result<()> {
    for (rel, dir) in &plan.remove {
        command(out, if *dir { 12 } else { 11 }, &[(15, rel.as_bytes())])?;
    }
    for rel in &plan.create {
        let full = root.join(rel);
        create(&full, rel.as_bytes(), &fs::symlink_metadata(&full)?, out)?;
    }
    for (rel, gone) in &plan.meta {
        let full = root.join(rel);
        metadata(&full, rel.as_bytes(), &fs::symlink_metadata(&full)?, gone, out)?;
    }
    Ok(())
}

fn path(root: &Path, bytes: &[u8], follow: bool) -> Result<std::path::PathBuf> {
    let rel = std::str::from_utf8(bytes)?;
    let p = crate::fsutil::safe_join(root, rel)?;
    if follow && fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("stream operation follows a symbolic link");
    }
    Ok(p)
}
/// Validate before applying. All writes stay in a private temporary directory.
/// Only a complete, sealed tree becomes visible under its state name.
pub fn receive(src: &mut dyn Read, dir: &Path, expected: &crate::receive_stream::Expect) -> Result<()> {
    use std::{
        io::{Seek, SeekFrom},
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let staging = dir.join(format!(".receive-{}", crate::ids::new_id()));
    let staging_owner = crate::repo::Temp::directory(staging.clone(), 0o700)?;
    let result = (|| -> Result<()> {
        let mut spool =
            fs::OpenOptions::new().create_new(true).read(true).write(true).mode(0o600).open(staging.join("stream"))?;
        let parent = crate::receive_stream::check(src, &mut spool, expected)?;
        spool.seek(SeekFrom::Start(0))?;
        let root = dir.join(format!(".receive-tree-{}", crate::ids::new_id()));
        let mut head = [0; 17];
        spool.read_exact(&mut head)?;
        let version = Version::decode(&head)?;
        if version.number() > 2 {
            bail!("portable receive supports send stream versions 1 and 2");
        }
        let working = match &parent {
            Some(parent) => crate::repo::Temp::capture(
                parent,
                root.clone(),
                crate::fs::Protection::Writable,
                crate::fs::CapturePurpose::State,
            )?,
            None => crate::repo::Temp::empty(root.clone())?,
        };
        let replay = (|| -> Result<_> {
            // Delay permissions until all content is in place. Key by inode to survive renames.
            let mut modes = BTreeMap::new();
            fn prepare(p: &Path, modes: &mut BTreeMap<u64, u32>) -> Result<()> {
                let m = fs::symlink_metadata(p)?;
                if m.file_type().is_symlink() {
                    return Ok(());
                }
                modes.insert(m.ino(), m.mode() & 0o7777);
                fs::set_permissions(p, fs::Permissions::from_mode((m.mode() & 0o7777) | 0o700))?;
                if m.is_dir() {
                    for e in fs::read_dir(p)? {
                        prepare(&e?.path(), modes)?;
                    }
                }
                Ok(())
            }
            prepare(&root, &mut modes)?;
            let mut times: BTreeMap<u64, (i64, i64)> = BTreeMap::new();
            let limits = DecodeLimits::default();
            let mut decoder = ReceiveDecoder::new(version, limits);
            loop {
                let mut hdr = [0; 10];
                spool.read_exact(&mut hdr)?;
                let frame = CommandHeader::decode(&hdr, limits)?;
                let mut body = vec![0; frame.body_len()];
                spool.read_exact(&mut body)?;
                let command = decoder.decode(frame, &body)?;
                let code = command.code();
                if code == 1 || code == 2 {
                    continue;
                }
                if code == 21 {
                    decoder.finish()?;
                    break;
                }
                let p = path(&root, command.value(15)?, !matches!(code, 8..=14 | 19 | 20))?;
                // On a volume that ignores case, README and Readme are one entry: refuse instead of
                // failing on an unclear error or replacing the other entry.
                let clash = |q: &Path| -> Result<()> {
                    if let Some(stored) = crate::fsutil::case_clash(q) {
                        bail!(
                        "{} and {stored} differ only in case, and this volume does not tell them apart; use a case-sensitive APFS volume for the store",
                        q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    );
                    }
                    Ok(())
                };
                if matches!(code, 3 | 4 | 8 | 10) {
                    clash(&p)?;
                }
                match code {
                    3 => {
                        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p)?;
                    }
                    4 => {
                        fs::create_dir(&p)?;
                    }
                    8 => {
                        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(command.value(17)?), &p)?;
                    }
                    9 => {
                        let to = path(&root, command.value(16)?, false)?;
                        // A rename of an entry to another case of its own name is no clash: the
                        // entry that `to` finds is `p` itself, in the same folder.
                        let itself = to.parent() == p.parent()
                            && crate::fsutil::case_clash(&to).is_some_and(|s| p.file_name() == Some(s.as_ref()));
                        if !itself {
                            clash(&to)?;
                        }
                        fs::rename(&p, to)?;
                    }
                    10 => {
                        let from = path(&root, command.value(17)?, true)?;
                        if !fs::symlink_metadata(&from)?.is_file() {
                            bail!("hardlink source is not a file");
                        }
                        fs::hard_link(from, &p)?;
                    }
                    11 => fs::remove_file(&p)?,
                    12 => fs::remove_dir(&p)?,
                    13 | 14 => {
                        let raw = command.value(13)?;
                        if raw.starts_with(b"system.posix_acl_") || raw.starts_with(b"security.") {
                            bail!("APFS cannot enforce the Linux security attribute {}", String::from_utf8_lossy(raw));
                        }
                        #[cfg(target_os = "macos")]
                        let name = if let Some(encoded) =
                            raw.strip_prefix(b"user.layr.apfs.").or_else(|| raw.strip_prefix(b"trusted.layr.apfs."))
                        {
                            hex::decode(encoded)?
                        } else {
                            raw.to_vec()
                        };
                        #[cfg(feature = "overlayfs")]
                        let name = {
                            if !raw.starts_with(b"user.") {
                                bail!("unsupported portable attribute namespace");
                            }
                            raw.to_vec()
                        };
                        if name == b"com.layr.subvolume"
                            || name == b"com.layr.origin"
                            || name == b"trusted.layr.origin"
                            || name == b"trusted.layr.cache-restore"
                        {
                            bail!("stream cannot set internal APFS metadata");
                        }
                        let name = std::ffi::CString::new(name)?;
                        if code == 13 {
                            crate::stream_fs::attribute(&p, &name, Some(command.value(14)?))?;
                        } else {
                            crate::stream_fs::remove_attribute(&p, &name)?;
                        }
                    }
                    15 => {
                        let mut f = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&p)?;
                        f.seek(SeekFrom::Start(command.number(18)?))?;
                        f.write_all(command.value(19)?)?;
                    }
                    16 => {
                        let source_id = uuid::Uuid::from_slice(command.value(20)?)?.to_string();
                        let source_root = if source_id == expected.uuid {
                            root.clone()
                        } else if let Some(p) = (expected.parent)(&source_id) {
                            p
                        } else {
                            expected
                                .clones
                                .iter()
                                .find(|(u, _, _)| *u == source_id)
                                .map(|(_, _, p)| p.clone())
                                .context("clone source unavailable")?
                        };
                        let from = path(&source_root, command.value(22)?, true)?;
                        if !fs::metadata(&from)?.is_file() {
                            bail!("clone source is not a file");
                        }
                        let mut input = File::open(from)?;
                        let mut output = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&p)?;
                        let len = command.number(24)?;
                        let start = command.number(23)?;
                        if start.checked_add(len).is_none_or(|end| end > input.metadata().map(|m| m.len()).unwrap_or(0)) {
                            bail!("clone range exceeds source");
                        }
                        input.seek(SeekFrom::Start(start))?;
                        output.seek(SeekFrom::Start(command.number(18)?))?;
                        if std::io::copy(&mut input.take(len), &mut output)? != len {
                            bail!("short clone source");
                        }
                    }
                    17 => {
                        fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&p)?.set_len(command.number(4)?)?
                    }
                    18 => {
                        modes.insert(fs::symlink_metadata(&p)?.ino(), (command.number(5)? & 0o7777) as u32);
                    }
                    // Numeric owners do not identify the same user on two machines.
                    // APFS stores belong to the local user; worktree access is applied by layr.
                    19 => {}
                    20 => {
                        let t = command.value(10)?;
                        let sec = i64::from_le_bytes(t[..8].try_into()?);
                        let ns = u32::from_le_bytes(t[8..].try_into()?);
                        times.insert(fs::symlink_metadata(&p)?.ino(), (sec, ns as i64));
                    }
                    _ => bail!("portable receive cannot apply stream command {code}"),
                }
            }
            fn finish(p: &Path, modes: &BTreeMap<u64, u32>, times: &BTreeMap<u64, (i64, i64)>) -> Result<()> {
                let m = fs::symlink_metadata(p)?;
                if m.is_dir() {
                    for e in fs::read_dir(p)? {
                        finish(&e?.path(), modes, times)?;
                    }
                }
                if let Some(&(s, n)) = times.get(&m.ino()) {
                    crate::stream_fs::mtime(p, s, n)?;
                }
                if !m.file_type().is_symlink() {
                    if let Some(&mode) = modes.get(&m.ino()) {
                        fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
                    }
                }
                Ok(())
            }
            finish(&root, &modes, &times)?;
            let target = dir.join(expected.name);
            let captured = crate::fs::freeze_path_owned(&root, &target, crate::fs::CapturePurpose::State)?;
            captured.received_identity(expected.uuid, 0)
        })();
        match replay {
            Ok(owner) => match working.discard() {
                Ok(()) => {
                    owner.release();
                    Ok(())
                }
                Err(error) => Err(owner.abort(error)),
            },
            Err(error) => Err(working.abort(error)),
        }
    })();
    match result {
        Ok(()) => staging_owner.discard(),
        Err(error) => Err(staging_owner.abort(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Sandbox(std::path::PathBuf);
    impl Sandbox {
        fn plain() -> Result<Self> {
            let root = std::env::temp_dir().join(format!("stream-plan-{}", crate::ids::new_id()));
            fs::create_dir(&root)?;
            Ok(Self(root))
        }
        fn new() -> Self {
            let key = if cfg!(feature = "overlayfs") { "LAYR_OVERLAYFS_TEST_ROOT" } else { "LAYR_APFS_TEST_ROOT" };
            let p = std::path::PathBuf::from(std::env::var_os(key).expect("backend test root"))
                .join(format!("stream-test-{}", crate::ids::new_id()));
            fs::create_dir(&p).unwrap();
            #[cfg(feature = "overlayfs")]
            crate::overlayfs::recover(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = crate::fs::remove(&self.0);
        }
    }
    #[test]
    fn incremental_plan_preserves_namespace_attribute_removals_and_ancestor_times() -> Result<()> {
        let sandbox = Sandbox::plain()?;
        let parent = sandbox.0.join("before");
        let child = sandbox.0.join("after");
        fs::create_dir_all(parent.join("dir/replace/nested"))?;
        fs::create_dir_all(child.join("dir"))?;
        fs::write(parent.join("dir/replace/nested/file"), b"old")?;
        fs::write(child.join("dir/replace"), b"replacement")?;
        fs::write(parent.join("kept"), b"same")?;
        fs::write(child.join("kept"), b"same")?;
        crate::stream_fs::attribute(&parent.join("kept"), c"user.layr.gone", Some(b"old"))?;
        let before = crate::source::Source::native(&parent);
        let after = crate::source::Source::native(&child);
        let changes = layr_fs_common::candidates(&before, &after)?;
        let plan = plan_from_changes(&parent, &child, changes)?.expect("ordinary files support an incremental plan");
        assert_eq!(
            plan.remove,
            vec![("dir/replace/nested/file".into(), false), ("dir/replace/nested".into(), true), ("dir/replace".into(), true)]
        );
        assert_eq!(plan.create, vec!["dir/replace"]);
        assert!(plan.meta.contains(&("kept".into(), vec![b"user.layr.gone".to_vec()])));
        assert!(plan.meta.iter().any(|(path, _)| path.is_empty()));
        assert!(plan.meta.iter().any(|(path, _)| path == "dir"));
        Ok(())
    }
    #[test]
    fn incremental_plan_requests_full_inode_map_for_touched_hard_links() -> Result<()> {
        let sandbox = Sandbox::plain()?;
        let parent = sandbox.0.join("before");
        let child = sandbox.0.join("after");
        fs::create_dir(&parent)?;
        fs::create_dir(&child)?;
        fs::write(child.join("one"), b"linked")?;
        fs::hard_link(child.join("one"), child.join("two"))?;
        let changes =
            layr_fs_common::candidates(&crate::source::Source::native(&parent), &crate::source::Source::native(&child))?;
        assert!(plan_from_changes(&parent, &child, changes)?.is_none());
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn transport_preserves_content_and_rejects_damage() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("state");
        crate::fs::create(&src)?;
        fs::create_dir(src.join("dir"))?;
        fs::write(src.join("dir/f"), b"content")?;
        fs::hard_link(src.join("dir/f"), src.join("hard"))?;
        std::os::unix::fs::symlink("dir/f", src.join("link"))?;
        fs::write(src.join("sparse"), vec![0; 200_000])?;
        crate::stream_fs::attribute(&src.join("dir/f"), c"user.layr.test", Some(b"metadata"))?;
        fs::set_permissions(src.join("dir/f"), fs::Permissions::from_mode(0o444))?;
        crate::fs::set_readonly(&src)?;
        let id = crate::fs::host().namespace.info(&src)?.uuid;
        let mut bytes = Vec::new();
        send(&src, None, &mut bytes)?;
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let e = crate::receive_stream::Expect { name: "state", uuid: &id, parent: &|_| None, clones: &[] };
        receive(&mut &bytes[..], &dir, &e)?;
        let dst = dir.join("state");
        assert_eq!(fs::read(dst.join("dir/f"))?, b"content");
        assert_eq!(fs::metadata(dst.join("dir/f"))?.ino(), fs::metadata(dst.join("hard"))?.ino());
        assert_eq!(fs::read_link(dst.join("link"))?, Path::new("dir/f"));
        assert_eq!(fs::metadata(dst.join("dir/f"))?.mode() & 0o777, 0o444);
        assert_eq!(fs::read(dst.join("sparse"))?, vec![0; 200_000]);
        assert_eq!(crate::stream_fs::attribute(&dst.join("dir/f"), c"user.layr.test", None)?, b"metadata");
        assert!(fs::write(dst.join("dir/f"), b"bad").is_err());
        assert!(receive(&mut &bytes[..], &dir, &e).is_err());
        assert_eq!(fs::read(dst.join("dir/f"))?, b"content");
        let bad = s.0.join("bad");
        fs::create_dir(&bad)?;
        bytes[23] ^= 1;
        assert!(receive(&mut &bytes[..], &bad, &e).is_err());
        assert_eq!(fs::read_dir(&bad)?.count(), 0);
        Ok(())
    }
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn a_stream_against_a_parent_holds_only_the_differences() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let s = Sandbox::new();
        let parent = s.0.join("parent");
        crate::fs::create(&parent)?;
        for i in 0..200 {
            fs::create_dir_all(parent.join(format!("d{}", i % 5)))?;
            fs::write(parent.join(format!("d{}/f{i}", i % 5)), format!("{i}\n").repeat(500))?;
        }
        fs::create_dir_all(parent.join("gone/inner"))?;
        fs::write(parent.join("gone/inner/x"), b"x")?;
        std::os::unix::fs::symlink("d0/f0", parent.join("link"))?;
        fs::write(parent.join("swap"), b"file")?;
        crate::fs::set_readonly(&parent)?;
        let child = s.0.join("child");
        crate::fs::snapshot(&parent, &child, false)?;
        fs::write(child.join("d1/f1"), b"changed")?;
        fs::remove_file(child.join("d2/f2"))?;
        fs::create_dir(child.join("new"))?;
        fs::write(child.join("new/file"), b"new")?;
        crate::stream_fs::attribute(&child.join("new/file"), c"user.layr.test", Some(b"v"))?;
        fs::set_permissions(child.join("d3/f3"), fs::Permissions::from_mode(0o755))?;
        fs::remove_dir_all(child.join("gone"))?;
        fs::remove_file(child.join("link"))?;
        std::os::unix::fs::symlink("d0/f5", child.join("link"))?;
        fs::remove_file(child.join("swap"))?;
        fs::create_dir(child.join("swap"))?;
        crate::fs::set_readonly(&child)?;
        let parent_id = crate::fs::host().namespace.info(&parent)?.uuid;
        let id = crate::fs::host().namespace.info(&child)?.uuid;
        let (mut full, mut diff) = (Vec::new(), Vec::new());
        send(&child, None, &mut full)?;
        send(&child, Some((&parent, &parent_id)), &mut diff)?;
        assert!(diff.len() * 20 < full.len(), "{} of {} bytes", diff.len(), full.len());
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let find = |u: &str| (u == parent_id).then(|| parent.clone());
        let e = crate::receive_stream::Expect { name: "child", uuid: &id, parent: &find, clones: &[] };
        receive(&mut &diff[..], &dir, &e)?;
        let got = dir.join("child");
        let tree = |root: &Path| -> Result<BTreeMap<String, (u32, Vec<u8>)>> {
            let mut out = BTreeMap::new();
            crate::fsutil::walk(root, "", &mut |rel, m| {
                let p = root.join(rel);
                let body = if m.is_file() {
                    fs::read(&p)?
                } else if m.ftype == crate::fsutil::FType::Symlink {
                    fs::read_link(&p)?.into_os_string().into_vec()
                } else {
                    Vec::new()
                };
                out.insert(rel.to_string(), (m.mode, body));
                Ok(true)
            })?;
            Ok(out)
        };
        assert_eq!(tree(&got)?, tree(&child)?);
        assert_eq!(crate::stream_fs::attribute(&got.join("new/file"), c"user.layr.test", None)?, b"v");
        Ok(())
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn transfer_refuses_acl_instead_of_losing_access_rules() -> Result<()> {
        let s = Sandbox::new();
        let root = s.0.join("state");
        crate::fs::create(&root)?;
        fs::write(root.join("file"), b"private")?;
        let result =
            std::process::Command::new("/bin/chmod").args(["+a", "everyone deny read"]).arg(root.join("file")).status()?;
        assert!(result.success());
        assert!(crate::stream_fs::has_acl(&root.join("file"))?);
        crate::fs::set_readonly(&root)?;
        let error = send(&root, None, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("access control lists"));
        crate::fs::set_writable(&root)?;
        crate::sys::apfs_clear_acl(&root.join("file"))?;
        assert!(!crate::stream_fs::has_acl(&root.join("file"))?);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn receive_uses_only_an_authorized_external_clone() -> Result<()> {
        let s = Sandbox::new();
        let source = s.0.join("source");
        crate::fs::create(&source)?;
        fs::write(source.join("file"), b"source data")?;
        crate::fs::set_readonly(&source)?;
        let source_id = crate::fs::host().namespace.info(&source)?.uuid;
        let source_uuid = uuid::Uuid::parse_str(&source_id)?;
        let id = uuid::Uuid::now_v7();
        let text = id.to_string();
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let mut bytes = b"btrfs-stream\0\x01\0\0\0".to_vec();
        command(&mut bytes, 1, &[(15, b"state"), (1, id.as_bytes()), (2, &1u64.to_le_bytes())])?;
        command(&mut bytes, 3, &[(15, b"file"), (3, &1u64.to_le_bytes())])?;
        command(
            &mut bytes,
            16,
            &[
                (15, b"file"),
                (18, &0u64.to_le_bytes()),
                (20, source_uuid.as_bytes()),
                (21, &7u64.to_le_bytes()),
                (22, b"file"),
                (23, &0u64.to_le_bytes()),
                (24, &11u64.to_le_bytes()),
            ],
        )?;
        command(&mut bytes, 21, &[])?;
        let e = crate::receive_stream::Expect { name: "state", uuid: &text, parent: &|_| None, clones: &[] };
        assert!(receive(&mut &bytes[..], &dir, &e).is_err());
        assert_eq!(fs::read_dir(&dir)?.count(), 0);
        let clones = [(source_id, None, source)];
        let e = crate::receive_stream::Expect { clones: &clones, ..e };
        receive(&mut &bytes[..], &dir, &e)?;
        assert_eq!(fs::read(dir.join("state/file"))?, b"source data");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn receive_cannot_write_through_a_symlink() -> Result<()> {
        let s = Sandbox::new();
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let outside = s.0.join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("keep"), b"keep")?;
        let id = uuid::Uuid::now_v7();
        let text = id.to_string();
        let e = crate::receive_stream::Expect { name: "state", uuid: &text, parent: &|_| None, clones: &[] };
        for (code, attrs) in [
            (3, vec![(15, b"link/new".as_slice()), (3, b"\x01\0\0\0\0\0\0\0".as_slice())]),
            (15, vec![(15, b"link/keep".as_slice()), (18, b"\0\0\0\0\0\0\0\0".as_slice()), (19, b"bad".as_slice())]),
        ] {
            let mut bytes = b"btrfs-stream\0\x01\0\0\0".to_vec();
            command(&mut bytes, 1, &[(15, b"state"), (1, id.as_bytes()), (2, &1u64.to_le_bytes())])?;
            command(&mut bytes, 8, &[(15, b"link"), (3, &1u64.to_le_bytes()), (17, outside.as_os_str().as_bytes())])?;
            command(&mut bytes, code, &attrs)?;
            command(&mut bytes, 21, &[])?;
            assert!(receive(&mut &bytes[..], &dir, &e).is_err());
            assert_eq!(fs::read_dir(&dir)?.count(), 0);
            assert_eq!(fs::read(outside.join("keep"))?, b"keep");
            assert!(!outside.join("new").exists());
        }
        Ok(())
    }
}
