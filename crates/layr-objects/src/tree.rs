//! The tree object: one folder, its entries sorted by name bytes, in a canonical binary form
//! (docs/objects.md, "Tree encoding"). A decoder refuses every form that the encoder does not make,
//! so one folder has exactly one encoding and one id.

use crate::id::Id;
use anyhow::{bail, ensure, Result};

/// The format version: the first byte of every tree.
const VERSION: u8 = 1;
/// The permission bits that a tree keeps (setuid, setgid, sticky and rwx for all).
pub const MODE_BITS: u32 = 0o7777;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A regular file: its size and the id of its blob.
    File { size: u64, blob: Id },
    /// A folder: the id of its tree. An empty folder names the empty tree.
    Dir { tree: Id },
    /// A symbolic link: its target bytes, kept in the tree (no blob).
    Symlink { target: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// One path component: not empty, not `.` or `..`, no `/` and no NUL. Any other bytes.
    pub name: Vec<u8>,
    /// `mode & 0o7777`; 0 for a symbolic link (its mode has no meaning on Linux).
    pub mode: u32,
    pub node: Node,
    /// Extended attributes, sorted by name (`crate::fsx::keep_xattr` selects them).
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<Entry>,
    /// Hard link groups: each group holds the paths (relative to this tree, sorted) of one file
    /// with more than one name in the tree. Only the root tree of a write holds groups, so the
    /// trees below it stay pure content and are shared between trees.
    pub links: Vec<Vec<Vec<u8>>>,
}

pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}

fn valid_path(path: &[u8]) -> bool {
    path.split(|b| *b == b'/').all(valid_name)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub(crate) fn byte(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or_else(|| anyhow::anyhow!("the object ends early"))?;
        self.pos += 1;
        Ok(b)
    }
    pub(crate) fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            ensure!(shift < 63 || b <= 1, "a varint is too large");
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                // One encoding per value: no trailing zero groups.
                ensure!(b != 0 || shift == 0, "a varint is not minimal");
                return Ok(value);
            }
        }
        bail!("a varint is too long")
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|end| *end <= self.data.len());
        let end = end.ok_or_else(|| anyhow::anyhow!("the object ends early"))?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = usize::try_from(self.varint()?)?;
        self.take(n)
    }
    pub(crate) fn id(&mut self) -> Result<Id> {
        Ok(Id(self.take(32)?.try_into().expect("32 bytes")))
    }
    pub(crate) fn done(&self) -> bool {
        self.pos == self.data.len()
    }
}

pub(crate) fn varint_into(out: &mut Vec<u8>, value: u64) {
    put_varint(out, value)
}

const FILE: u8 = 1;
const DIR: u8 = 2;
const SYMLINK: u8 = 3;

