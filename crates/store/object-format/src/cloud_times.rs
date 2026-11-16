//! Canonical flat Cloud V1 times table.
use crate::{tree, Error, Result};

const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ns: i64,
    /// Number of descendants, or None for a file or symbolic link.
    pub below: Option<usize>,
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = vec![VERSION];
    tree::varint_into(&mut out, entries.len() as u64);
    let mut previous = 0i64;
    for entry in entries {
        tree::varint_into(&mut out, zigzag(entry.ns.wrapping_sub(previous)));
        tree::varint_into(&mut out, entry.below.map_or(0, |n| n as u64 + 1));
        previous = entry.ns;
    }
    out
}
pub fn decode(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = tree::Reader::new(bytes);
    ensure!(reader.byte()? == VERSION, Error::CloudVersion);
    let count = usize::try_from(reader.varint()?)?;
    ensure!(count > 0 && count <= bytes.len(), Error::CloudCount);
    let mut entries = Vec::with_capacity(count);
    let mut previous = 0i64;
    let mut open: Vec<usize> = Vec::new();
    for i in 0..count {
        while open.last() == Some(&i) {
            open.pop();
        }
        previous = previous.wrapping_add(unzigzag(reader.varint()?));
        let span = usize::try_from(reader.varint()?)?;
        let below = span.checked_sub(1);
        if i == 0 {
            ensure!(below == Some(count - 1), Error::CloudRoot);
        } else {
            ensure!(!open.is_empty(), Error::CloudOutsideRoot);
        }
        if let Some(n) = below {
            let end = i.checked_add(1).and_then(|v| v.checked_add(n)).filter(|end| *end <= count).ok_or(Error::CloudSpan)?;
            ensure!(open.last().is_none_or(|parent| end <= *parent), Error::CloudParentSpan);
            open.push(end);
        }
        entries.push(Entry { ns: previous, below });
    }
    ensure!(reader.done() && encode(&entries) == bytes, Error::NoncanonicalCloudTimes);
    Ok(entries)
}

/// Check a flat Cloud V1 times table before it enters the local store.
pub fn validate(bytes: &[u8]) -> Result<()> {
    decode(bytes).map(|_| ())
}
