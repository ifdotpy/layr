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

pub use crate::receive_stream::Expect;
use anyhow::Result;
use std::{io::Read, path::Path};
pub fn receive(source: &mut dyn Read, destination: &Path, expected: &Expect) -> Result<()> {
    crate::fs::host().transfer.receive(source, destination, expected)
}
