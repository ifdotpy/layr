//! Moving objects between stores: the objects a tree reaches, transfer packs made from a list of
//! ids, and a received pack added to a store after every check (docs/objects.md, "Packs"; the
//! Layr Cloud API v1 client in layr `docs/remotes.md`).
//!
//! A transfer pack has the pack format of `crate::pack`: zstd blocks with entries sorted by id.
//! Its entries are raw content only (form `r`): a receiver checks each entry against its id
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

/// The times tables reached by a folder times table, root first.
pub fn reachable_times(store: &Store, root: &Id) -> Result<Vec<Id>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut stack = vec![*root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        result.push(id);
        for record in store.get_times(&id)?.records.into_iter().rev() {
            if let crate::times::Record::Folder(child) = record {
                stack.push(child);
            }
        }
    }
    Ok(result)
}

/// Missing folder times tables, stopping at a table the store does not have yet.
pub fn missing_times(store: &Store, root: &Id) -> Result<Vec<Id>> {
    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    let mut stack = vec![*root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if !store.has(&id) {
            missing.push(id);
            continue;
        }
        for record in store.get_times(&id)?.records {
            if let crate::times::Record::Folder(child) = record {
                stack.push(child);
            }
        }
    }
    Ok(missing)
}

/// Missing tables for a Cloud state. Cloud version 1 is a single whole-tree table.
pub fn missing_cloud_times(store: &Store, root: &Id) -> Result<Vec<Id>> {
    if !store.has(root) {
        return Ok(vec![*root]);
    }
    let bytes = store.times_bytes(root)?;
    if bytes.first() == Some(&1) {
        crate::times::decode_cloud_v1(&bytes)?;
        return Ok(Vec::new());
    }
    missing_times(store, root)
}

/// Translate a Cloud whole-tree times table to local folder tables after checking the tree shape.
/// No state reference is changed. A v2 table is returned unchanged.
pub fn cloud_times_to_folder(store: &Store, tree: &Id, root: &Id) -> Result<Id> {
    use crate::times::{Record, Times};
    let bytes = store.times_bytes(root)?;
    if bytes.first() != Some(&1) {
        Times::decode(&bytes)?;
        return Ok(*root);
    }
    let records = crate::times::decode_cloud_v1(&bytes)?;
    struct Frame {
        entries: std::vec::IntoIter<crate::tree::Entry>,
        records: Vec<Record>,
        own: i64,
        start: usize,
        span: u64,
    }
    fn frame(store: &Store, id: &Id, records: &[(i64, u64)], at: &mut usize) -> Result<Frame> {
        let (own, span) = *records.get(*at).context("Cloud times table is too short")?;
        ensure!(span != 0, "folder has a file times record");
        let start = *at;
        *at += 1;
        Ok(Frame { entries: store.get_tree(id)?.entries.into_iter(), records: Vec::new(), own, start, span })
    }
    let mut at = 0;
    let mut stack = vec![frame(store, tree, &records, &mut at)?];
    let mut pending = Vec::new();
    let result = loop {
        let current = stack.last_mut().expect("a root frame");
        if let Some(entry) = current.entries.next() {
            match entry.node {
                Node::Dir { tree } => stack.push(frame(store, &tree, &records, &mut at)?),
                Node::File { .. } | Node::Symlink { .. } => {
                    let (time, span) = *records.get(at).context("Cloud times table is too short")?;
                    ensure!(span == 0, "file has a folder times record");
                    at += 1;
                    current.records.push(Record::Time(time));
                }
            }
            continue;
        }
        let done = stack.pop().expect("a folder frame");
        ensure!((at - done.start) as u64 == done.span, "Cloud folder times span does not match the tree");
        let table = Times { own: done.own, records: done.records };
        let id = store.keying().id(Kind::Times, &table.encode());
        pending.push((id, table));
        if let Some(parent) = stack.last_mut() {
            parent.records.push(Record::Folder(id));
        } else {
            break id;
        }
    };
    ensure!(at == records.len(), "Cloud times table has extra records");
    for (id, table) in pending {
        ensure!(store.put_times(&table)? == id, "folder times id changed");
    }
    Ok(result)
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
        Kind::Times => store.times_bytes(id)?,
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
    add_pack_with_source(store, data, index, false)
}

