//! Moving objects between stores: the objects a tree reaches, transfer packs made from a list of
//! ids, and a received pack added to a store after every check (docs/objects.md, "Packs"; the
//! Layr Cloud API v1 client in layr `docs/remotes.md`).
//!
//! A transfer pack has the pack format of `crate::pack`: one zstd stream per pack, entries sorted
//! by id. Its entries are raw content only (form `r`): a receiver checks each entry against its id
//! without any other object, and a sender needs no delta base on the other side.

use crate::id::{Id, Kind};
use crate::pack::{self, Builder, Index};
use crate::store::Store;
use crate::tree::Node;
use anyhow::{bail, ensure, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

/// Every object that `tree` reaches, each once: the trees first (pre-order), then the blobs in
/// path order. Symbolic links live in their tree and reach nothing. Trees must be in the store;
/// blobs need not be.
pub fn reachable(store: &Store, tree: &Id) -> Result<Vec<(Id, Kind)>> {
    let mut seen = HashSet::new();
    let (mut trees, mut blobs) = (Vec::new(), Vec::new());
    walk(store, tree, &mut seen, &mut trees, &mut blobs)?;
    trees.extend(blobs);
    Ok(trees)
}

fn walk(store: &Store, id: &Id, seen: &mut HashSet<Id>, trees: &mut Vec<(Id, Kind)>, blobs: &mut Vec<(Id, Kind)>) -> Result<()> {
    if !seen.insert(*id) {
        return Ok(());
    }
    trees.push((*id, Kind::Tree));
    for entry in store.get_tree(id)?.entries {
        match entry.node {
            Node::Dir { tree } => walk(store, &tree, seen, trees, blobs)?,
            Node::File { blob, .. } => {
                if seen.insert(blob) {
                    blobs.push((blob, Kind::Blob));
                }
            }
            Node::Symlink { .. } => {}
        }
    }
    Ok(())
}

/// The trees that `tree` reaches and the store lacks, as far as the store's trees go: a tree that
/// is missing is named, its subtrees are not (they come with it).
pub fn missing_trees(store: &Store, tree: &Id) -> Result<Vec<Id>> {
    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    let mut stack = vec![*tree];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if !store.has(&id) {
            missing.push(id);
            continue;
        }
        for entry in store.get_tree(&id)?.entries {
            if let Node::Dir { tree } = entry.node {
                stack.push(tree);
            }
        }
    }
    Ok(missing)
}

/// A transfer pack in memory: its id (the `opaque` id of `data`), the data file and the index file.
pub struct BuiltPack {
    pub id: Id,
    pub data: Vec<u8>,
    pub index: Vec<u8>,
    pub objects: usize,
}

/// The raw content of an object of the store, checked against its id.
fn content(store: &Store, id: &Id, kind: Kind) -> Result<Vec<u8>> {
    Ok(match kind {
        Kind::Tree => store.get_tree(id)?.encode(),
        Kind::Times => store.get_times(id)?.encode(),
        Kind::Blob => store.read_blob(id)?,
        other => bail!("a transfer pack takes trees, times tables and blobs, not a {} ({id})", other.name()),
    })
}

/// Transfer packs of the objects `ids`, in their order, of about `Options::pack_plain_max` plain
/// bytes each (a compressed pack stays under the 64 MiB single upload of Layr Cloud; one object
/// larger than that makes a pack of its own). The store does not change.
pub fn build_packs(store: &Store, ids: &[(Id, Kind)]) -> Result<Vec<BuiltPack>> {
    let codec = crate::zstdx::Codec { level: store.options.pack_level, long: store.options.pack_long, dict: None };
    let mut packs = Vec::new();
    let mut builder = Builder::new(store.options.block_size);
    let mut seen = HashSet::new();
    for (id, kind) in ids {
        if !seen.insert(*id) {
            continue;
        }
        let bytes = content(store, id, *kind)?;
        if !builder.entries.is_empty() && builder.plain_len() + bytes.len() > store.options.pack_plain_max {
            packs.push(finish(store, std::mem::replace(&mut builder, Builder::new(store.options.block_size)), &codec)?);
        }
        builder.add(*id, *kind, pack::RAW, &bytes);
    }
    if !builder.entries.is_empty() {
        packs.push(finish(store, builder, &codec)?);
    }
    Ok(packs)
}

