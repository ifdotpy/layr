//! Receiving btrfs streams from outside this machine (peers, remote copies) with checks.
//!
//! `btrfs receive` trusts its stream: a snapshot or clone command finds its source subvolume by
//! UUID anywhere on the file system, and a clone source path is not limited to that subvolume.
//! So every stream passes through `check` on its way to `btrfs receive`:
//! - one subvolume, with the expected name and UUID;
//! - a parent only from this project (a state whose signed record names that UUID), and clones
//!   only from the new subvolume, that parent, or explicitly verified project states;
//! - relative paths without `.` or `..` components;
//! - known commands of at most 16 MiB each, and nothing after the end command.
//!
//! After the receive, the new subvolume must be read-only, carry the expected received UUID and,
//! for an incremental stream, be a snapshot of the expected local parent.

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use crate::storage as btrfs;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use anyhow::{anyhow, bail};
use anyhow::Result;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use std::process::{Command, Stdio};

pub use crate::receive_stream::Expect;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use crate::receive_stream::check;

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
fn names(dir: &Path) -> Result<BTreeSet<String>> {
    Ok(std::fs::read_dir(dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
}

/// Receive one checked stream into `dir`. On any failure nothing new stays in `dir`.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
pub fn receive(src: &mut dyn Read, dir: &Path, e: &Expect) -> Result<()> {
    let before = names(dir)?;
    // `-e`: stop at the end command.
    let mut child = Command::new("btrfs")
        .args(["receive", "-q", "-e"])
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let checked = check(src, &mut stdin, e);
    drop(stdin);
    let out = child.wait_with_output()?;
    let new: Vec<String> = names(dir)?.difference(&before).cloned().collect();
    let res = (|| -> Result<()> {
        let parent = checked?;
        if !out.status.success() {
            bail!("btrfs receive failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        if new.len() != 1 || new[0] != e.name {
            bail!("the stream did not make exactly {} (got {:?})", e.name, new);
        }
        let info = btrfs::subvol_info(&dir.join(e.name))?;
        if !info.readonly || info.received_uuid.as_deref() != Some(e.uuid) {
            bail!("incomplete receive of {}", e.name);
        }
        // `btrfs receive` found the parent by UUID: it must be the local state we checked.
        let expected_parent = match &parent {
            Some(p) => Some(btrfs::subvol_info(p)?.uuid),
            None => None,
        };
        if info.parent_uuid != expected_parent {
            bail!("{} was received onto another subvolume than its parent state", e.name);
        }
        Ok(())
    })();
    if res.is_err() {
        for n in &new {
            let _ = btrfs::delete_tree(&dir.join(n));
        }
    }
    res
}

#[cfg(any(target_os = "macos", feature = "overlayfs"))]
pub fn receive(src: &mut dyn Read, dir: &Path, e: &Expect) -> Result<()> {
    crate::apfs_stream::receive(src, dir, e)
}