/// Accept version 1 whole-tree times only for a pack from the Layr Cloud v1 API.
pub fn add_cloud_pack(store: &Store, data: &[u8], index: &[u8]) -> Result<Id> {
    add_pack_with_source(store, data, index, true)
}

fn add_pack_with_source(store: &Store, data: &[u8], index: &[u8], cloud: bool) -> Result<Id> {
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
        match entry.kind {
            Kind::Tree => {
                crate::tree::Tree::decode(bytes).with_context(|| format!("pack {id}: invalid tree {}", entry.id))?;
            }
            Kind::Times => {
                if cloud && bytes.first() == Some(&1) {
                    crate::times::decode_cloud_v1(bytes)
                        .with_context(|| format!("pack {id}: invalid Cloud times {}", entry.id))?;
                } else {
                    crate::times::Times::decode(bytes).with_context(|| format!("pack {id}: invalid times {}", entry.id))?;
                }
            }
            Kind::Blob => {}
            other => bail!("pack {id}: unsupported transfer kind {}", other.name()),
        }
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
        assert_eq!(missing_times(&b, &times).unwrap(), vec![times]);
        let root_only = build_packs(&a, &[(times, Kind::Times)]).unwrap();
        add_pack(&b, &root_only[0].data, &root_only[0].index).unwrap();
        let b = Store::open(&s.join("b"), Keying::Plain).unwrap();
        assert_eq!(missing_times(&b, &times).unwrap().len(), 1, "the root names a missing child table");
        let all_times = a.times_ids(&times).unwrap();
        assert_eq!(all_times.len(), 3, "each folder has its own times table");
        let mut all = ids.clone();
        all.extend(all_times.into_iter().map(|id| (id, Kind::Times)));
        let packs = build_packs(&a, &all).unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].objects, all.len());
        for p in &packs {
            assert_eq!(add_pack(&b, &p.data, &p.index).unwrap(), p.id);
            // Again: the same pack is left as it is.
            assert_eq!(add_pack(&b, &p.data, &p.index).unwrap(), p.id);
        }
        let b = Store::open(&s.join("b"), Keying::Plain).unwrap();
        assert!(missing_trees(&b, &tree).unwrap().is_empty());
        assert!(missing_times(&b, &times).unwrap().is_empty());
        let dst = s.join("dst");
        crate::checkout_with(&b, &tree, Some(&times), &dst).unwrap();
        assert!(crate::compare(&src, &dst).unwrap().differences.is_empty());
        assert_eq!(fs::read(dst.join("a/b/three")).unwrap(), noise(200_000, 3));
    }

    #[test]
    fn an_ambiguous_legacy_times_entry_is_not_published() {
        let s = scratch("legacy-times-pack");
        let store = Store::open(&s.join("store"), Keying::Plain).unwrap();
        let bytes = [1, 0, 0];
        let id = store.keying().id(Kind::Times, &bytes);
        let mut builder = Builder::new(1 << 20);
        builder.add(id, Kind::Times, pack::RAW, &bytes);
        let codec = crate::zstdx::Codec { level: 3, long: false, dict: None };
        let pack = finish(&store, builder, &codec).unwrap();
        assert!(add_pack(&store, &pack.data, &pack.index).is_err());
        assert!(fs::read_dir(store.root().join("packs")).unwrap().next().is_none());
    }

    #[test]
    fn cloud_legacy_times_are_converted_without_guessing() {
        use crate::tree::{Entry, Node, Tree};
        let s = scratch("cloud-legacy");
        let store = Store::open(&s.join("store"), Keying::Plain).unwrap();
        let (blob, size) = store.put_cold_bytes(b"content", None).unwrap();
        let file = Entry { name: b"file".to_vec(), mode: 0o644, node: Node::File { size, blob }, xattrs: vec![] };
        let child = store.put_tree(&Tree { entries: vec![file], links: vec![] }).unwrap();
        let root = store
            .put_tree(&Tree {
                entries: vec![Entry { name: b"dir".to_vec(), mode: 0o755, node: Node::Dir { tree: child }, xattrs: vec![] }],
                links: vec![],
            })
            .unwrap();
        // Whole-tree v1: root 10 ns, folder 11 ns, file 13 ns.
        let bytes = [1, 3, 20, 3, 2, 2, 4, 0];
        let id = store.keying().id(Kind::Times, &bytes);
        let mut builder = Builder::new(1 << 20);
        builder.add(id, Kind::Times, pack::RAW, &bytes);
        let pack = finish(&store, builder, &crate::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
        assert!(add_pack(&store, &pack.data, &pack.index).is_err());
        add_cloud_pack(&store, &pack.data, &pack.index).unwrap();
        let store = Store::open(&s.join("store"), Keying::Plain).unwrap();
        assert!(missing_cloud_times(&store, &id).unwrap().is_empty());
        assert!(store.get_times(&id).is_err());
        let converted = cloud_times_to_folder(&store, &root, &id).unwrap();
        let top = store.get_times(&converted).unwrap();
        assert_eq!(top.own, 10);
        let crate::times::Record::Folder(nested) = top.records[0] else { panic!("missing folder table") };
        let nested = store.get_times(&nested).unwrap();
        assert_eq!(nested.own, 11);
        assert_eq!(nested.records, vec![crate::times::Record::Time(13)]);
        let out = s.join("out");
        crate::checkout_with(&store, &root, Some(&converted), &out).unwrap();
        assert_eq!(fs::read(out.join("dir/file")).unwrap(), b"content");
        assert_eq!(crate::times::ns_of(&fs::metadata(out.join("dir/file")).unwrap()), 13);
    }

    #[test]
    fn cloud_legacy_times_reject_a_wrong_tree_before_writing_tables() {
        use crate::tree::{Entry, Node, Tree};
        let s = scratch("cloud-legacy-invalid");
        let store = Store::open(&s.join("store"), Keying::Plain).unwrap();
        let root = store
            .put_tree(&Tree {
                entries: vec![Entry {
                    name: b"link".to_vec(),
                    mode: 0,
                    node: Node::Symlink { target: b"x".to_vec() },
                    xattrs: vec![],
                }],
                links: vec![],
            })
            .unwrap();
        // A valid whole-tree encoding, but the root span claims a missing record.
        let bytes = [1, 2, 0, 3, 0, 0];
        let id = store.keying().id(Kind::Times, &bytes);
        let mut builder = Builder::new(1 << 20);
        builder.add(id, Kind::Times, pack::RAW, &bytes);
        let pack = finish(&store, builder, &crate::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
        add_cloud_pack(&store, &pack.data, &pack.index).unwrap();
        let store = Store::open(&s.join("store"), Keying::Plain).unwrap();
        let would_write = crate::times::Times { own: 0, records: vec![crate::times::Record::Time(0)] };
        let would_write_id = store.keying().id(Kind::Times, &would_write.encode());
        assert!(!store.has(&would_write_id));
        assert!(cloud_times_to_folder(&store, &root, &id).is_err());
        assert!(!store.has(&would_write_id));
    }

    #[test]
    fn malformed_tree_and_times_entries_are_not_published() {
        let s = scratch("malformed-transfer");
        let store = Store::open(&s.join("store"), Keying::Plain).unwrap();
        let codec = crate::zstdx::Codec { level: 3, long: false, dict: None };
        for (kind, bytes) in [(Kind::Times, &[2, 0, 1][..]), (Kind::Tree, &[1, 1][..])] {
            let id = store.keying().id(kind, bytes);
            let mut builder = Builder::new(1 << 20);
            builder.add(id, kind, pack::RAW, bytes);
            let pack = finish(&store, builder, &codec).unwrap();
            assert!(add_pack(&store, &pack.data, &pack.index).is_err());
            assert!(fs::read_dir(store.root().join("packs")).unwrap().next().is_none());
        }
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
