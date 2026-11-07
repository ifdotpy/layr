//! The times table of a state (docs/objects.md, "Times"): the modification time of every entry,
//! outside the tree, so equal folders keep equal tree ids and a checkout still gets the times
//! that build tools compare.
//!
//! ```text
//! times  = version:u8 (1)  count:varint  record*
//! record = delta:zigzag-varint  span:varint
//! ```
//!
//! Records are in tree order: the root folder first, then the entries of each folder sorted by
//! name bytes, each folder's own entries right after it (pre-order). `delta` is the modification
//! time in nanoseconds since the Unix epoch minus the time of the previous record (the first:
//! minus 0), zigzag-encoded. `span` is 0 for a file or a symbolic link and 1 + the number of
//! records below a folder for a folder, so a reader can skip a folder without its tree (an
//! incremental write copies the records of unchanged folders). Equal times cost two bytes per
//! entry.

use crate::tree::{varint_into, Reader};
use anyhow::{bail, ensure, Result};

const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub ns: i64,
    /// `Some(n)`: a folder with `n` records below it.
    pub below: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Times {
    pub records: Vec<Record>,
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

impl Times {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        varint_into(&mut out, self.records.len() as u64);
        let mut prev = 0i64;
        for r in &self.records {
            varint_into(&mut out, zigzag(r.ns.wrapping_sub(prev)));
            varint_into(&mut out, r.below.map_or(0, |n| n + 1));
            prev = r.ns;
        }
        out
    }

    /// Decode and check the folder spans: the root is a folder that spans every other record, and
    /// each folder lies inside its parent.
    pub fn decode(bytes: &[u8]) -> Result<Times> {
        let mut r = Reader::new(bytes);
        ensure!(r.byte()? == VERSION, "unknown times format version");
        let n = r.varint()?;
        ensure!(n >= 1 && n <= bytes.len() as u64, "a times table has a wrong count");
        let mut records = Vec::with_capacity(n as usize);
        let mut prev = 0i64;
        // The end (exclusive) of each open folder.
        let mut open: Vec<u64> = Vec::new();
        for i in 0..n {
            while open.last() == Some(&i) {
                open.pop();
            }
            prev = prev.wrapping_add(unzigzag(r.varint()?));
            let below = match r.varint()? {
                0 => None,
                k => Some(k - 1),
            };
            if i == 0 {
                ensure!(below == Some(n - 1), "the root of a times table must span every record");
            } else {
                ensure!(!open.is_empty(), "a times record lies outside the root");
            }
            if let Some(k) = below {
                let end = i
                    .checked_add(1 + k)
                    .filter(|end| *end <= n)
                    .ok_or_else(|| anyhow::anyhow!("a folder span leaves the table"))?;
                ensure!(open.last().is_none_or(|parent| end <= *parent), "a folder span leaves its parent");
                open.push(end);
            }
            records.push(Record { ns: prev, below });
        }
        if !r.done() {
            bail!("bytes after the times table");
        }
        Ok(Times { records })
    }
}

/// Nanoseconds since the epoch of a modification time.
pub fn ns_of(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime().wrapping_mul(1_000_000_000).wrapping_add(meta.mtime_nsec())
}

/// Set the modification time of `path` (not followed if it is a link); the access time stays.
pub fn set(path: &std::path::Path, ns: i64) -> std::io::Result<()> {
    let ts = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT },
        last_modification: rustix::fs::Timespec {
            tv_sec: ns.div_euclid(1_000_000_000) as _,
            tv_nsec: ns.rem_euclid(1_000_000_000) as _,
        },
    };
    Ok(rustix::fs::utimensat(rustix::fs::CWD, path, &ts, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(ns: i64) -> Record {
        Record { ns, below: None }
    }
    fn dir(ns: i64, below: u64) -> Record {
        Record { ns, below: Some(below) }
    }

    #[test]
    fn times_round_trip_compactly() {
        let base = 1_794_000_000_123_456_789i64;
        let t = Times {
            records: vec![dir(base, 6), file(base), dir(base + 1, 2), file(-1), file(i64::MAX), file(i64::MIN), file(0)],
        };
        assert_eq!(Times::decode(&t.encode()).unwrap(), t);
        let mut same = vec![dir(base, 999)];
        same.extend((0..999).map(|_| file(base)));
        assert!(Times { records: same }.encode().len() < 2000 + 16, "equal times cost two bytes each");
        // The encoding is fixed: version, count, then (zigzag delta, span) per record.
        assert_eq!(Times { records: vec![dir(1, 2), file(0), file(3)] }.encode(), vec![1, 3, 2, 3, 1, 0, 6, 0]);
        for bad in [
            vec![1, 2, 0, 3, 0],          // the root has no span over the second record
            vec![1, 2, 0, 3],             // cut short
            vec![1, 1, 0, 1, 0],          // bytes after the end
            vec![2, 1, 0, 1],             // unknown version
            vec![1, 3, 0, 3, 0, 3, 0, 0], // a folder span leaves its parent
            vec![1, 0],                   // no root
        ] {
            assert!(Times::decode(&bad).is_err(), "{bad:?}");
        }
    }
}
