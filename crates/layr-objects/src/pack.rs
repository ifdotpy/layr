//! Packs: many objects in one data file, in zstd blocks, with a separate index
//! (docs/objects.md, "Packs").
//!
//! ```text
//! <id>.pack   "LPK1"  block*                       block = one zstd frame of entry payloads
//! <id>.idx    "LPI1"  pack-id:32
//!             blocks:u32  (offset:u64 compressed:u32 plain:u32)*
//!             entries:u32 (id:32 kind:u8 form:u8 block:u32 offset:u32 length:u32)*   sorted by id
//!             sha256:32   of every byte before it
//! ```
//!
//! Integers are little-endian. The pack id is the `opaque` id of the whole data file. An entry
//! payload is the decoded object for form `r` (raw: a tree, a chunk, a whole blob), the delta body
//! for `d` (depth, base id, zstd frame), and the chunk list for `c`.

use crate::id::{Id, Kind};
use anyhow::{ensure, Result};
use layr_hashers::{ContentAddress, Digest};

pub const DATA_MAGIC: &[u8; 4] = b"LPK1";
const INDEX_MAGIC: &[u8; 4] = b"LPI1";
/// Raw payload: the decoded content of a tree, a chunk or a whole blob.
pub const RAW: u8 = b'r';
pub const DELTA: u8 = b'd';
pub const CHUNKED: u8 = b'c';
const ENTRY_LEN: usize = 32 + 1 + 1 + 4 + 4 + 4;
const BLOCK_LEN: usize = 8 + 4 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub offset: u64,
    pub compressed: u32,
    pub plain: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: Id,
    pub kind: Kind,
    pub form: u8,
    pub block: u32,
    pub offset: u32,
    pub length: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub pack: Id,
    pub blocks: Vec<Block>,
    /// Sorted by id, unique.
    pub entries: Vec<Entry>,
}

impl Index {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 32 + 8 + self.blocks.len() * BLOCK_LEN + self.entries.len() * ENTRY_LEN + 32);
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&self.pack.0);
        out.extend_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        for b in &self.blocks {
            out.extend_from_slice(&b.offset.to_le_bytes());
            out.extend_from_slice(&b.compressed.to_le_bytes());
            out.extend_from_slice(&b.plain.to_le_bytes());
        }
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(&e.id.0);
            out.push(e.kind.byte());
            out.push(e.form);
            out.extend_from_slice(&e.block.to_le_bytes());
            out.extend_from_slice(&e.offset.to_le_bytes());
            out.extend_from_slice(&e.length.to_le_bytes());
        }
        let sum: [u8; 32] = ContentAddress::digest(&out).into();
        out.extend_from_slice(&sum);
        out
    }

    /// Decode and check an index: its checksum, block layout without gaps after the magic, entry
    /// ranges inside their blocks, and sorted unique ids. `data_len` is the size of the data file.
    pub fn decode(bytes: &[u8], data_len: u64) -> Result<Index> {
        ensure!(bytes.len() >= 4 + 32 + 4 + 4 + 32, "a pack index is too short");
        let (body, sum) = bytes.split_at(bytes.len() - 32);
        ensure!(<[u8; 32]>::from(ContentAddress::digest(body)) == sum, "a pack index does not match its checksum");
        ensure!(&body[..4] == INDEX_MAGIC, "not a pack index");
        let pack = Id(body[4..36].try_into().expect("32 bytes"));
        let u32_at = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().expect("4 bytes"));
        let nblocks = u32_at(36) as usize;
        let mut at = 40;
        ensure!(body.len() >= at + nblocks * BLOCK_LEN + 4, "a pack index is cut short");
        let mut blocks = Vec::with_capacity(nblocks);
        let mut end = DATA_MAGIC.len() as u64;
        for _ in 0..nblocks {
            let offset = u64::from_le_bytes(body[at..at + 8].try_into().expect("8 bytes"));
            let block = Block { offset, compressed: u32_at(at + 8), plain: u32_at(at + 12) };
            ensure!(block.offset == end && block.compressed > 0, "pack blocks are not contiguous");
            end += u64::from(block.compressed);
            blocks.push(block);
            at += BLOCK_LEN;
        }
        ensure!(end == data_len, "the pack data has {data_len} bytes, its index says {end}");
        let nentries = u32_at(at) as usize;
        at += 4;
        ensure!(body.len() == at + nentries * ENTRY_LEN, "a pack index has the wrong length");
        let mut entries: Vec<Entry> = Vec::with_capacity(nentries);
        for _ in 0..nentries {
            let e = Entry {
                id: Id(body[at..at + 32].try_into().expect("32 bytes")),
                kind: Kind::from_byte(body[at + 32])?,
                form: body[at + 33],
                block: u32_at(at + 34),
                offset: u32_at(at + 38),
                length: u32_at(at + 42),
            };
            ensure!(matches!(e.form, RAW | DELTA | CHUNKED), "unknown pack entry form");
            let block = blocks.get(e.block as usize).ok_or_else(|| anyhow::anyhow!("a pack entry names a missing block"))?;
            ensure!(u64::from(e.offset) + u64::from(e.length) <= u64::from(block.plain), "a pack entry leaves its block");
            if let Some(last) = entries.last() {
                ensure!(last.id < e.id, "pack entries are not sorted or not unique");
            }
            entries.push(e);
            at += ENTRY_LEN;
        }
        Ok(Index { pack, blocks, entries })
    }

    pub fn find(&self, id: &Id) -> Option<&Entry> {
        self.entries.binary_search_by(|e| e.id.cmp(id)).ok().map(|i| &self.entries[i])
    }
}