impl Tree {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        put_varint(&mut out, self.entries.len() as u64);
        for e in &self.entries {
            put_bytes(&mut out, &e.name);
            match &e.node {
                Node::File { size, blob } => {
                    out.push(FILE);
                    put_varint(&mut out, u64::from(e.mode));
                    put_varint(&mut out, *size);
                    out.extend_from_slice(&blob.0);
                }
                Node::Dir { tree } => {
                    out.push(DIR);
                    put_varint(&mut out, u64::from(e.mode));
                    out.extend_from_slice(&tree.0);
                }
                Node::Symlink { target } => {
                    out.push(SYMLINK);
                    put_varint(&mut out, u64::from(e.mode));
                    put_bytes(&mut out, target);
                }
            }
            put_varint(&mut out, e.xattrs.len() as u64);
            for (name, value) in &e.xattrs {
                put_bytes(&mut out, name);
                put_bytes(&mut out, value);
            }
        }
        put_varint(&mut out, self.links.len() as u64);
        for group in &self.links {
            put_varint(&mut out, group.len() as u64);
            for path in group {
                put_bytes(&mut out, path);
            }
        }
        out
    }

    /// Decode and check a tree: sorted unique names, valid modes, and only the canonical form.
    pub fn decode(data: &[u8]) -> Result<Tree> {
        let mut r = Reader::new(data);
        ensure!(r.byte()? == VERSION, "unknown tree format version");
        let count = r.varint()?;
        let mut entries: Vec<Entry> = Vec::new();
        for _ in 0..count {
            let name = r.bytes()?.to_vec();
            ensure!(valid_name(&name), "invalid entry name {:?}", String::from_utf8_lossy(&name));
            if let Some(last) = entries.last() {
                ensure!(last.name < name, "tree entries are not sorted or not unique");
            }
            let kind = r.byte()?;
            let mode = u32::try_from(r.varint()?)?;
            ensure!(mode & !MODE_BITS == 0, "invalid mode {mode:o}");
            let node = match kind {
                FILE => Node::File { size: r.varint()?, blob: r.id()? },
                DIR => Node::Dir { tree: r.id()? },
                SYMLINK => {
                    ensure!(mode == 0, "a symbolic link has mode 0 in a tree");
                    let target = r.bytes()?.to_vec();
                    ensure!(!target.is_empty() && !target.contains(&0), "invalid symbolic link target");
                    Node::Symlink { target }
                }
                _ => bail!("unknown entry type {kind}"),
            };
            let n = r.varint()?;
            let mut xattrs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            for _ in 0..n {
                let name = r.bytes()?.to_vec();
                ensure!(!name.is_empty() && !name.contains(&0), "invalid extended attribute name");
                if let Some((last, _)) = xattrs.last() {
                    ensure!(*last < name, "extended attributes are not sorted or not unique");
                }
                xattrs.push((name, r.bytes()?.to_vec()));
            }
            entries.push(Entry { name, mode, node, xattrs });
        }
        let groups = r.varint()?;
        let mut links: Vec<Vec<Vec<u8>>> = Vec::new();
        for _ in 0..groups {
            let n = r.varint()?;
            ensure!(n >= 2, "a hard link group has fewer than two paths");
            let mut group: Vec<Vec<u8>> = Vec::new();
            for _ in 0..n {
                let path = r.bytes()?.to_vec();
                ensure!(valid_path(&path), "invalid hard link path");
                if let Some(last) = group.last() {
                    ensure!(*last < path, "hard link paths are not sorted or not unique");
                }
                group.push(path);
            }
            if let Some(last) = links.last() {
                ensure!(last[0] < group[0], "hard link groups are not sorted");
            }
            links.push(group);
        }
        ensure!(r.done(), "bytes after the tree");
        let tree = Tree { entries, links };
        // Every check above makes the encoding canonical; this one keeps it so if a check is missed.
        ensure!(tree.encode() == data, "noncanonical tree encoding");
        Ok(tree)
    }

    /// Sort entries and groups into the canonical order.
    pub fn normalize(&mut self) {
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in &mut self.entries {
            e.xattrs.sort();
        }
        for g in &mut self.links {
            g.sort();
        }
        self.links.retain(|g| g.len() >= 2);
        self.links.sort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Keying, Kind};

    fn sample() -> Tree {
        let blob = Keying::Plain.id(Kind::Blob, b"hello\n");
        let empty = Keying::Plain.id(Kind::Tree, &Tree::default().encode());
        let mut t = Tree {
            entries: vec![
                Entry { name: b"run.sh".to_vec(), mode: 0o755, node: Node::File { size: 6, blob }, xattrs: vec![] },
                Entry { name: b"empty".to_vec(), mode: 0o700, node: Node::Dir { tree: empty }, xattrs: vec![] },
                Entry {
                    name: b"link".to_vec(),
                    mode: 0,
                    node: Node::Symlink { target: b"../outside/\xff".to_vec() },
                    xattrs: vec![],
                },
                Entry {
                    name: b"caf\xe9".to_vec(),
                    mode: 0o4644,
                    node: Node::File { size: 6, blob },
                    xattrs: vec![(b"user.b".to_vec(), vec![]), (b"user.a".to_vec(), b"\0v".to_vec())],
                },
            ],
            links: vec![vec![b"run.sh".to_vec(), b"caf\xe9".to_vec()]],
        };
        t.normalize();
        t
    }

    #[test]
    fn a_tree_round_trips() {
        let t = sample();
        let bytes = t.encode();
        assert_eq!(Tree::decode(&bytes).unwrap(), t);
        assert_eq!(Tree::decode(&Tree::default().encode()).unwrap(), Tree::default());
    }

    #[test]
    fn the_empty_tree_is_fixed() {
        // A change here changes every tree id.
        assert_eq!(Tree::default().encode(), vec![1, 0, 0]);
    }

    #[test]
    fn noncanonical_trees_are_refused() {
        let good = sample();
        let mut unsorted = good.clone();
        unsorted.entries.swap(0, 1);
        let mut duplicate = good.clone();
        duplicate.entries[1].name = duplicate.entries[0].name.clone();
        let mut slash = good.clone();
        slash.entries[0].name = b"a/b".to_vec();
        let mut dot = good.clone();
        dot.entries[0].name = b"..".to_vec();
        let mut mode = good.clone();
        mode.entries[0].mode = 0o100644;
        let mut lonely = good.clone();
        lonely.links = vec![vec![b"run.sh".to_vec()]];
        let mut bad_link = good.clone();
        bad_link.links = vec![vec![b"a//b".to_vec(), b"c".to_vec()]];
        for bad in [unsorted, duplicate, slash, dot, mode, lonely, bad_link] {
            assert!(Tree::decode(&bad.encode()).is_err(), "{bad:?}");
        }
        let mut trailing = good.encode();
        trailing.push(0);
        assert!(Tree::decode(&trailing).is_err());
        // A varint with a redundant zero group decodes to the same number but is not canonical.
        let mut long = vec![1, 0x80, 0x00, 0];
        assert!(Tree::decode(&long).is_err());
        long = vec![2, 0, 0];
        assert!(Tree::decode(&long).is_err());
        for cut in 0..good.encode().len() {
            assert!(Tree::decode(&good.encode()[..cut]).is_err());
        }
    }
}
