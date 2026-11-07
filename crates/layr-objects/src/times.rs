//! The times table of a folder (docs/objects.md, "Times"): the modification times of the folder
//! and its entries, outside the tree, so equal folders keep equal tree ids and a checkout still
//! gets the times that build tools compare.
//!
//! ```text
//! times  = version:u8 (1)  own:zigzag-varint  count:varint  record*count
//! record = 0x00 delta:zigzag-varint     a file or a symbolic link
//!        | 0x01 id:32                   a folder: the id of its own times table
//! ```
//!
//! Records follow the entries of the folder's tree, sorted by name bytes. `own` is the time of the
//! folder in nanoseconds since the Unix epoch. `delta` is the time of the entry minus the previous
//! file time of this table (the first: minus `own`). A table per folder, like the trees: a save
//! writes the tables on the way to a change and shares all the others, whatever the tree's size.

use crate::id::Id;
use crate::tree::{varint_into, Reader};
use anyhow::{bail, ensure, Result};

const VERSION: u8 = 1;
const TIME: u8 = 0;
const FOLDER: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// The time of a file or a symbolic link, in nanoseconds since the epoch.
    Time(i64),
    /// The times table of a folder.
    Folder(Id),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Times {
    /// The time of this folder.
    pub own: i64,
    /// One record per entry of the folder's tree, in the tree's order.
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
        varint_into(&mut out, zigzag(self.own));
        varint_into(&mut out, self.records.len() as u64);
        let mut prev = self.own;
        for r in &self.records {
            match r {
                Record::Time(ns) => {
                    out.push(TIME);
                    varint_into(&mut out, zigzag(ns.wrapping_sub(prev)));
                    prev = *ns;
                }
                Record::Folder(id) => {
                    out.push(FOLDER);
                    out.extend_from_slice(&id.0);
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Times> {
        let mut r = Reader::new(bytes);
        ensure!(r.byte()? == VERSION, "unknown times format version");
        let own = unzigzag(r.varint()?);
        let n = r.varint()?;
        ensure!(n <= bytes.len() as u64, "a times table has a wrong count");
        let mut records = Vec::with_capacity(n as usize);
        let mut prev = own;
        for _ in 0..n {
            match r.byte()? {
                TIME => {
                    prev = prev.wrapping_add(unzigzag(r.varint()?));
                    records.push(Record::Time(prev));
                }
                FOLDER => records.push(Record::Folder(r.id()?)),
                other => bail!("unknown times record {other}"),
            }
        }
        if !r.done() {
            bail!("bytes after the times table");
        }
        Ok(Times { own, records })
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

    #[test]
    fn times_round_trip_compactly() {
        let base = 1_794_000_000_123_456_789i64;
        let child = Id([7; 32]);
        let t = Times {
            own: base,
            records: vec![
                Record::Time(base),
                Record::Folder(child),
                Record::Time(-1),
                Record::Time(i64::MAX),
                Record::Time(i64::MIN),
                Record::Time(0),
            ],
        };
        assert_eq!(Times::decode(&t.encode()).unwrap(), t);
        let same = Times { own: base, records: vec![Record::Time(base); 999] };
        assert!(same.encode().len() < 2000 + 16, "equal times cost two bytes each");
        // The encoding is fixed: version, own, count, then a tag and a delta or an id per record.
        assert_eq!(
            Times { own: 1, records: vec![Record::Time(0), Record::Time(3)] }.encode(),
            vec![1, 2, 2, 0, 1, 0, 6]
        );
        let mut with_id = vec![1, 0, 1, 1];
        with_id.extend_from_slice(&child.0);
        assert_eq!(Times { own: 0, records: vec![Record::Folder(child)] }.encode(), with_id);
        for bad in [
            vec![1, 0, 1],          // a record is missing
            vec![1, 0, 1, 0],       // cut short
            vec![1, 0, 0, 0],       // bytes after the end
            vec![2, 0, 0],          // unknown version
            vec![1, 0, 1, 9, 0],    // unknown record
            vec![1, 0, 1, 1, 7, 7], // a cut id
        ] {
            assert!(Times::decode(&bad).is_err(), "{bad:?}");
        }
    }
}