fn finish(store: &Store, builder: Builder, codec: &crate::zstdx::Codec) -> Result<BuiltPack> {
    let Builder { plain, mut entries, .. } = builder;
    let mut data = pack::DATA_MAGIC.to_vec();
    let mut blocks = Vec::with_capacity(plain.len());
    for block in &plain {
        let frame = crate::zstdx::compress(block, codec)?;
        ensure!(crate::zstdx::decompress(&frame, None, block.len())? == *block, "a pack block does not decode to its input");
        blocks.push(pack::Block { offset: data.len() as u64, compressed: frame.len() as u32, plain: block.len() as u32 });
        data.extend_from_slice(&frame);
    }
    let id = store.keying().id(Kind::Opaque, &data);
    entries.sort_by_key(|e| e.id);
    let objects = entries.len();
    let index = Index { pack: id, blocks, entries }.encode();
    Ok(BuiltPack { id, data, index, objects })
}

/// Add a received pack to the store after every check: the index decodes and fits the data, the
/// data has the pack id, every block decodes, and every entry is raw content that matches its id.
/// Returns the pack id. A pack that the store holds already is left as it is. The store reads the
/// new pack at its next lookup that misses (or open it again).
pub fn add_pack(store: &Store, data: &[u8], index: &[u8]) -> Result<Id> {
    let decoded = Index::decode(index, data.len() as u64)?;
    let id = store.keying().id(Kind::Opaque, data);
    ensure!(decoded.pack == id, "the pack data has id {id}, its index names {}", decoded.pack);
    let mut plain = Vec::with_capacity(decoded.blocks.len());
    for (n, block) in decoded.blocks.iter().enumerate() {
        let start = block.offset as usize;
        let frame = &data[start..start + block.compressed as usize];
        plain.push(
            crate::zstdx::decompress(frame, None, block.plain as usize).with_context(|| format!("block {n} of pack {id}"))?,
        );
    }
    for entry in &decoded.entries {
        ensure!(entry.form == pack::RAW, "pack {id}: entry {} is not raw content", entry.id);
        let bytes = &plain[entry.block as usize][entry.offset as usize..(entry.offset + entry.length) as usize];
        ensure!(store.keying().id(entry.kind, bytes) == entry.id, "pack {id}: entry {} does not match its id", entry.id);
    }
    let dir = store.root().join("packs");
    for (bytes, name) in [(data, format!("{id}.pack")), (index, format!("{id}.idx"))] {
        let tmp = store.root().join("tmp").join(format!("recv-{}-{name}", std::process::id()));
        let written = (|| -> Result<()> {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        // A link never replaces: an existing pack of this id holds the same bytes.
        let linked = fs::hard_link(&tmp, dir.join(&name));
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot store pack file {name}")),
        }
    }
    fs::File::open(&dir)?.sync_all()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Keying;
    use crate::store::Temperature;
    use crate::testutil::{noise, scratch};
    use crate::write::{write_tree, WriteOptions};

    fn options(temperature: Temperature) -> WriteOptions {
        WriteOptions { temperature, previous: None, exclude: Vec::new(), similar: false }
    }

    fn sample(dir: &std::path::Path) {
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("one"), "first file\n").unwrap();
        fs::write(dir.join("a/two"), "second file\n").unwrap();
        fs::write(dir.join("a/b/three"), noise(200_000, 3)).unwrap();
        fs::write(dir.join("a/b/same"), "first file\n").unwrap();
        std::os::unix::fs::symlink("one", dir.join("link")).unwrap();
    }

    #[test]
    fn a_transfer_pack_moves_a_tree_to_another_store_and_a_checkout_matches() {
        let s = scratch("transfer");
        let src = s.join("src");
        sample(&src);
        let a = Store::open(&s.join("a"), Keying::Plain).unwrap();
        let w = write_tree(&a, &src, &options(Temperature::Hot)).unwrap();
        let (tree, times) = (w.tree.unwrap(), w.times.unwrap());
        let ids = reachable(&a, &tree).unwrap();
        // Four trees (root, a, a/b and none for the link), three distinct blobs: "same" equals "one".
        assert_eq!(ids.iter().filter(|(_, k)| *k == Kind::Tree).count(), 3);
        assert_eq!(ids.iter().filter(|(_, k)| *k == Kind::Blob).count(), 3);
        assert_eq!(ids[0], (tree, Kind::Tree), "the root tree comes first");

        let b = Store::open(&s.join("b"), Keying::Plain).unwrap();
        assert_eq!(missing_trees(&b, &tree).unwrap(), vec![tree]);
        let mut all = ids.clone();
        all.push((times, Kind::Times));
        let packs = build_packs(&a, &all).unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].objects, 7);
        for p in &packs {
            assert_eq!(add_pack(&b, &p.data, &p.index).unwrap(), p.id);
            // Again: the same pack is left as it is.
            assert_eq!(add_pack(&b, &p.data, &p.index).unwrap(), p.id);
        }
        let b = Store::open(&s.join("b"), Keying::Plain).unwrap();
        assert!(missing_trees(&b, &tree).unwrap().is_empty());
        let dst = s.join("dst");
        crate::checkout_with(&b, &tree, Some(&times), &dst).unwrap();
        assert!(crate::compare(&src, &dst).unwrap().differences.is_empty());
        assert_eq!(fs::read(dst.join("a/b/three")).unwrap(), noise(200_000, 3));
    }

    #[test]
    fn packs_split_at_the_plain_limit_and_bad_packs_are_refused() {
        let s = scratch("transfer-bad");
        let src = s.join("src");
        sample(&src);
        let mut a = Store::open(&s.join("a"), Keying::Plain).unwrap();
        let w = write_tree(&a, &src, &options(Temperature::Cold)).unwrap();
        let ids = reachable(&a, &w.tree.unwrap()).unwrap();
        a.options.pack_plain_max = 100_000;
        let packs = build_packs(&a, &ids).unwrap();
        assert!(packs.len() >= 2, "the 200 kB blob makes a pack of its own");
        assert_eq!(packs.iter().map(|p| p.objects).sum::<usize>(), ids.len());

        let b = Store::open(&s.join("b"), Keying::Plain).unwrap();
        let p = &packs[0];
        // A changed data byte: the id no longer matches the index.
        let mut data = p.data.clone();
        let last = data.len() - 1;
        data[last] ^= 1;
        assert!(add_pack(&b, &data, &p.index).is_err());
        // An index of another pack.
        assert!(add_pack(&b, &p.data, &packs[1].index).is_err());
        // A valid pack whose entry does not match its id: rebuild one with a wrong id.
        let mut builder = Builder::new(1 << 20);
        builder.add(Id([7; 32]), Kind::Blob, pack::RAW, b"not the content of id 0707");
        let codec = crate::zstdx::Codec { level: 3, long: false, dict: None };
        let forged = finish(&b, builder, &codec).unwrap();
        let error = add_pack(&b, &forged.data, &forged.index).unwrap_err().to_string();
        assert!(error.contains("does not match its id"), "{error}");
        assert!(fs::read_dir(b.root().join("packs")).unwrap().next().is_none(), "nothing was stored");
        // Only trees, times and blobs go into a transfer pack.
        assert!(build_packs(&a, &[(Id([1; 32]), Kind::Chunk)]).is_err());
    }
}