/// Collects payloads into blocks of about `block_size` plain bytes.
pub struct Builder {
    block_size: usize,
    pub plain: Vec<Vec<u8>>,
    pub entries: Vec<Entry>,
}

impl Builder {
    pub fn new(block_size: usize) -> Self {
        Builder { block_size, plain: Vec::new(), entries: Vec::new() }
    }

    pub fn add(&mut self, id: Id, kind: Kind, form: u8, payload: &[u8]) {
        let start_new = match self.plain.last() {
            None => true,
            Some(b) => !b.is_empty() && b.len() + payload.len() > self.block_size,
        };
        if start_new {
            self.plain.push(Vec::with_capacity(self.block_size.max(payload.len())));
        }
        let block = self.plain.len() - 1;
        let buf = &mut self.plain[block];
        self.entries.push(Entry { id, kind, form, block: block as u32, offset: buf.len() as u32, length: payload.len() as u32 });
        buf.extend_from_slice(payload);
    }

    pub fn plain_len(&self) -> usize {
        self.plain.iter().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut entries = vec![
            Entry { id: Id([3; 32]), kind: Kind::Blob, form: RAW, block: 0, offset: 0, length: 10 },
            Entry { id: Id([1; 32]), kind: Kind::Tree, form: RAW, block: 1, offset: 5, length: 5 },
        ];
        entries.sort_by_key(|e| e.id);
        Index {
            pack: Id([9; 32]),
            blocks: vec![Block { offset: 4, compressed: 7, plain: 10 }, Block { offset: 11, compressed: 3, plain: 10 }],
            entries,
        }
    }

    #[test]
    fn an_index_round_trips_and_checks_itself() {
        let index = sample();
        let bytes = index.encode();
        assert_eq!(Index::decode(&bytes, 14).unwrap(), index);
        assert_eq!(index.find(&Id([3; 32])).unwrap().length, 10);
        assert!(index.find(&Id([2; 32])).is_none());
        assert!(Index::decode(&bytes, 15).is_err(), "the data file size must match");
        for at in [0, 10, bytes.len() - 40, bytes.len() - 1] {
            let mut bad = bytes.clone();
            bad[at] ^= 1;
            assert!(Index::decode(&bad, 14).is_err(), "a changed byte at {at}");
        }
        let mut leaves = sample();
        leaves.entries[0].length = 11;
        assert!(Index::decode(&leaves.encode(), 14).is_err());
        let mut unsorted = sample();
        unsorted.entries.swap(0, 1);
        assert!(Index::decode(&unsorted.encode(), 14).is_err());
        let mut gap = sample();
        gap.blocks[1].offset = 12;
        assert!(Index::decode(&gap.encode(), 15).is_err());
    }

    #[test]
    fn the_builder_fills_blocks() {
        let mut b = Builder::new(10);
        b.add(Id([1; 32]), Kind::Blob, RAW, b"123456");
        b.add(Id([2; 32]), Kind::Blob, RAW, b"7890");
        b.add(Id([3; 32]), Kind::Blob, RAW, b"x");
        b.add(Id([4; 32]), Kind::Blob, RAW, &[0; 25]);
        assert_eq!(b.plain.iter().map(Vec::len).collect::<Vec<_>>(), vec![10, 1, 25]);
        assert_eq!((b.entries[2].block, b.entries[2].offset), (1, 0));
        assert_eq!(b.plain_len(), 36);
    }
}
