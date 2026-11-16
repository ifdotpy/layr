//! Native stream execution and its postconditions; shared wire validation remains in receive_stream.
use crate::receive_stream::{check, Expect};
use anyhow::{anyhow, bail, Result};
use std::{
    collections::BTreeSet,
    io::Read,
    path::Path,
    process::{Command, Stdio},
};
fn names(dir: &Path) -> Result<BTreeSet<String>> {
    Ok(std::fs::read_dir(dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
}

/// Receive one checked stream into `dir`. On any failure nothing new stays in `dir`.
pub(crate) fn receive(src: &mut dyn Read, dir: &Path, e: &Expect) -> Result<()> {
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
        let info = crate::fs::host().namespace.info(&dir.join(e.name))?;
        if !info.readonly || info.received_uuid.as_deref() != Some(e.uuid) {
            bail!("incomplete receive of {}", e.name);
        }
        // `btrfs receive` found the parent by UUID: it must be the local state we checked.
        let expected_parent = match &parent {
            Some(p) => Some(crate::fs::host().namespace.info(p)?.uuid),
            None => None,
        };
        if info.parent_uuid != expected_parent {
            bail!("{} was received onto another subvolume than its parent state", e.name);
        }
        Ok(())
    })();
    if res.is_err() {
        for n in &new {
            let _ = crate::fs::remove(&dir.join(n));
        }
    }
    res
}
